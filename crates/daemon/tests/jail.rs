//! The daemon's jail on this PC's kernel, with no GPU. A child applies the daemon's own jail and tries
//! what a guest that had taken it over would. With the `stand-in` feature, the real program is run
//! too, and has to be serving while jailed.

use guesthouse_daemon::jail::{Needs, lock_calls, lock_files};
use guesthouse_jail::Support;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const CASE_VARIABLE: &str = "DAEMON_JAIL_CASE";
const FOLDER_VARIABLE: &str = "DAEMON_JAIL_FOLDER";

fn refused<T>(result: std::io::Result<T>) -> bool {
    matches!(result, Err(error) if error.kind() == ErrorKind::PermissionDenied)
}

fn last_error() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}

/// What a guest that has the daemon would try first.
fn jailed(folder: &Path) {
    let own = folder.join("run-2");
    let locked = lock_files(&Needs { run_folder: own.clone(), ..Needs::default() }).unwrap();
    lock_calls(locked).unwrap();
    fs::write(own.join("note"), "its own folder").unwrap();
    assert!(refused(fs::read(folder.join("home/.ssh/id_ed25519"))), "a file in the home folder");
    let home = env::var_os("HOME").map(PathBuf::from).unwrap();
    assert!(refused(fs::read_dir(&home)), "the real home folder");
    assert!(refused(fs::read(folder.join("run-1/vgpu.sock"))), "another VM's run folder");
    assert!(refused(fs::write(folder.join("gpupv-ram-1"), "")), "another VM's memory");
    assert!(refused(TcpListener::bind(("127.0.0.1", 0))), "a TCP socket");
    let traced = unsafe { libc::ptrace(libc::PTRACE_ATTACH, libc::getppid(), 0, 0) };
    assert_eq!((traced, last_error()), (-1, Some(libc::ENOSYS)), "ptrace of its parent");
    let ran = Command::new("/usr/bin/true").status();
    assert!(ran.is_err(), "running a program");
    let status = fs::read_to_string("/proc/self/status").unwrap();
    for wanted in [["NoNewPrivs:", "1"], ["Seccomp:", "2"]] {
        assert!(status.lines().any(|line| line.split_whitespace().eq(wanted)), "{wanted:?}");
    }
}

/// Setup runs `nvidia-smi` with its input on `/dev/null`, after the files' jail and before the filter.
fn setup_runs_a_program(folder: &Path) {
    lock_files(&Needs { run_folder: folder.join("run-2"), ..Needs::default() }).unwrap();
    let ran = Command::new("/usr/bin/true").stdin(std::process::Stdio::null()).output();
    assert!(ran.unwrap().status.success());
}

#[cfg(all(feature = "stand-in", not(feature = "nvidia")))]
fn serving(folder: &Path) {
    serve_jailed(folder, &[]);
}

/// The memory comes as two files that the daemon makes, which the jail names neither of.
#[cfg(all(feature = "stand-in", not(feature = "nvidia")))]
fn serving_with_memory_files(folder: &Path) {
    use guesthouse_daemon::guest_memory::WINDOW_SIZE;
    use std::os::unix::fs::PermissionsExt;

    let (ram, window) = (folder.join("guest-ram"), folder.join("guest-window"));
    serve_jailed(
        folder,
        &[
            ("GUESTHOUSE_RAM", "1G"),
            ("GUESTHOUSE_RAM_FILE", ram.to_str().unwrap()),
            ("GUESTHOUSE_WINDOW_FILE", window.to_str().unwrap()),
        ],
    );
    for (file, size) in [(&ram, 1 << 30), (&window, WINDOW_SIZE)] {
        let found = fs::metadata(file).unwrap();
        assert_eq!((found.len(), found.permissions().mode() & 0o777), (size, 0o600), "{}", file.display());
    }
}

#[cfg(all(feature = "stand-in", not(feature = "nvidia")))]
fn serve_jailed(folder: &Path, environment: &[(&str, &str)]) {
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    const PATIENCE: Duration = Duration::from_secs(20);
    let output = fs::File::create(folder.join("daemon.out")).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_guesthouse-daemon"))
        .arg(folder)
        .env("GUESTHOUSE_SLOT", "2")
        // Its own store and home, so the test never reads the real ones.
        .env("GUESTHOUSE_STORE", folder.join("store"))
        .env("HOME", folder.join("home"))
        .envs(environment.iter().copied())
        .env_remove("GUESTHOUSE_JAIL")
        .stderr(output)
        .spawn()
        .unwrap();
    let socket = folder.join("run-2/vgpu.sock");
    let end = Instant::now() + PATIENCE;
    while !folder.join("run-2/global.sock").exists() {
        assert!(Instant::now() < end && daemon.try_wait().unwrap().is_none(), "{}", log_of(folder));
        std::thread::sleep(Duration::from_millis(50));
    }
    let connected = UnixStream::connect(&socket);
    let status = fs::read_to_string(format!("/proc/{}/status", daemon.id())).unwrap();
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(connected.is_ok(), "QEMU's socket answers");
    for wanted in [["NoNewPrivs:", "1"], ["Seccomp:", "2"]] {
        assert!(
            status.lines().any(|line| line.split_whitespace().eq(wanted)),
            "{wanted:?}
{status}"
        );
    }
    assert!(log_of(folder).contains("jailed, with Landlock"), "{}", log_of(folder));
}

#[cfg(all(feature = "stand-in", not(feature = "nvidia")))]
fn log_of(folder: &Path) -> String {
    fs::read_to_string(folder.join("daemon.out")).unwrap_or_default()
}

fn set_up(folder: &Path) {
    for name in ["home/.ssh", "run-1", "run-2"] {
        fs::create_dir_all(folder.join(name)).unwrap();
    }
    fs::write(folder.join("home/.ssh/id_ed25519"), "a key").unwrap();
    fs::write(folder.join("run-1/vgpu.sock"), "").unwrap();
    fs::write(folder.join("gpupv-ram-1"), "").unwrap();
}

/// A case's name, and what the jailed child does.
type Case = (&'static str, fn(&Path));

const CASES: &[Case] = &[
    ("jailed", jailed),
    ("setup_runs_a_program", setup_runs_a_program),
    #[cfg(all(feature = "stand-in", not(feature = "nvidia")))]
    ("serving", serving),
    #[cfg(all(feature = "stand-in", not(feature = "nvidia")))]
    ("serving_with_memory_files", serving_with_memory_files),
];

fn main() {
    if let (Ok(case), Some(folder)) = (env::var(CASE_VARIABLE), env::var_os(FOLDER_VARIABLE)) {
        let (_, run) = CASES.iter().find(|(name, _)| *name == case).unwrap();
        run(Path::new(&folder));
        return;
    }
    if let Some(refusal) = Support::probe().refusal() {
        println!("skipped: {refusal}");
        return;
    }
    let mut failed = Vec::new();
    for (name, _) in CASES {
        let folder = env::temp_dir().join(format!("gh-daemon-jail-{}-{name}", process::id()));
        set_up(&folder);
        let done = Command::new(env::current_exe().unwrap())
            .env(CASE_VARIABLE, name)
            .env(FOLDER_VARIABLE, &folder)
            .env_remove("GUESTHOUSE_JAIL")
            .status()
            .unwrap();
        let _ = fs::remove_dir_all(&folder);
        println!("{name} ... {}", if done.success() { "ok" } else { "FAILED" });
        if !done.success() {
            failed.push(name);
        }
    }
    assert!(failed.is_empty(), "failed: {failed:?}");
}
