//! Whether Windows really got its GPU. A guest whose GPU failed to start boots all the same, on the
//! basic display, so nothing else in a start would notice.

use super::Task;
use crate::daemon;
use crate::failure::{Failure, Result};
use crate::gpu_identity::{MICROSOFT_RENDER, identity_of};
use crate::gpu_maker::GpuMaker;
use std::fs;
use std::time::Duration;

const WORKING_MARK: &str = "gpu working ";
/// A device that's still starting gets a moment. One that has failed stays that way.
const CHECK_ATTEMPTS: u32 = 10;
const CHECK_POLL: Duration = Duration::from_secs(3);
const CHECK_ANSWER_SECONDS: u64 = 15;
const NO_GPU: &str = "Windows started without its GPU, so the VM was turned off.";
const DAEMON_ERROR_MARK: &str = "ERROR ";

/// The virtual render device, under any identity the QEMU patch can give it.
fn is_render_device() -> String {
    let identities = std::iter::once(MICROSOFT_RENDER).chain(GpuMaker::ALL.map(identity_of));
    let patterns =
        identities.map(|identity| format!("$_.PNPDeviceID -like '{}'", identity.windows_pattern()));
    patterns.collect::<Vec<_>>().join(" -or ")
}

fn working_devices(task: &Task) -> Option<u32> {
    let script = format!(
        "$found = @(Get-CimInstance Win32_VideoController | \
         Where-Object {{ ({}) -and $_.ConfigManagerErrorCode -eq 0 }}); \
         \"{WORKING_MARK}$($found.Count)\"",
        is_render_device()
    );
    let said = task.ask_guest(&script, CHECK_ANSWER_SECONDS);
    said.split(WORKING_MARK).nth(1)?.trim().parse().ok()
}

/// The daemon's last error, which is the most likely reason.
fn daemons_reason(task: &Task) -> Option<String> {
    let said = fs::read_to_string(task.run_file(daemon::OUTPUT_FILE)).ok()?;
    let last = said.lines().rev().find_map(|line| line.strip_prefix(DAEMON_ERROR_MARK))?;
    Some(last.trim().to_owned())
}

pub(super) fn check_gpu(task: &Task) -> Result<()> {
    task.step("Checking the GPU");
    for attempt in 0..CHECK_ATTEMPTS {
        if attempt > 0 {
            task.sleep(CHECK_POLL);
        }
        if working_devices(task).is_some_and(|count| count > 0) {
            return Ok(());
        }
    }
    Err(Failure(match daemons_reason(task) {
        Some(reason) => format!("{NO_GPU} The daemon said: {reason}"),
        None => NO_GPU.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_identity::{AMD_RENDER, INTEL_RENDER, NVIDIA_RENDER};

    #[test]
    fn the_check_accepts_the_render_device_under_every_identity_a_vm_can_show() {
        let test = is_render_device();
        for identity in [MICROSOFT_RENDER, NVIDIA_RENDER, AMD_RENDER, INTEL_RENDER] {
            assert!(test.contains(&identity.windows_pattern()), "{identity:?}: {test}");
        }
    }
}
