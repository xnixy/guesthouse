use super::desktop::{BOOTS_FILE, SERIAL_FILE};
use super::guest_files::{place_driver_files, place_profile_store};
use super::*;
use crate::agent::stand_in::ScriptedAgent;
use crate::clock::Clock;
use crate::machine::Kind;
use crate::processes::stand_in::fake_process;
use crate::slots;
use crate::tasks::{read_task, status};
use crate::testing::{Scratch, scratch_context};
use crate::vm_host::stand_in::RecordingHost;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Time that only moves when something sleeps.
#[derive(Default)]
pub(super) struct FakeClock(AtomicU64);

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::Relaxed))
    }

    fn sleep(&self, time: Duration) {
        self.0.fetch_add(time.as_millis() as u64, Ordering::Relaxed);
    }
}

pub(super) fn machine(context: &Context) -> Machine {
    let mut machine = Machine::new("x", "x", "", "");
    machine.save_files(context);
    machine
}

impl Machine {
    /// Gives it a real disk and firmware file, so a test can get past the checks.
    fn save_files(&mut self, context: &Context) {
        let folder = self.folder(context);
        fs::create_dir_all(&folder).unwrap();
        self.disk = folder.join("disk.qcow2").display().to_string();
        self.vars = folder.join("vars.fd").display().to_string();
        fs::write(&self.disk, "").unwrap();
        fs::write(&self.vars, "").unwrap();
        self.save(context).unwrap();
    }
}

#[test]
fn a_shared_folder_is_reported_only_if_it_did_not_map() {
    let scratch = Scratch::new();
    let mut context = scratch_context(&scratch);
    scratch.write("program/guest/share-start.ps1", "map it");
    let agent = Arc::new(ScriptedAgent::answering(&["mapped\r\n", "not mapped\r\n", ""]));
    context.agent = agent.clone();
    let shared = Machine { share: "/folder".into(), ..machine(&context) };
    crate::agent::make_token(&context, &shared).unwrap();
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", shared.clone());
    apply_share(&mut task).unwrap();
    assert_eq!(task.error, "");
    apply_share(&mut task).unwrap();
    assert!(task.error.contains("did not reach"));
    task.error.clear();
    apply_share(&mut task).unwrap();
    assert!(task.error.contains("did not reach"));
    let asked = agent.requests.lock().unwrap().len();
    let mut alone = Task::new(&context, &host, "start", Machine { share: String::new(), ..shared });
    apply_share(&mut alone).unwrap();
    assert_eq!(agent.requests.lock().unwrap().len(), asked, "with no folder, the guest is not asked");
}

pub(super) fn with_vm_running(context: &Context) {
    let qemu = context.paths.qemu.display().to_string();
    let socket = format!("unix:{}/qmp.sock,server=on", context.paths.run_folder(slots::first()).display());
    fake_process(context, 40, &[&qemu, "-qmp", &socket], &[], "");
}

pub(super) fn plain_task_context(scratch: &Scratch) -> Context {
    let mut context = scratch_context(scratch);
    context.cpus = crate::host::stand_in::fake_cpus(scratch, "cpus", &[32768; 16]);
    context.meminfo =
        scratch.write("meminfo", "MemTotal:       100000000 kB\nMemAvailable:   100000000 kB\n");
    context.clock = Arc::new(FakeClock::default());
    context
}

#[test]
fn a_plain_machine_starts_through_the_host_with_its_iso() {
    let scratch = Scratch::new();
    let context = plain_task_context(&scratch);
    let iso = scratch.write("disc.iso", "").display().to_string();
    let vm = Machine { kind: Kind::Plain, iso: iso.clone(), ..machine(&context) };
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", vm);
    start(&mut task).unwrap();
    assert_eq!(host.said(), ["start plain"]);
    assert_eq!(host.plans.lock().unwrap()[0].boot, Boot::Plain { iso });
}

#[test]
fn a_gpu_machine_with_no_setup_is_refused_before_anything_starts() {
    let scratch = Scratch::new();
    let context = plain_task_context(&scratch);
    let vm = machine(&context);
    crate::agent::make_token(&context, &vm).unwrap();
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", vm);
    let refused = start(&mut task).unwrap_err();
    assert_eq!(refused.0, crate::start_checks::NO_SETUP);
    assert!(host.said().is_empty(), "nothing was started");
}

#[test]
fn a_gpu_machine_with_no_token_is_refused_before_anything_starts() {
    let scratch = Scratch::new();
    let context = plain_task_context(&scratch);
    fs::create_dir_all(crate::settings::driver_folder(&context)).unwrap();
    let card = serde_json::json!({"card": {"device": 1}});
    crate::settings::write_setup(&context, card.as_object().cloned().unwrap()).unwrap();
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", machine(&context));
    let refused = start(&mut task).unwrap_err();
    assert_eq!(refused.0, crate::start_checks::NO_TOKEN);
    assert!(host.said().is_empty(), "nothing was started");
}

#[test]
fn a_gpu_machine_starts_through_the_same_host_and_sets_up_its_desktop() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    scratch.write("program/guest/display.ps1", "what the session sees");
    let agent = Arc::new(ScriptedAgent::answering(&[
        "3
",
        "gpu working 1
",
        "device missing",
        "",
        "screen basic 1920x1080 64 hdr 0",
        "",
        "screen 1920x1080
",
        "",
        "screen basic 1920x1080 64 hdr 0",
    ]));
    context.agent = agent.clone();
    let vm = machine(&context);
    crate::agent::make_token(&context, &vm).unwrap();
    fs::create_dir_all(context.run_folder(None)).unwrap();
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", vm);
    boot(&mut task).unwrap();
    assert_eq!(host.said(), ["start gpu"]);
    assert_eq!(task.error, "");
    let asked = agent.requests.lock().unwrap();
    assert!(asked[1].2.contains("Win32_VideoController"), "the GPU is checked before anything else");
    assert!(asked[2].2.contains("DEVPKEY"), "then the display driver");
    assert!(asked[3].2.contains("WriteAllBytes"), "a script goes to the guest first");
    assert!(asked[4].2.contains("New-ScheduledTaskPrincipal"), "and runs in the user's session");
}

#[test]
fn a_desktop_at_the_wrong_size_is_set_and_checked_again() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    scratch.write("program/guest/display.ps1", "what the session sees");
    scratch.write("program/guest/setmode.ps1", "set the mode");
    let answers = [
        "3
",
        "gpu working 1
",
        "device missing",
        "",
        "screen basic 1024x768 64 hdr 0",
        "",
        "screen 1024x768
",
        "",
        "",
        "",
        "screen 800x600
",
        "",
        "screen basic 800x600 64 hdr 0",
    ];
    context.agent = Arc::new(ScriptedAgent::answering(&answers));
    let vm = machine(&context);
    crate::agent::make_token(&context, &vm).unwrap();
    fs::create_dir_all(context.run_folder(None)).unwrap();
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", vm);
    boot(&mut task).unwrap();
    assert_eq!(task.error, "The desktop is 800x600, not 1920x1080.");
}

#[test]
fn a_failed_gpu_start_gives_back_what_it_took() {
    let scratch = Scratch::new();
    let context = plain_task_context(&scratch);
    let vm = machine(&context);
    let host = RecordingHost::default();
    *host.start_failure.lock().unwrap() = Some(Failure::new("no daemon"));
    let mut task = Task::new(&context, &host, "start", vm);
    // A GPU machine needs real shared memory facts, so this goes straight to the part that matters.
    let booted = boot(&mut task);
    assert!(booted.is_err());
    assert_eq!(host.said(), ["start gpu", "release"]);
}

/// A guest whose desktop comes up, and which then says this about its GPU every time it's asked.
fn guest_saying(context: &mut Context, gpu: &'static str) -> Machine {
    let mut answers = vec!["3\n"];
    answers.resize(20, gpu);
    context.agent = Arc::new(ScriptedAgent::answering(&answers));
    context.clock = Arc::new(FakeClock::default());
    let vm = machine(context);
    crate::agent::make_token(context, &vm).unwrap();
    fs::create_dir_all(context.run_folder(None)).unwrap();
    vm
}

#[test]
fn a_guest_with_no_gpu_fails_the_start_says_so_and_turns_off() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    let vm = guest_saying(&mut context, "gpu working 0\n");
    let log = "INFO  Vgpu: Reset\nERROR /dev/shm/window: No such file\nWARN  it failed\n";
    fs::write(context.run_folder(None).join(crate::daemon::OUTPUT_FILE), log).unwrap();
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", vm);
    let error = boot(&mut task).unwrap_err();
    assert!(error.0.contains("without its GPU"), "{error}");
    assert!(error.0.contains("/dev/shm/window: No such file"), "{error}");
    assert_eq!(host.said(), ["start gpu", "stop 30"]);
}

#[test]
fn a_guest_that_will_not_say_counts_as_having_no_gpu() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    let vm = guest_saying(&mut context, "");
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", vm);
    assert!(boot(&mut task).unwrap_err().0.contains("without its GPU"));
}

#[test]
fn a_gpu_that_is_a_moment_late_is_waited_for() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    let vm = guest_saying(&mut context, "");
    context.agent = Arc::new(ScriptedAgent::answering(&["gpu working 0\n", "", "gpu working 1\n"]));
    let host = RecordingHost::default();
    let task = Task::new(&context, &host, "start", vm);
    assert_eq!(check_gpu(&task), Ok(()));
}

#[test]
fn a_gpu_start_that_fails_after_the_vm_is_up_turns_it_off() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    context.clock = Arc::new(FakeClock::default());
    let vm = machine(&context);
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", vm);
    let booted = boot(&mut task);
    assert!(booted.unwrap_err().0.contains("desktop never came up"));
    assert_eq!(host.said(), ["start gpu", "stop 30"]);
}

#[test]
fn a_vm_that_will_not_stop_after_a_failed_start_is_forced_and_keeps_the_first_reason() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    context.clock = Arc::new(FakeClock::default());
    let vm = machine(&context);
    let host = RecordingHost::default();
    *host.stop_answer.lock().unwrap() = Some(Err(Failure::new("it would not go")));
    let mut task = Task::new(&context, &host, "start", vm);
    let booted = boot(&mut task);
    assert!(booted.unwrap_err().0.contains("desktop never came up"));
    assert_eq!(host.said(), ["start gpu", "stop 30", "release"]);
}

#[test]
fn stopping_says_when_the_guest_ran_out_of_time() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let host = RecordingHost::default();
    *host.stop_answer.lock().unwrap() = Some(Ok(Stopped::TurnedOff));
    let mut task = Task::new(&context, &host, "stop", machine(&context));
    stop(&mut task).unwrap();
    assert!(task.error.contains("Windows did not shut down in time"));
    task.machine.kind = Kind::Plain;
    stop(&mut task).unwrap();
    assert!(task.error.contains("The guest did not shut down in time"));
}

#[test]
fn a_disk_grows_and_only_grows() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let vm = machine(&context);
    let done =
        Command::new("qemu-img").args(["create", "-q", "-f", "qcow2", &vm.disk, "2G"]).status().unwrap();
    assert!(done.success());
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "grow", vm.clone());
    grow(&mut task, 3).unwrap();
    assert_eq!(disk_size_gib(&vm.disk).unwrap(), 3.0);
    assert!(grow(&mut task, 3).unwrap_err().0.contains("only grow"));
    assert!(grow(&mut task, 1).unwrap_err().0.contains("only grow"));
    with_vm_running(&context);
    assert!(grow(&mut task, 4).unwrap_err().0.contains("while the VM is off"));
}

#[test]
fn a_task_that_cannot_load_its_machine_says_so() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    fs::create_dir_all(context.run_folder(None)).unwrap();
    let host = RecordingHost::default();
    let result = run_task(&context, &host, "start", "no-such-machine", &[]);
    assert!(result.is_err_and(|error| error.0.contains("Could not read")));
    let found = status(&context, &Machine::new("no-such-machine", "x", "/d", "/v"));
    assert!(found.error.contains("Could not read") && !found.busy, "{found:?}");
}

#[test]
fn a_task_leaves_its_error_and_clears_its_pid() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    fs::create_dir_all(context.run_folder(None)).unwrap();
    let vm = machine(&context);
    let host = RecordingHost::default();
    let result = run_task(&context, &host, "no-such-action", &vm.id, &[]);
    assert!(result.is_err_and(|error| error.0.contains("no action")));
    let record = read_task(&context, None);
    assert_eq!(record.pid, 0);
    assert!(record.error.contains("no action"));
}

#[test]
fn a_new_install_gets_its_run_folder_from_the_first_task() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let _ = fs::remove_dir_all(context.run_folder(None));
    assert!(take_lock(&context).is_ok());
    assert!(context.run_folder(None).is_dir());
}

#[test]
fn only_one_task_holds_the_lock() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    fs::create_dir_all(context.run_folder(None)).unwrap();
    let held = take_lock(&context).unwrap();
    assert!(take_lock(&context).unwrap_err().0.contains("still in progress"));
    // Not a drop: a child another test is forking can hold a copy of the file for a moment, and with
    // that the lock. Unlocking goes for every copy.
    flock(&held, FlockOperation::Unlock).unwrap();
    take_lock(&context).unwrap();
}

#[test]
fn a_reboot_is_seen_from_the_serial_log() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let folder = context.run_folder(None);
    fs::create_dir_all(&folder).unwrap();
    assert!(!rebooted(&context, None), "a VM the app never set up is left as it is");
    fs::write(folder.join(SERIAL_FILE), "BdsDxe: starting\n").unwrap();
    fs::write(folder.join(BOOTS_FILE), "1").unwrap();
    assert!(!rebooted(&context, None));
    fs::write(folder.join(SERIAL_FILE), "BdsDxe: starting\nBdsDxe: starting\n").unwrap();
    assert!(rebooted(&context, None));
}

fn with_driver_files(scratch: &Scratch, context: &Context) {
    use crate::settings::{driver_folder, write_setup};
    fs::create_dir_all(driver_folder(context)).unwrap();
    let inf = crate::setup::guest_files::stand_in::FILES_INF;
    fs::write(driver_folder(context).join("nv_dispi.inf"), inf).unwrap();
    let card = serde_json::json!({"card": {"device": 0x2B85}});
    write_setup(context, card.as_object().cloned().unwrap()).unwrap();
    scratch.write("program/guest/nv-files.ps1", "copies files");
}

fn placing_task<'a>(context: &'a Context, host: &'a RecordingHost) -> Task<'a> {
    let vm = machine(context);
    crate::agent::make_token(context, &vm).unwrap();
    Task::new(context, host, "start", vm)
}

#[test]
fn a_guest_with_all_the_drivers_files_is_left_alone() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    with_driver_files(&scratch, &context);
    let agent = Arc::new(ScriptedAgent::answering(&["", "missing 0\r\n"]));
    context.agent = agent.clone();
    let host = RecordingHost::default();
    place_driver_files(&mut placing_task(&context, &host)).unwrap();
    let asked = agent.requests.lock().unwrap();
    assert_eq!(asked.len(), 2, "only the list and the count");
}

#[test]
fn missing_files_are_placed_with_the_guests_script() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    with_driver_files(&scratch, &context);
    let agent =
        Arc::new(ScriptedAgent::answering(&["", "missing 4\r\n", "", "copied System32\\one.dll\r\n"]));
    context.agent = agent.clone();
    let host = RecordingHost::default();
    let mut task = placing_task(&context, &host);
    place_driver_files(&mut task).unwrap();
    assert_eq!(task.error, "");
    let asked = agent.requests.lock().unwrap();
    assert!(asked[2].2.contains("nv-files.ps1"), "the script goes to the guest");
    assert!(asked[3].2.contains("nv-files.txt"), "and is given the list");
}

#[test]
fn a_file_that_fails_is_said_and_the_start_goes_on() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    with_driver_files(&scratch, &context);
    let agent =
        Arc::new(ScriptedAgent::answering(&["", "missing 1\r\n", "", "FAILED System32\\one.dll\r\n"]));
    context.agent = agent.clone();
    let host = RecordingHost::default();
    let mut task = placing_task(&context, &host);
    place_driver_files(&mut task).unwrap();
    assert!(task.error.contains("did not reach Windows"));
}

#[test]
fn with_no_card_known_the_guest_is_not_asked() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    let agent = Arc::new(ScriptedAgent::answering(&[]));
    context.agent = agent.clone();
    let host = RecordingHost::default();
    place_driver_files(&mut placing_task(&context, &host)).unwrap();
    assert!(agent.requests.lock().unwrap().is_empty());
}

#[test]
fn with_no_script_the_start_goes_on_and_says_so() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    with_driver_files(&scratch, &context);
    fs::remove_file(scratch.path("program/guest/nv-files.ps1")).unwrap();
    context.agent = Arc::new(ScriptedAgent::answering(&["", "missing 4\r\n"]));
    let host = RecordingHost::default();
    let mut task = placing_task(&context, &host);
    place_driver_files(&mut task).unwrap();
    assert!(task.error.contains("no script"));
}

#[test]
fn the_profile_store_script_goes_to_the_guest_and_a_failure_is_said() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    scratch.write("program/guest/nv-profiles.ps1", "make the store");
    let agent = Arc::new(ScriptedAgent::answering(&["kept the store\r\n", "FAILED the store: locked\r\n"]));
    context.agent = agent.clone();
    let host = RecordingHost::default();
    let mut task = placing_task(&context, &host);
    place_profile_store(&mut task).unwrap();
    assert_eq!(task.error, "");
    place_profile_store(&mut task).unwrap();
    assert!(task.error.contains("settings store"));
    let asked = agent.requests.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].2, "make the store");
}

#[test]
fn with_no_profile_store_script_the_guest_is_not_asked() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    let agent = Arc::new(ScriptedAgent::answering(&[]));
    context.agent = agent.clone();
    let host = RecordingHost::default();
    place_profile_store(&mut placing_task(&context, &host)).unwrap();
    assert!(agent.requests.lock().unwrap().is_empty());
}

#[test]
fn an_install_with_no_room_to_boot_ends_well_and_stays_off() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    context.ram_limit = Some(1);
    let host = RecordingHost::default();
    let mut vm = machine(&context);
    vm.installing = false;
    let mut task = Task::new(&context, &host, "install", vm);
    start_if_there_is_room(&mut task, "Windows").unwrap();
    assert_eq!(task.error, "Windows is installed. There isn't room to start it right now.");
    assert!(host.said().is_empty(), "nothing may start");
}

#[test]
fn an_intel_machine_is_given_neither_nvidias_files_nor_amds_and_the_guest_is_not_asked() {
    let scratch = Scratch::new();
    let mut context = plain_task_context(&scratch);
    let agent = Arc::new(ScriptedAgent::answering(&[]));
    context.agent = agent.clone();
    let mut intel = machine(&context);
    intel.use_windows_card(GpuMaker::Intel, "0000:00:02.0");
    let host = RecordingHost::default();
    let mut task = Task::new(&context, &host, "start", intel);
    let refused = place_files_for_maker(&mut task).unwrap_err();
    assert_eq!(refused.0, NO_INTEL_SUPPORT);
    assert!(agent.requests.lock().unwrap().is_empty(), "nothing was sent to the guest");
    assert_eq!(task.error, "");
}
