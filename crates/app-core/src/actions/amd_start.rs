//! An Ubuntu guest on an AMD card builds its Mesa for native context the first time it boots on it,
//! and each time Ubuntu updates Mesa. The start waits until the guest says how its GPU is: if native
//! context failed at once, the VM is started again on Venus, which needs no build, and the row says why.
//! A build takes the best part of an hour, so the start doesn't wait through one, which would leave the
//! machine with nothing but Force Off: the row shows its progress from the guest's serial port, and a
//! build that fails puts the next start on Venus.

use super::{KEY_POLL, STOP_WAIT_SECONDS, Task};
use crate::failure::Result;
use crate::linux::amd::AmdMode;
use crate::linux::amd_says::{self, AmdSays, learn};
use crate::linux::guest_says;
use crate::vm_args::VmPlan;
use std::time::Duration;

/// The first boot's packages fit in this, before the guest says anything.
const SAY_MINUTES: u64 = 30;
/// A guest from before AMD cards says only that it has no NVIDIA GPU. It's given this long after that
/// to say anything more.
const OLD_GUEST_SECONDS: u64 = 120;
const WAITING: &str = "Waiting for the guest to set up its AMD GPU";
pub(super) const STARTING_ON_VENUS: &str = "Starting the VM again, on Venus";
const OLD_GUEST: &str = "This guest's own scripts don't know AMD cards, so its desktop runs on the plain \
                         virtio GPU, with no 3D on the card. A new Ubuntu machine has them.";

/// What came of waiting.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Waited {
    /// It has its GPU, or is building Mesa for it, or the VM went, or the guest is too old to say:
    /// nothing more to do here.
    Done,
    /// Native context failed, and the VM has to start again on Venus.
    OnVenusNext(String),
}

/// Waits until the guest says how its GPU went, or that it's building Mesa for it.
pub(super) fn wait_for_guest(task: &mut Task, plan: &VmPlan) -> Waited {
    let Some(amd) = plan.amd_boot() else { return Waited::Done };
    let mode = amd.mode;
    task.step(WAITING);
    let end = task.now() + Duration::from_secs(SAY_MINUTES * 60);
    let mut quiet_since: Option<Duration> = None;
    while task.pid().is_some() && task.now() < end {
        let says = amd_says::read(&plan.run_folder);
        if let Some(says) = &says {
            learn(task.context, &task.machine, mode, says);
            match says {
                AmdSays::Failed { reason } if mode == AmdMode::Native => {
                    return Waited::OnVenusNext(reason.clone());
                }
                _ => return Waited::Done,
            }
        } else if guest_says::read(&plan.run_folder).is_some() {
            let since = *quiet_since.get_or_insert(task.now());
            if task.now() >= since + Duration::from_secs(OLD_GUEST_SECONDS) {
                task.error = OLD_GUEST.to_owned();
                return Waited::Done;
            }
        }
        task.sleep(KEY_POLL);
    }
    Waited::Done
}

/// Stops the VM that failed on native context, so that the next one starts on Venus.
pub(super) fn stop_for_venus(task: &mut Task, reason: &str) -> Result<()> {
    task.step(STARTING_ON_VENUS);
    task.host.stop(&task.machine, STOP_WAIT_SECONDS)?;
    task.error = format!("Native context failed in the guest, so it's on Venus now: {reason}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::amd::{AmdCard, amd_mode};
    use crate::machine::{Kind, Machine};
    use crate::testing::{Scratch, scratch_context};
    use crate::vm_args::{AmdBoot, Boot, UbuntuBoot};
    use crate::vm_host::stand_in::RecordingHost;
    use std::fs;

    fn plan(task: &Task, mode: AmdMode) -> VmPlan {
        let card = AmdCard {
            pci: "0000:7c:00.0".into(),
            render_node: "/dev/dri/renderD128".into(),
            name: "AMD".into(),
        };
        let boot = Boot::Ubuntu(UbuntuBoot::Amd(AmdBoot { card, mode, setup_image: None }));
        super::super::plan_for(task.context, &task.machine, boot)
    }

    fn machine(context: &crate::context::Context) -> Machine {
        let machine =
            Machine { kind: Kind::Linux, amd_gpu: "0000:7c:00.0".into(), ..Machine::new("u", "U", "/d", "") };
        fs::create_dir_all(machine.folder(context)).unwrap();
        machine
    }

    #[test]
    fn a_vm_that_has_gone_is_not_waited_for() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine(&context));
        let wanted = plan(&task, AmdMode::Native);
        assert_eq!(wait_for_guest(&mut task, &wanted), Waited::Done);
    }

    #[test]
    fn a_guest_building_mesa_is_left_to_it_and_one_that_says_nothing_is_not_waited_on_forever() {
        let scratch = Scratch::new();
        let context = super::super::tests::plain_task_context(&scratch);
        super::super::tests::with_vm_running(&context);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine(&context));
        let wanted = plan(&task, AmdMode::Native);
        let started = task.now();
        assert_eq!(wait_for_guest(&mut task, &wanted), Waited::Done);
        assert!(task.now() - started >= Duration::from_secs(SAY_MINUTES * 60), "it gave up in time");
        fs::write(wanted.run_folder.join("serial.log"), "guesthouse-amd building 5 5803\n").unwrap();
        let started = task.now();
        assert_eq!(wait_for_guest(&mut task, &wanted), Waited::Done);
        assert_eq!(task.now(), started, "no wait through the build");
        assert_eq!(amd_mode(&context, &task.machine), AmdMode::Native);
    }

    #[test]
    fn a_failure_moves_the_machine_to_venus() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine(&context));
        learn(
            &context,
            &task.machine,
            AmdMode::Native,
            &AmdSays::Failed { reason: "the build failed".into() },
        );
        stop_for_venus(&mut task, "the build failed").unwrap();
        assert_eq!(host.said(), ["stop 100"]);
        assert!(task.error.contains("on Venus now: the build failed"), "{}", task.error);
        assert_eq!(amd_mode(&context, &task.machine), AmdMode::Venus);
    }
}
