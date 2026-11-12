//! A drive given to the real QEMU. It needs root, a loop device, KVM and the QEMU the app starts, so it
//! runs only when asked: `cargo test -p guesthouse-app-core drive_tests -- --ignored`.

use super::*;
use crate::context::Context;
use crate::firmware::Firmware;
use crate::host_drives::OpenDrives;
use crate::jailing::Offer;
use crate::machine::{Drive, Kind};
use crate::monitor::Command as Qmp;
use crate::slots;
use crate::testing::{Scratch, scratch_context};
use crate::vm_args::{Boot, QemuJail, program_on_path};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
const DRIVE_MIB: u64 = 64;
const QEMU: &str = "qemu-system-x86_64";
const OVMF_FOLDER: &str = "/usr/share/OVMF";
const QEMU_STOPS: Duration = Duration::from_secs(10);

/// A loop device over a file, released when this goes.
struct LoopDevice(String);

impl LoopDevice {
    fn over(file: &Path) -> Self {
        let made = Command::new("losetup").args(["--find", "--show"]).arg(file).output().unwrap();
        assert!(made.status.success(), "losetup needs root");
        Self(String::from_utf8(made.stdout).unwrap().trim().to_owned())
    }

    fn mount_on(&self, folder: &Path) -> std::process::Output {
        Command::new("mount").arg(&self.0).arg(folder).output().unwrap()
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        let _ = Command::new("losetup").args(["--detach", &self.0]).status();
    }
}

fn run(program: &str, arguments: &[&str]) {
    let done = Command::new(program).args(arguments).output().unwrap();
    assert!(done.status.success(), "{program}: {}", String::from_utf8_lossy(&done.stderr));
}

/// How the QEMU under test is kept.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Jailed {
    No,
    /// With Landlock and seccomp and no namespaces.
    InTheKernelOnly,
    /// And in bubblewrap's namespaces too.
    InNamespaces,
}

macro_rules! lab_test {
    ($name:ident, $jailed:expr) => {
        #[test]
        #[ignore = "needs root, a loop device, /dev/kvm, qemu-system-x86_64 and a built guesthouse-jail"]
        fn $name() {
            give_a_loop_device($jailed);
        }
    };
}

lab_test!(a_loop_device_is_given_to_a_real_qemu_which_keeps_the_host_from_mounting_it, Jailed::No);
lab_test!(a_jailed_qemu_uses_the_drive_it_is_handed_and_cannot_open_it_by_path, Jailed::InTheKernelOnly);
lab_test!(a_qemu_in_namespaces_uses_the_drive_it_is_handed_and_has_no_path_to_it, Jailed::InNamespaces);

/// The jail's launcher is built beside the test program, one folder up.
fn launcher() -> PathBuf {
    let test_program = std::env::current_exe().unwrap();
    let launcher = test_program.parent().unwrap().parent().unwrap().join("guesthouse-jail");
    assert!(launcher.exists(), "build guesthouse-app-core first: {} is missing", launcher.display());
    launcher
}

fn jail_for(plan: &VmPlan, jailed: Jailed) -> Option<QemuJail> {
    if jailed == Jailed::No {
        return None;
    }
    let offer = Offer::here(plan);
    assert_eq!(offer.support.refusal(), None, "this kernel can't jail QEMU");
    let namespaces = jailed == Jailed::InNamespaces;
    assert!(offer.namespaces || !namespaces, "this PC can't run bubblewrap's namespaces");
    Some(QemuJail {
        launcher: launcher(),
        namespaces,
        qemu_sandbox: offer.qemu_sandbox,
        sound_socket: None,
        backing_files: Vec::new(),
    })
}

fn give_a_loop_device(jailed: Jailed) {
    // Both use the first slot's ports and run folder.
    let _one_at_a_time = ONE_AT_A_TIME.lock().unwrap_or_else(|held| held.into_inner());
    let scratch = Scratch::new();
    let mut context = scratch_context(&scratch);
    let image = scratch.write("drive.img", "");
    run("truncate", &["-s", &format!("{DRIVE_MIB}M"), image.to_str().unwrap()]);
    let device = LoopDevice::over(&image);
    run("mkfs.ext4", &["-q", &device.0]);
    // The kernel mounts it when nothing holds it, so a refusal below is the holding's.
    let place = scratch.path("place");
    fs::create_dir_all(&place).unwrap();
    assert!(device.mount_on(&place).status.success());
    run("umount", &[place.to_str().unwrap()]);

    fs::create_dir_all(scratch.path("drive-ids")).unwrap();
    std::os::unix::fs::symlink(&device.0, scratch.path("drive-ids/lab-loop")).unwrap();
    let disk = scratch.path("disk.qcow2");
    run("qemu-img", &["create", "-f", "qcow2", disk.to_str().unwrap(), "1M"]);
    let vars = scratch.path("vars.fd");
    fs::copy(Path::new(OVMF_FOLDER).join("OVMF_VARS_4M.fd"), &vars).unwrap();
    let code = PathBuf::from(OVMF_FOLDER).join("OVMF_CODE_4M.fd");
    context.paths.firmware =
        Firmware { code: code.clone(), vars: vars.clone(), code_secure: code, vars_secure: vars.clone() };
    context.paths.qemu = program_on_path(QEMU).expect("qemu-system-x86_64 must be installed");

    let machine = Machine {
        kind: Kind::Plain,
        cores: 1,
        ram_gib: 1,
        drives: vec![Drive { id: "lab-loop".into() }],
        ..Machine::new("lab", "Lab", disk.to_str().unwrap(), vars.to_str().unwrap())
    };
    let plan = VmPlan::new(&context, &machine, Boot::Plain { iso: String::new() }, slots::first(), 0);
    let plan = VmPlan { jail: jail_for(&plan, jailed), ..plan };
    let host = LocalHost { context: &context };
    host.start(&plan, &machine).unwrap();
    // Stops the QEMU whatever the checks below find.
    let _stop = StopQemu(&context);

    let nodes = context.ask_monitor(None, &Qmp::new("query-named-block-nodes", json!({}))).unwrap();
    let nodes = nodes["return"].as_array().unwrap();
    let held = nodes.iter().find(|node| node["drv"] == "host_device").unwrap();
    assert_eq!(held["ro"], false);
    assert_eq!(held["image"]["virtual-size"], DRIVE_MIB << 20);

    // Only the descriptor it was handed leads to the drive: by its path QEMU gets nowhere in a jail.
    let probe =
        json!({"driver": "host_device", "node-name": "probe", "filename": device.0, "read-only": true});
    let by_path = context.ask_monitor(None, &Qmp::new("blockdev-add", probe)).unwrap();
    println!("by path: {by_path}");
    assert_eq!(by_path.get("error").is_none(), jailed == Jailed::No);

    let mounted = device.mount_on(&place);
    assert!(!mounted.status.success(), "the host mounted a drive a machine has");
    println!("mount: {}", String::from_utf8_lossy(&mounted.stderr).trim());
    let second = OpenDrives::open(&context, &machine.drives).unwrap_err();
    assert!(second.0.contains("in use on this PC"), "{second}");
}

/// Asks QEMU to quit and waits for it to let go of the drive.
struct StopQemu<'a>(&'a Context);

impl Drop for StopQemu<'_> {
    fn drop(&mut self) {
        let _ = self.0.ask_monitor(None, &Qmp::bare("quit"));
        let end = Instant::now() + QEMU_STOPS;
        while self.0.run_state(None).is_some() && Instant::now() < end {
            thread::sleep(Duration::from_millis(100));
        }
    }
}
