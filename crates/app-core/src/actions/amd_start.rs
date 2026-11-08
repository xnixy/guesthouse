//! An Ubuntu guest on an AMD card builds its Mesa for native context the first time it boots on it,
//! and each time Ubuntu updates Mesa. The start waits on what the guest says, so the machine's row
//! shows how far the build is. If native context failed, the VM is started again on Venus, which needs
//! no build, and the row says why.

use super::{KEY_POLL, STOP_WAIT_SECONDS, Task};
use crate::failure::Result;
use crate::linux::amd::AmdMode;
use crate::linux::amd_says::{self, AmdSays, learn};
use crate::linux::guest_says;
use crate::vm_args::VmPlan;
use std::time::Duration;

/// The first boot's packages and the build both fit in this, as Ubuntu's install does.
const SETUP_HOURS: u64 = 2;
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
    /// It has its GPU, or the VM went, or the guest is too old to say: nothing more to do.
    Done,
    /// Native context failed, and the VM has to start again on Venus.
    OnVenusNext(String),
}

fn progress(says: &AmdSays) -> Option<String> {
    let AmdSays::Building { done, total } = says else { return None };
    Some(format!("Building Mesa for the AMD GPU in the guest: {done} of {total}"))
}

/// Waits until the guest says how its GPU went, showing the build's progress as it goes.
pub(super) fn wait_for_guest(task: &mut Task, plan: &VmPlan) -> Waited {
    let Some(amd) = plan.amd_boot() else { return Waited::Done };
    let mode = amd.mode;
    task.step(WAITING);
    let end = task.now() + Duration::from_secs(SETUP_HOURS * 3600);
    let mut quiet_since: Option<Duration> = None;
    let mut shown = String::new();
    while task.pid().is_some() && task.now() < end {
        let says = amd_says::read(&plan.run_folder);
        if let Some(says) = &says {
            learn(task.context, &task.machine, mode, says);
            if let Some(text) = progress(says).filter(|text| *text != shown) {
                task.step(&text);
                shown = text;
            }
            match says {
                AmdSays::Failed { reason } if mode == AmdMode::Native => {
                    return Waited::OnVenusNext(reason.clone());
                }
                _ if says.settled() => return Waited::Done,
                _ => {}
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
    fn the_builds_progress_is_shown_and_a_failure_moves_the_machine_to_venus() {
        assert_eq!(
            progress(&AmdSays::Building { done: 812, total: 2400 }).unwrap(),
            "Building Mesa for the AMD GPU in the guest: 812 of 2400"
        );
        assert_eq!(progress(&AmdSays::Native { renderer: "radeonsi".into() }), None);
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
