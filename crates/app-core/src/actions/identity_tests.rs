use super::*;
use crate::actions::tests::{machine, plain_task_context};
use crate::agent::stand_in::ScriptedAgent;
use crate::context::Context;
use crate::gpu_identity::{AMD_RENDER, MICROSOFT_RENDER, NVIDIA_RENDER, shown_identity};
use crate::machine::Machine;
use crate::testing::Scratch;
use crate::vm_host::stand_in::RecordingHost;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

const STAGED: &str = "signing: Valid\r\npackage in: oem7.inf\r\nidentity staged\r\n";
const REFUSED: &str = "stay: Windows' driver store has no copy of vrd.sys.\r\n";
const UPDATED: &str = "package in: oem9.inf
identity updated
identity staged
";
const SCRIPT_TEXT: &str = "make the package";
const CARD_DRIVER: &str = "32.0.16.1742";

struct Rig {
    context: Context,
    agent: Arc<ScriptedAgent>,
    machine: Machine,
}

impl Rig {
    /// A guest answering as given, on a QEMU that can set the identity unless told otherwise.
    fn new(scratch: &Scratch, qemu_can: bool, answers: &[&str]) -> Self {
        let mut context = plain_task_context(scratch);
        scratch.write("program/guest/gpu-identity.ps1", SCRIPT_TEXT);
        let listing =
            if qemu_can { "echo 'channel=<str>'; echo 'vendor-id=<uint16>'" } else { "echo 'channel=<str>'" };
        let qemu = scratch.write("qemu", format!("#!/bin/sh\n{listing}\n"));
        fs::set_permissions(&qemu, fs::Permissions::from_mode(0o700)).unwrap();
        context.paths.qemu = qemu;
        let agent = Arc::new(ScriptedAgent::answering(answers));
        context.agent = agent.clone();
        let machine = machine(&context);
        crate::agent::make_token(&context, &machine).unwrap();
        Self { context, agent, machine }
    }

    /// The same, for a machine on an AMD card.
    fn on_amd(scratch: &Scratch, answers: &[&str]) -> Self {
        let mut rig = Self::new(scratch, true, answers);
        rig.machine.amd_gpu = "0000:7c:00.0".into();
        rig
    }

    fn boot(&self) -> String {
        let host = RecordingHost::default();
        let mut task = Task::new(&self.context, &host, "start", self.machine.clone());
        set_up_identity(&mut task);
        task.error
    }

    /// A start, and whether it says Windows has to restart.
    fn boot_says_restart(&self) -> bool {
        let host = RecordingHost::default();
        let mut task = Task::new(&self.context, &host, "start", self.machine.clone());
        set_up_identity(&mut task)
    }

    /// Puts NVIDIA's Windows driver at this version where setup keeps it.
    fn with_card_driver(self, version: &str) -> Self {
        let folder = driver_folder(&self.context);
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("nv_dispi.inf"),
            format!(
                "[Version]
DriverVer = 01/01/2026, {version}
"
            ),
        )
        .unwrap();
        self
    }

    fn on_makers_number(self, package_version: &str) -> Self {
        let state = IdentityState {
            shown: Shown::Maker,
            package_version: package_version.into(),
            ..IdentityState::default()
        };
        write_state(&self.context, &self.machine, &state).unwrap();
        self
    }

    fn asked(&self) -> Vec<String> {
        self.agent.requests.lock().unwrap().iter().map(|(_, _, script)| script.clone()).collect()
    }

    fn state(&self) -> IdentityState {
        read_state(&self.context, &self.machine)
    }
}

#[test]
fn a_guest_that_takes_the_package_is_shown_nvidias_number_from_the_next_start() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[STAGED]);
    assert_eq!(shown_identity(&rig.context, &rig.machine), MICROSOFT_RENDER);
    assert_eq!(rig.boot(), "");
    let asked = rig.asked();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].starts_with("$vendor = '10DE'\n$device = '2BFE'\n"), "the script gets the numbers");
    assert!(asked[0].ends_with(SCRIPT_TEXT));
    let state = rig.state();
    assert_eq!(state.shown, Shown::Maker);
    assert_eq!(state.reason, "");
    assert_eq!(state.note, next_start("NVIDIA"));
    assert!(state.restarts_windows(), "the next start restarts Windows once");
    assert_eq!(shown_identity(&rig.context, &rig.machine), NVIDIA_RENDER);
}

#[test]
fn an_amd_machine_gets_the_package_for_amds_number() {
    let scratch = Scratch::new();
    let rig = Rig::on_amd(&scratch, &["identity staged\r\n"]);
    assert_eq!(rig.boot(), "");
    assert!(
        rig.asked()[0].starts_with("$vendor = '1002'\n$device = '7FFE'\n"),
        "the script gets AMD's numbers"
    );
    let state = rig.state();
    assert_eq!((state.shown, state.package_vendor), (Shown::Maker, 0x1002));
    assert_eq!(state.note, next_start("AMD"));
    assert_eq!(shown_identity(&rig.context, &rig.machine), AMD_RENDER);
}

#[test]
fn a_package_for_nvidias_number_on_an_amd_machine_is_made_again_for_amds() {
    let scratch = Scratch::new();
    let rig = Rig::on_amd(&scratch, &["identity staged\r\n"]);
    let old = IdentityState { shown: Shown::Maker, ..IdentityState::default() };
    write_state(&rig.context, &rig.machine, &old).unwrap();
    assert_eq!(
        shown_identity(&rig.context, &rig.machine),
        MICROSOFT_RENDER,
        "never a number with no package"
    );
    assert_eq!(rig.boot(), "");
    assert_eq!(rig.asked().len(), 1);
    assert_eq!(shown_identity(&rig.context, &rig.machine), AMD_RENDER);
}

#[test]
fn a_guest_that_keeps_microsofts_number_says_why_and_is_not_asked_again() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[REFUSED, STAGED]);
    assert_eq!(rig.boot(), "");
    let state = rig.state();
    assert_eq!(state.shown, Shown::Microsoft);
    assert_eq!(
        state.reason,
        "Windows keeps Microsoft's vendor ID. Windows' driver store has no copy of vrd.sys."
    );
    assert_eq!(state.note, "");
    assert!(!state.failed_script.is_empty());
    assert_eq!(rig.boot(), "");
    assert_eq!(rig.asked().len(), 1, "the same script isn't tried again");
    assert_eq!(rig.state(), state);
}

#[test]
fn a_guest_that_says_nothing_useful_keeps_microsofts_number_too() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &["pnputil said something else"]);
    assert_eq!(rig.boot(), "");
    let state = rig.state();
    assert_eq!(state.shown, Shown::Microsoft);
    assert_eq!(state.reason, did_not_stage("NVIDIA"));
}

#[test]
fn a_qemu_without_the_properties_keeps_microsofts_number_without_asking() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, false, &[STAGED]);
    assert_eq!(rig.boot(), "");
    assert_eq!(rig.asked().len(), 0);
    let state = rig.state();
    assert_eq!(state.shown, Shown::Microsoft);
    assert_eq!(state.reason, qemu_without("NVIDIA"));
}

#[test]
fn a_machine_on_nvidias_number_is_left_alone_and_its_note_goes() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[STAGED]);
    let state = IdentityState { shown: Shown::Maker, note: next_start("NVIDIA"), ..IdentityState::default() };
    write_state(&rig.context, &rig.machine, &state).unwrap();
    assert_eq!(rig.boot(), "");
    assert_eq!(rig.asked().len(), 0);
    assert_eq!(rig.state(), IdentityState { shown: Shown::Maker, ..IdentityState::default() });
}

#[test]
fn a_failed_gpu_start_on_nvidias_number_goes_back_to_microsofts() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[]);
    let host = RecordingHost::default();
    let task = Task::new(&rig.context, &host, "start", rig.machine.clone());
    let same = after_failed_gpu(&task, Failure::new("no GPU"));
    assert_eq!(same.0, "no GPU", "a machine on Microsoft's number has nothing to go back to");
    let state = IdentityState { shown: Shown::Maker, restart_due: true, ..IdentityState::default() };
    write_state(&rig.context, &rig.machine, &state).unwrap();
    let said = after_failed_gpu(&task, Failure::new("no GPU"));
    assert_eq!(said.0, format!("no GPU {FELL_BACK}"));
    let state = rig.state();
    assert_eq!(state.shown, Shown::Microsoft);
    assert_eq!(state.reason, FELL_BACK);
    assert!(!state.restart_due);
}

#[test]
fn a_package_that_cost_a_start_is_not_staged_again() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[STAGED]);
    let host = RecordingHost::default();
    let task = Task::new(&rig.context, &host, "start", rig.machine.clone());
    let state = IdentityState { shown: Shown::Maker, restart_due: true, ..IdentityState::default() };
    write_state(&rig.context, &rig.machine, &state).unwrap();
    after_failed_gpu(&task, Failure::new("no GPU"));
    assert_eq!(rig.boot(), "");
    assert!(rig.asked().is_empty(), "the next start stays on Microsoft's number");
    assert_eq!(shown_identity(&rig.context, &rig.machine), MICROSOFT_RENDER);
}

#[test]
fn a_build_without_the_script_keeps_microsofts_number_and_says_so() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[STAGED]);
    fs::remove_file(rig.context.paths.guest_files().join("gpu-identity.ps1")).unwrap();
    assert_eq!(rig.boot(), "");
    assert_eq!(rig.asked().len(), 0);
    assert_eq!(rig.state().reason, no_script("NVIDIA"));
}

#[test]
fn a_new_package_carries_the_card_drivers_version() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[STAGED]).with_card_driver(CARD_DRIVER);
    assert!(!rig.boot_says_restart(), "the VM starts again for the number, which is restart enough");
    let sent = "$vendor = '10DE'
$device = '2BFE'
$driverVersion = '32.0.16.1742'
";
    assert!(rig.asked()[0].starts_with(sent));
    assert_eq!(rig.state().package_version, CARD_DRIVER);
}

#[test]
fn with_no_card_driver_to_read_the_package_keeps_windows_own_version() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[STAGED]);
    assert_eq!(rig.boot(), "");
    assert!(rig.asked()[0].contains(
        "$driverVersion = ''
"
    ));
    assert_eq!(rig.state().package_version, "");
}

#[test]
fn a_machine_whose_package_has_another_version_gets_a_new_one_and_restarts_windows() {
    let scratch = Scratch::new();
    let rig = Rig::new(&scratch, true, &[UPDATED]).with_card_driver(CARD_DRIVER).on_makers_number("");
    assert!(rig.boot_says_restart());
    assert_eq!(rig.asked().len(), 1);
    assert!(rig.asked()[0].contains(
        "$driverVersion = '32.0.16.1742'
"
    ));
    let state = rig.state();
    assert_eq!((state.shown, state.package_version.as_str()), (Shown::Maker, CARD_DRIVER));
    assert!(state.restarts_windows());
    assert!(!rig.boot_says_restart(), "the next start has nothing to change");
    assert_eq!(rig.asked().len(), 1);
}

#[test]
fn a_new_package_no_device_was_running_on_needs_no_restart() {
    let scratch = Scratch::new();
    let rig =
        Rig::new(&scratch, true, &[STAGED]).with_card_driver(CARD_DRIVER).on_makers_number("32.0.15.9186");
    assert!(!rig.boot_says_restart());
    let state = rig.state();
    assert_eq!(state.package_version, CARD_DRIVER);
    assert!(!state.restarts_windows());
}

#[test]
fn a_guest_that_wont_take_the_new_version_keeps_its_number_and_is_not_asked_again() {
    let scratch = Scratch::new();
    let rig =
        Rig::new(&scratch, true, &[REFUSED, UPDATED]).with_card_driver(CARD_DRIVER).on_makers_number("");
    assert!(!rig.boot_says_restart());
    let state = rig.state();
    assert_eq!((state.shown, state.package_version.as_str()), (Shown::Maker, ""));
    assert_eq!(state.reason, "", "the machine works as it did, so the row has nothing to say");
    assert!(!rig.boot_says_restart());
    assert_eq!(rig.asked().len(), 1);
    assert_eq!(shown_identity(&rig.context, &rig.machine), NVIDIA_RENDER);
}

#[test]
fn only_a_plain_version_goes_into_the_script() {
    assert!(is_version("32.0.16.1742"));
    assert!(is_version("24.21.13.9924"));
    for odd in ["", "32.0.16", "32.0.16.1742.1", "32.0.16.x", "32.0.16.'; calc", "32.0.16.123456", "32..16.1"]
    {
        assert!(!is_version(odd), "{odd}");
    }
}
