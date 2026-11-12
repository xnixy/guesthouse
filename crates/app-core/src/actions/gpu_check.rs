//! Whether Windows really got its GPU. A guest whose GPU failed to start boots all the same, on the
//! basic display, so nothing else in a start would notice.

use super::Task;
use crate::daemon;
use crate::failure::{Failure, Result};
use crate::gpu_identity::{AMD_RENDER, MICROSOFT_RENDER, NVIDIA_RENDER};
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
    let patterns = [MICROSOFT_RENDER, NVIDIA_RENDER, AMD_RENDER].map(|identity| identity.windows_pattern());
    patterns.map(|pattern| format!("$_.PNPDeviceID -like '{pattern}'")).join(" -or ")
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
    let last = without_time(last);
    Some(last.trim().to_owned())
}

/// A verbose log line has its time in brackets after the level. The reason reads the same without it.
fn without_time(message: &str) -> &str {
    let Some(rest) = message.strip_prefix('[') else { return message };
    match rest.split_once("] ") {
        Some((time, after))
            if !time.is_empty()
                && time.bytes().all(|byte| byte.is_ascii_digit() || byte == b' ' || byte == b'.')
                && time.matches('.').count() == 1 =>
        {
            after
        }
        _ => message,
    }
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
