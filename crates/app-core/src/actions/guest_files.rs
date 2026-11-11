//! Puts the driver's files in an existing guest's system folders. The driver's own folder reaches the
//! guest at install, but a program like a benchmark's launcher looks in System32 by name. The same for
//! NVIDIA's settings store, which the driver's installer would make and a game reads its profile from.

use super::Task;
use super::desktop::{GUEST_FOLDER, put_guest_file};
use crate::agent::guest_script;
use crate::failure::Result;
use crate::log::write_log;
use crate::setup::guest_driver::INTEL_STORE_FOLDER;
use crate::setup::guest_files::guest_file_list;
use crate::setup::intel::{SYSTEM_LIST, intel_driver_folder};
use std::fs;
use crate::text::base64_encode;

const SCRIPT_NAME: &str = "nv-files.ps1";
const PROFILES_SCRIPT: &str = "nv-profiles.ps1";
const PROFILES_SECONDS: u64 = 30;
const LIST_NAME: &str = "nv-files.txt";
const LOG_NAME: &str = "nv-files.log";
const CHECK_SECONDS: u64 = 20;
const PLACE_SECONDS: u64 = 110;
const MISSING_MARK: &str = "missing ";
const FAILED_MARK: &str = "FAILED";
const NO_SCRIPT: &str = "Windows lacks some of the driver's files, so a program that looks for them may crash. \
                         This install has no script to place them.";

fn guest_path(name: &str) -> String {
    format!("{GUEST_FOLDER}\\{name}")
}

fn send_list(task: &Task, list: &str) {
    let data = base64_encode(list.as_bytes());
    let path = guest_path(LIST_NAME);
    task.ask_guest(
        &format!("[IO.File]::WriteAllBytes('{path}', [Convert]::FromBase64String('{data}'))"),
        CHECK_SECONDS,
    );
}

/// The guest counts for itself, as only it knows what is in its folders.
fn count_missing(task: &Task) -> Option<u32> {
    let list = guest_path(LIST_NAME);
    let script = format!(
        "$count = 0; foreach ($line in Get-Content -LiteralPath '{list}') {{ \
         $folder, $name, $target = $line -split \"`t\"; \
         if (-not (Test-Path -LiteralPath (Join-Path (Join-Path $env:windir $folder) $target))) {{ $count++ }} }}; \
         \"{MISSING_MARK}$count\""
    );
    let said = task.ask_guest(&script, CHECK_SECONDS);
    said.split(MISSING_MARK).nth(1)?.trim().parse().ok()
}

/// `store` names the driver's folder in the guest's driver store, where NVIDIA's is the script's own.
fn run_placing_script(task: &Task, store: Option<&str>) -> String {
    let (script, list, log) = (guest_path(SCRIPT_NAME), guest_path(LIST_NAME), guest_path(LOG_NAME));
    let store = store.map(|name| format!(" -StoreFolder {name}")).unwrap_or_default();
    task.ask_guest(&format!("& {script} {list}{store} | Tee-Object {log}"), PLACE_SECONDS)
}

/// The folder NVIDIA's settings store lives in, with the driver's own profile database in it. A guest
/// that has it keeps it, as the files change once a program has used them.
pub(super) fn place_profile_store(task: &mut Task) -> Result<()> {
    let script = match guest_script(task.context, PROFILES_SCRIPT) {
        Ok(script) => script,
        Err(failure) => {
            write_log(task.context, &format!("profile store: {}", failure.0))?;
            return Ok(());
        }
    };
    let said = task.ask_guest(&script, PROFILES_SECONDS);
    if said.contains(FAILED_MARK) {
        task.error = "NVIDIA's settings store did not reach Windows. The log says why.".to_owned();
    }
    if !said.trim().starts_with("kept") {
        write_log(task.context, &format!("profile store: {}", said.trim()))?;
    }
    Ok(())
}

/// Windows is up by now, so anything that goes wrong here is said and doesn't stop the machine.
pub(super) fn place_driver_files(task: &mut Task) -> Result<()> {
    let list = match guest_file_list(task.context) {
        Ok(list) => list,
        Err(failure) => {
            write_log(task.context, &format!("driver files: {}", failure.0))?;
            return Ok(());
        }
    };
    place_listed_files(task, &list, None)
}

/// What Intel's INF puts in the system folders, from the list unpacked with its driver: the loaders
/// the guest's programs find Intel's OpenCL, Vulkan and Level Zero drivers through. A guest that
/// has them all is left alone.
pub(super) fn place_system_files(task: &mut Task) -> Result<()> {
    let list = intel_driver_folder(task.context).and_then(|folder| fs::read_to_string(folder.join(SYSTEM_LIST)).ok());
    match list.filter(|list| !list.trim().is_empty()) {
        Some(list) => place_listed_files(task, &list, Some(INTEL_STORE_FOLDER)),
        None => {
            write_log(task.context, "driver files: Intel's driver has no list of system files")?;
            Ok(())
        }
    }
}

fn place_listed_files(task: &mut Task, list: &str, store: Option<&str>) -> Result<()> {
    task.step("Checking the driver's files");
    send_list(task, list);
    if count_missing(task).is_none_or(|count| count == 0) {
        return Ok(());
    }
    if !task.context.paths.guest_files().join(SCRIPT_NAME).exists() {
        write_log(task.context, "driver files: this program folder has no script to place them")?;
        task.error = NO_SCRIPT.to_owned();
        return Ok(());
    }
    task.step("Placing the driver's files");
    put_guest_file(task, SCRIPT_NAME, &guest_path(SCRIPT_NAME))?;
    let said = run_placing_script(task, store);
    write_log(task.context, &format!("driver files: {}", said.trim()))?;
    if said.contains(FAILED_MARK) {
        task.error = "Some of the driver's files did not reach Windows. The log says which.".to_owned();
    }
    Ok(())
}
