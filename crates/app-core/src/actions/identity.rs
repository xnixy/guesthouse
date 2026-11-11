//! The card maker's vendor number going into a machine. A start on Microsoft's number has Windows
//! write the driver package for the maker's, and then starts the VM again to show it (see identity_switch.rs),
//! restarting Windows once on the way (see identity_restart.rs). Nothing here can stop a machine: a
//! guest that won't take the package keeps Microsoft's number, and the row says why.
//!
//! The package also gives the device the card driver's version. Some games read the device's driver
//! version and refuse one that's too old, and Windows' own render driver has a version of Windows'.
//! So a machine already on its maker's number gets a new package when the card driver's version isn't
//! the one its package carries, and Windows restarts once to take it.

use super::Task;
use crate::agent::guest_script;
use crate::failure::{Failure, Result};
use crate::gpu_identity::{IdentityState, Shown, maker_identity, maker_name, read_state, write_state};
use crate::gpu_support::qemu_sets_identity;
use crate::log::write_log;
use crate::settings::driver_folder;
use crate::setup::amd::amd_driver_version;
use crate::setup::driver::read_inf;
use crate::setup::inf::inf_driver_version;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const SCRIPT: &str = "gpu-identity.ps1";
/// Making and signing the package took about 20 seconds in a guest, and this is what the agent gets.
const STAGE_SECONDS: u64 = 110;
const STAGED_MARK: &str = "identity staged";
/// The guest put the new package on a device that was running on an older one.
const UPDATED_MARK: &str = "identity updated";
const STAY_MARK: &str = "stay: ";
/// A Windows driver version is four numbers, each of which fits in 16 bits.
const VERSION_PARTS: usize = 4;
const VERSION_PART_DIGITS: usize = 5;

const FELL_BACK: &str = "The next start shows Windows Microsoft's vendor ID again.";

fn qemu_without(maker: &str) -> String {
    format!("This QEMU can't show Windows {maker}'s vendor ID, so Windows sees Microsoft's.")
}

fn no_script(maker: &str) -> String {
    format!("This build has no script for {maker}'s vendor ID, so Windows sees Microsoft's.")
}

fn did_not_stage(maker: &str) -> String {
    format!(
        "Windows could not make the driver package for {maker}'s vendor ID, so it keeps Microsoft's. \
         The log has what Windows said."
    )
}

fn next_start(maker: &str) -> String {
    format!("Windows sees Microsoft's vendor ID until its next start, when it will see {maker}'s.")
}

/// Tells one version of the guest script from another, and one driver version from another, so
/// what failed isn't run again as it was.
fn script_id(script: &str, version: &str) -> String {
    let mut hasher = DefaultHasher::new();
    script.hash(&mut hasher);
    version.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Whether this is a Windows driver version and nothing else, as it goes into a script.
fn is_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    let number = |part: &&str| {
        !part.is_empty() && part.len() <= VERSION_PART_DIGITS && part.bytes().all(|b| b.is_ascii_digit())
    };
    parts.len() == VERSION_PARTS && parts.iter().all(number)
}

/// The card driver's version, which the package gives the device. Empty when it can't be read, and
/// the package then keeps the version of Windows' own driver.
fn card_driver_version(task: &Task) -> String {
    let found = if task.machine.windows_amd_card().is_some() {
        amd_driver_version(task.context)
    } else {
        read_inf(&driver_folder(task.context)).and_then(|inf| inf_driver_version(&inf)).ok()
    };
    found.filter(|version| is_version(version)).unwrap_or_default()
}

/// What the guest is sent: the numbers, the version, and the script that uses them.
fn script_with_values(task: &Task, script: &str, version: &str) -> String {
    let identity = maker_identity(&task.machine);
    format!(
        "$vendor = '{:04X}'\n$device = '{:04X}'\n$driverVersion = '{version}'\n{script}",
        identity.vendor, identity.device
    )
}

/// Has Windows write the package, and records what came of it.
fn stage(task: &Task, state: &mut IdentityState, script: &str, version: &str) -> Result<()> {
    let maker = maker_name(&task.machine);
    let said = task.ask_guest(&script_with_values(task, script, version), STAGE_SECONDS);
    write_log(task.context, &format!("gpu identity: {}", said.trim()))?;
    if said.lines().any(|line| line.trim() == STAGED_MARK) {
        state.shown = Shown::Maker;
        state.package_vendor = maker_identity(&task.machine).vendor;
        state.package_version = version.to_owned();
        state.failed_script.clear();
        state.reason.clear();
        state.note = next_start(maker);
        state.restart_due = true;
        return Ok(());
    }
    state.failed_script = script_id(script, version);
    let why = said.lines().find_map(|line| line.trim().strip_prefix(STAY_MARK));
    state.reason = match why {
        Some(why) => format!("Windows keeps Microsoft's vendor ID. {why}"),
        None => did_not_stage(maker),
    };
    Ok(())
}

/// Gives a machine already on its maker's number a package with the card driver's version, when the
/// one it holds has another. Says whether Windows now has to restart to take it. A guest that won't
/// take the new package keeps the old one and its number, and only the log says so.
fn update_version(task: &Task, state: &mut IdentityState) -> Result<bool> {
    let version = card_driver_version(task);
    if version.is_empty() || version == state.package_version {
        return Ok(false);
    }
    if !qemu_sets_identity(&task.context.paths.qemu) {
        return Ok(false);
    }
    let Ok(script) = guest_script(task.context, SCRIPT) else { return Ok(false) };
    let id = script_id(&script, &version);
    if state.failed_script == id {
        return Ok(false);
    }
    task.step(&format!("Giving the GPU its driver's version, {version}"));
    let said = task.ask_guest(&script_with_values(task, &script, &version), STAGE_SECONDS);
    write_log(task.context, &format!("gpu identity: {}", said.trim()))?;
    let marked = |mark: &str| said.lines().any(|line| line.trim() == mark);
    if !marked(STAGED_MARK) {
        state.failed_script = id;
        return Ok(false);
    }
    state.package_version = version;
    state.failed_script.clear();
    state.restart_due = marked(UPDATED_MARK);
    Ok(state.restart_due)
}

/// Says whether Windows has to restart now, for a package it took under a device that's running.
fn set_up(task: &Task) -> Result<bool> {
    let mut state = read_state(task.context, &task.machine);
    state.note.clear();
    if state.shown == Shown::Maker {
        let restart = update_version(task, &mut state)?;
        write_state(task.context, &task.machine, &state)?;
        return Ok(restart);
    }
    set_up_first(task, &mut state)?;
    write_state(task.context, &task.machine, &state)?;
    Ok(false)
}

/// The move from Microsoft's number to the maker's.
fn set_up_first(task: &Task, state: &mut IdentityState) -> Result<()> {
    let maker = maker_name(&task.machine);
    task.step(&format!("Preparing {maker}'s vendor ID"));
    if !qemu_sets_identity(&task.context.paths.qemu) {
        state.reason = qemu_without(maker);
        return Ok(());
    }
    let Ok(script) = guest_script(task.context, SCRIPT) else {
        state.reason = no_script(maker);
        return Ok(());
    };
    let version = card_driver_version(task);
    if state.failed_script != script_id(&script, &version) {
        stage(task, state, &script, &version)?;
    }
    Ok(())
}

/// What each boot does, once the GPU and the desktop are up. Whatever goes wrong here goes in the log
/// and on the row, and can't reach the code that turns off a VM whose start has failed. Says whether
/// Windows has to restart now (see identity_restart.rs).
pub(super) fn set_up_identity(task: &mut Task) -> bool {
    match set_up(task) {
        Ok(restart) => restart,
        Err(failure) => {
            let said =
                format!("{}'s vendor ID could not be prepared: {}", maker_name(&task.machine), failure.0);
            let _ = write_log(task.context, &said);
            task.error = said;
            false
        }
    }
}

/// Windows started without its GPU. If it was shown its maker's number, that's the first suspect, and
/// the next start goes back to the one that always works.
pub(super) fn after_failed_gpu(task: &Task, failure: Failure) -> Failure {
    let mut state = read_state(task.context, &task.machine);
    if state.shown != Shown::Maker {
        return failure;
    }
    state.shown = Shown::Microsoft;
    state.restart_due = false;
    state.note.clear();
    state.reason = FELL_BACK.to_owned();
    // Staging the same package again would end the same way at every start, and no start would
    // ever finish. A new build's script is tried afresh.
    if let Ok(script) = guest_script(task.context, SCRIPT) {
        state.failed_script = script_id(&script, &card_driver_version(task));
    }
    if let Err(problem) = write_state(task.context, &task.machine, &state) {
        let _ = write_log(task.context, &format!("gpu identity: could not fall back: {}", problem.0));
    }
    Failure(format!("{} {FELL_BACK}", failure.0))
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
