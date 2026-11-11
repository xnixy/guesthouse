//! The whole flow, as the app drives it: a machine is started by a real task, runs, and is stopped.
//!
//! This program also stands in for QEMU and the daemon. The task runs copies of it under their names,
//! and a copy only answers as they would, so no VM runs. Set `GUESTHOUSE_TEST_DAEMON` to a build of
//! `guesthouse-daemon` with the `stand-in` feature to go through the real daemon instead.

use guesthouse_app_core::actions::run_task;
use guesthouse_app_core::agent::make_token;
use guesthouse_app_core::context::Context;
use guesthouse_app_core::machine::{Kind, Machine};
use guesthouse_app_core::processes::qemu_pid;
use guesthouse_app_core::settings::{driver_folder, write_setup};
use guesthouse_app_core::tasks::{State, Status, launch, status};
use guesthouse_app_core::text::base64_decode;
use guesthouse_app_core::vm_args::{PASST_PID_FILE, passt_installed};
use guesthouse_app_core::vm_host::LocalHost;
use guesthouse_daemon::handover::Part;
use guesthouse_daemon::slot::Slot;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command};
use std::thread;
use std::time::{Duration, Instant};

const INNER_VARIABLE: &str = "FLOW_TEST_INNER";
const REAL_DAEMON_VARIABLE: &str = "GUESTHOUSE_TEST_DAEMON";
/// Long enough for the task's own waits for a desktop.
const MOST_WAIT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(200);
const PRETEND_LIFE: Duration = Duration::from_secs(120);
/// The first forward on QEMU's user network is the agent's.
const AGENT_FORWARD: &str = "hostfwd=tcp:127.0.0.1:";
/// How the guest's memfds show in `/proc`.
const GUEST_MEMORY: &str = "/memfd:gpupv-";

fn serve_monitor(mut stream: UnixStream) {
    let _ = stream.write_all(b"{\"QMP\": {}}\n");
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines().map_while(Result::ok) {
        let request: Value = serde_json::from_str(&line).unwrap_or_default();
        let answer = match request["execute"].as_str() {
            Some("query-status") => json!({"return": {"status": "running"}}),
            Some("system_powerdown") => process::exit(0),
            _ => json!({"return": {}}),
        };
        let _ = writeln!(stream, "{answer}");
    }
}

fn listen_on(socket: &str) {
    // QEMU replaces a socket an earlier run left.
    let _ = fs::remove_file(socket);
    serve_monitors(UnixListener::bind(socket).unwrap());
}

/// A monitor on a socket the jail's launcher opened, as a QEMU with no sound gets them.
fn listen_on_descriptor(chardev: &str) {
    use std::os::fd::FromRawFd;
    let fd = chardev.split(',').find_map(|part| part.strip_prefix("fd=")).unwrap().parse().unwrap();
    // SAFETY: the launcher left this socket open for QEMU, and nothing else here holds it.
    serve_monitors(unsafe { UnixListener::from_raw_fd(fd) });
}

fn serve_monitors(listener: UnixListener) {
    thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            thread::spawn(move || serve_monitor(stream));
        }
    });
}

/// What the guest's agent says, as far as a start needs it.
fn agent_answer(script: &str) -> &'static str {
    if script.contains("Stop-Computer") {
        process::exit(0);
    }
    if script.contains("Win32_VideoController") {
        "gpu working 1\r\n"
    } else if script.contains("Get-Process") {
        "3\r\n"
    } else if script.contains("Get-Content") {
        "screen 1920x1080\r\n"
    } else {
        "up\r\n"
    }
}

fn serve_agent(port: u16) {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    thread::spawn(move || {
        for mut stream in listener.incoming().map_while(Result::ok) {
            let mut line = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
            let encoded = line.split_whitespace().nth(1).unwrap_or_default();
            let script = base64_decode(encoded).unwrap_or_default();
            let _ = stream.write_all(agent_answer(&String::from_utf8_lossy(&script)).as_bytes());
        }
    });
}

fn pretend_qemu(arguments: &[String]) {
    // Asked what the render device takes, the real one lists its properties and exits.
    if arguments.iter().any(|word| word == "gpupv-render,help") {
        println!("gpupv-render options:\n  vendor-id=<uint16>\n  device-id=<uint16>");
        return;
    }
    // Asked whether it has a seccomp filter of its own, as a build with one does.
    if arguments.iter().any(|word| word == "-version") {
        println!("QEMU emulator version 0");
        return;
    }
    for pair in arguments.windows(2) {
        if pair[0] == "-chardev" && pair[1].starts_with("socket,id=monitor") {
            listen_on_descriptor(&pair[1]);
        }
        if pair[0] == "-qmp" {
            let path = pair[1].strip_prefix("unix:").unwrap().split(',').next().unwrap();
            listen_on(path);
        }
        if pair[0] == "-nic" && pair[1].starts_with("socket,") {
            // passt, started by the jail's launcher, is on the other end.
            let fd = pair[1].split(',').find_map(|part| part.strip_prefix("fd=")).unwrap();
            let target = fs::read_link(format!("/proc/self/fd/{fd}")).unwrap_or_default();
            assert!(target.to_string_lossy().starts_with("socket:"), "passt's end is a socket: {target:?}");
        }
        if pair[0] == "-object" && pair[1].starts_with("memory-backend-file,") {
            open_guest_memory(&pair[1]);
        }
        if (pair[0] == "-nic" || pair[0] == "-netdev") && pair[1].contains(AGENT_FORWARD) {
            let port = pair[1].split(AGENT_FORWARD).nth(1).unwrap().split('-').next().unwrap();
            serve_agent(port.parse().unwrap());
        }
    }
    thread::sleep(PRETEND_LIFE);
}

/// Opens a part of the guest's memory as QEMU does, from inside its jail, and checks it's the memfd
/// the app made for it.
fn open_guest_memory(object: &str) {
    let path = object.split(',').find_map(|part| part.strip_prefix("mem-path=")).unwrap();
    let opened = fs::OpenOptions::new().read(true).write(true).open(path);
    assert!(opened.is_ok(), "QEMU can open {path}: {opened:?}");
    let target = fs::read_link(path).unwrap_or_default();
    assert!(target.to_string_lossy().starts_with(GUEST_MEMORY), "{path} is the guest's memory: {target:?}");
}

/// Where the daemon finds the guest's memory, which the app makes as memfds.
fn holds_guest_memory() -> bool {
    Part::ALL.iter().all(|part| {
        let target = fs::read_link(format!("/proc/self/fd/{}", part.descriptor())).unwrap_or_default();
        target.to_string_lossy().starts_with(GUEST_MEMORY)
    })
}

/// Makes the sockets QEMU connects to, as the daemon does once it's ready. They go in its slot's run
/// folder, which isn't slot 0's when real VMs are running on this PC.
fn pretend_daemon(base: &str) {
    assert!(holds_guest_memory(), "the daemon is handed the guest's memory");
    let folder = Slot::from_environment().unwrap().run_folder(Path::new(base));
    let _sockets = ["vgpu.sock", "global.sock"].map(|name| UnixListener::bind(folder.join(name)).unwrap());
    thread::sleep(PRETEND_LIFE);
}

fn wait_for(context: &Context, machine: &Machine, wanted: impl Fn(&Status) -> bool) -> Status {
    let end = Instant::now() + MOST_WAIT;
    loop {
        let found = status(context, machine);
        if wanted(&found) {
            return found;
        }
        assert!(
            Instant::now() < end,
            "it never happened: {found:?}\nlog: {}",
            guesthouse_app_core::log::read_log(context)
        );
        thread::sleep(POLL);
    }
}

/// A task outlives what starts it, so nothing waits for it here but a thread.
fn collect(mut task: Child) {
    thread::spawn(move || task.wait());
}

fn jailed(pid: u32) -> bool {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let says = |wanted: [&str; 2]| status.lines().any(|line| line.split_whitespace().eq(wanted));
    says(["NoNewPrivs:", "1"]) && says(["Seccomp:", "2"])
}

/// The daemon, if it's the real one: the pretend one doesn't jail itself.
fn real_daemon(context: &Context) -> Option<u32> {
    env::var_os(REAL_DAEMON_VARIABLE)?;
    let program = context.paths.binary("guesthouse-daemon");
    let pids =
        fs::read_dir("/proc").unwrap().flatten().filter_map(|entry| entry.file_name().to_str()?.parse().ok());
    pids.into_iter().find(|pid: &u32| {
        let words = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        words.split(|byte| *byte == 0).next() == Some(program.as_os_str().as_encoded_bytes())
    })
}

/// Gives the slot the machine ran in, which isn't slot 0 when real VMs are running on this PC.
fn start_and_stop(context: &Context, machine: &Machine) -> Slot {
    let task = launch(context, "start", machine, &[], None).unwrap();
    collect(task);
    let running = wait_for(context, machine, |found| found.state == State::Running && !found.busy);
    assert_eq!(running.error, "", "{running:?}");
    println!("{} is running in slot {:?}", machine.name, running.slot);
    let slot = Slot::parse(running.slot.map(|number| number.to_string()).as_deref()).unwrap();
    if machine.has_gpu() {
        let qemu = qemu_pid(context, Some(slot)).unwrap();
        assert!(jailed(qemu), "QEMU runs jailed");
        if let Some(daemon) = real_daemon(context) {
            assert!(jailed(daemon), "the daemon runs jailed");
            println!("the daemon is jailed");
        }
        let note = if running.jail_note.is_empty() { "a whole jail" } else { &running.jail_note };
        println!("QEMU is jailed: {note}");
    }
    let task = launch(context, "stop", machine, &[], None).unwrap();
    collect(task);
    let off = wait_for(context, machine, |found| found.state == State::Off && !found.busy);
    assert_eq!(off.error, "", "{off:?}");
    assert_eq!(qemu_pid(context, Some(slot)), None);
    println!("{} has stopped", machine.name);
    slot
}

fn make_machine(context: &Context, id: &str, kind: Kind) -> Machine {
    let folder = context.paths.machines.join(id);
    fs::create_dir_all(&folder).unwrap();
    let (disk, vars) = (folder.join("disk.qcow2"), folder.join("vars.fd"));
    fs::write(&disk, "").unwrap();
    fs::write(&vars, "").unwrap();
    let machine = Machine {
        kind,
        cores: 2,
        ram_gib: 4,
        ..Machine::new(id, id, &disk.to_string_lossy(), &vars.to_string_lossy())
    };
    machine.save(context).unwrap();
    machine
}

fn inner() {
    let mut context = Context::from_process().unwrap();
    context.paths.binaries = env::var_os("FLOW_BINARIES").unwrap().into();
    let plain = make_machine(&context, "plain", Kind::Plain);
    start_and_stop(&context, &plain);
    let windows = make_machine(&context, "windows", Kind::Windows);
    make_token(&context, &windows).unwrap();
    fs::create_dir_all(driver_folder(&context)).unwrap();
    write_setup(&context, json!({"card": {"device": 1}}).as_object().cloned().unwrap()).unwrap();
    start_and_stop(&context, &windows);
    let shared_memory = fs::read_dir(&context.paths.shared_memory).unwrap().count();
    assert_eq!(shared_memory, 0, "nothing of the guest's is left in shared memory");
    if passt_installed() {
        let on_passt = Machine { passt: true, ..windows };
        on_passt.save(&context).unwrap();
        let slot = start_and_stop(&context, &on_passt);
        let pid_file = context.run_folder(Some(slot)).join(PASST_PID_FILE);
        let noted = fs::read_to_string(pid_file).unwrap();
        let passt: u32 = noted.split_whitespace().next().unwrap().parse().unwrap();
        assert!(!Path::new(&format!("/proc/{passt}")).exists(), "passt went with QEMU");
        println!("passt went with QEMU");
    }
}

/// A port nothing has now. The real one could be held by a real VM.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
}

fn copy_program(from: &Path, to: PathBuf) {
    fs::copy(from, to).unwrap();
}

/// Where this test's own tasks read the host's memory figures. Only this program knows it.
const MEMINFO_VARIABLE: &str = "FLOW_MEMINFO";
/// Enough for the test's machine however busy the PC is.
const PLENTY_OF_MEMORY: &str = "MemTotal: 67108864 kB\nMemAvailable: 67108864 kB\n";
const PLENTY_OF_GIB: u64 = 64;

fn outer() {
    let this = env::current_exe().unwrap();
    let folder = env::temp_dir().join(format!("gh-flow-{}", process::id()));
    let bin = folder.join("bin");
    let program = folder.join("program");
    for path in [&bin, &program.join("guest"), &folder.join("shm"), &folder.join("run")] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(program.join("guest/display.ps1"), "what the session sees").unwrap();
    fs::write(folder.join("meminfo"), PLENTY_OF_MEMORY).unwrap();
    copy_program(&this, bin.join("guesthouse-task"));
    copy_program(&this, bin.join("qemu"));
    let daemon = env::var_os(REAL_DAEMON_VARIABLE).map_or_else(|| this.clone(), PathBuf::from);
    copy_program(&daemon, bin.join("guesthouse-daemon"));
    copy_program(Path::new(env!("CARGO_BIN_EXE_guesthouse-jail")), bin.join("guesthouse-jail"));
    let done = Command::new(&this)
        .env(INNER_VARIABLE, "1")
        .env("FLOW_BINARIES", &bin)
        .env("HOME", folder.join("home"))
        // These win over HOME, and would send the test to the real folders.
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("GUESTHOUSE_STORE")
        .env_remove("GUESTHOUSE_MACHINES")
        .env("XDG_RUNTIME_DIR", folder.join("run"))
        .env("GUESTHOUSE_SHARED_MEMORY", folder.join("shm"))
        .env(MEMINFO_VARIABLE, folder.join("meminfo"))
        .env("GUESTHOUSE_PROGRAM", &program)
        .env("AGENT_PORT", free_port().to_string())
        .env("QEMU", bin.join("qemu"))
        .status()
        .unwrap();
    let _ = fs::remove_dir_all(&folder);
    assert!(done.success(), "the flow failed");
    println!("flow ok");
}

/// The real task, with the one difference that it can't be refused for what else the PC is doing.
fn task_with_room(arguments: &[String]) {
    let [action, machine, rest @ ..] = arguments else { panic!("a task needs an action and a machine") };
    let mut context = Context::from_process().unwrap();
    context.meminfo = env::var_os(MEMINFO_VARIABLE).unwrap().into();
    context.ram_limit = Some(PLENTY_OF_GIB);
    context.reserve_memory = guesthouse_app_core::guest_memory::GuestMemory::sized;
    // The flow reads each task's file, as the app does, so the exit code is left to the real task.
    let _ = run_task(&context, &LocalHost { context: &context }, action, machine, rest);
}

fn main() {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let name = env::args().next().unwrap_or_default();
    if name.ends_with("/qemu") {
        pretend_qemu(&arguments);
    } else if name.ends_with("guesthouse-daemon") {
        pretend_daemon(&arguments[0]);
    } else if name.ends_with("guesthouse-task") {
        task_with_room(&arguments);
    } else if env::var_os(INNER_VARIABLE).is_some() {
        inner();
    } else {
        outer();
    }
}
