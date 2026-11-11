//! AMD's driver files for a Windows guest on an AMD card. Setup puts together the folder the guest
//! gets, and the install disc takes it to the guest. A machine set up by an older version of the app
//! may lack files the daemon now names, on this PC and in the guest, so a start brings both up to date.

use super::Task;
use super::guest_upload::{put_big_file, quoted};
use crate::failure::Result;
use crate::gpu_maker::GpuMaker;
use crate::log::write_log;
use crate::setup::amd::complete_amd_driver;
use crate::setup::guest_driver::guest_driver;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Where Windows keeps a driver the host lends it, by the folder name the daemon gives.
const HOST_DRIVER_STORE: &str = "C:\\Windows\\System32\\HostDriverStore\\FileRepository";
/// The user-mode driver, which every guest set up for AMD got at install.
const USER_DRIVER: &str = "amdxc64.dll";
const LIST_SECONDS: u64 = 20;
const LISTED_MARK: &str = "all listed";
const PLACING: &str = "Giving Windows AMD's driver files it lacks";
const NOT_PLACED: &str = "Some of AMD's driver files did not reach Windows, so Vulkan or OpenGL may not work. The \
                          log says which, and the next start tries again.";

/// Any problem is said and doesn't stop the task: Direct3D works without what's missing.
pub(super) fn complete_host_driver(task: &mut Task) -> Result<()> {
    if task.machine.windows_maker() != Some(GpuMaker::Amd) {
        return Ok(());
    }
    if let Some(problem) = complete_amd_driver(task.context) {
        write_log(task.context, &format!("AMD driver: {problem}"))?;
        task.error = problem;
    }
    Ok(())
}

/// The names of the files in the guest's copy of the driver, in lower case as Windows ignores case,
/// or `None` if the guest didn't say.
fn guest_names(task: &Task, store: &str) -> Option<HashSet<String>> {
    let script = format!(
        "Get-ChildItem -LiteralPath {} -File -ea 0 | ForEach-Object Name; '{LISTED_MARK}'",
        quoted(store)
    );
    let said = task.ask_guest(&script, LIST_SECONDS);
    let lines: Vec<&str> = said.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let (last, names) = lines.split_last()?;
    (*last == LISTED_MARK).then(|| names.iter().map(|name| name.to_lowercase()).collect())
}

/// The files of the driver's folder on this PC that the guest's copy lacks.
fn missing_in_guest(folder: &Path, in_guest: &HashSet<String>) -> Result<Vec<PathBuf>> {
    let mut missing: Vec<PathBuf> = fs::read_dir(folder)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            !in_guest.contains(&name)
        })
        .collect();
    missing.sort();
    Ok(missing)
}

/// Brings the driver's folder on this PC up to date, then copies into the guest what its copy lacks.
/// Windows is up by now, so what goes wrong is said and the machine stays up.
pub(super) fn bring_amd_driver_up_to_date(task: &mut Task) -> Result<()> {
    complete_host_driver(task)?;
    let Ok(driver) = guest_driver(task.context, &task.machine) else { return Ok(()) };
    let store = format!("{HOST_DRIVER_STORE}\\{}", driver.store_folder);
    let Some(in_guest) = guest_names(task, &store) else {
        write_log(task.context, "AMD driver: the guest didn't list its driver files")?;
        return Ok(());
    };
    // A guest without the driver itself didn't get it at install, and copying all of it this way
    // would take long. Installing again is the way there.
    if !in_guest.contains(USER_DRIVER) {
        write_log(task.context, "AMD driver: the guest has no copy of the driver to complete")?;
        return Ok(());
    }
    let missing = missing_in_guest(&driver.folder, &in_guest)?;
    if missing.is_empty() {
        return Ok(());
    }
    let mut failed = Vec::new();
    for (done, local) in missing.iter().enumerate() {
        task.step(&format!("{PLACING}, {} of {}", done + 1, missing.len()));
        let name = local.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if !put_big_file(task, local, &format!("{store}\\{name}"))? {
            failed.push(name);
        }
    }
    let placed = missing.len() - failed.len();
    write_log(task.context, &format!("AMD driver: gave Windows {placed} files it lacked"))?;
    if !failed.is_empty() {
        write_log(task.context, &format!("AMD driver: these did not reach Windows: {}", failed.join(", ")))?;
        task.error = NOT_PLACED.to_owned();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::guest_upload::stand_in::FilesGuest;
    use crate::actions::tests::plain_task_context;
    use crate::context::Context;
    use crate::machine::Machine;
    use crate::setup::amd::write_amd_setup;
    use crate::setup::amd_guest_folder::add_components;
    use crate::setup::amd_guest_folder::stand_in::lay_out_driver;
    use crate::testing::{Scratch, scratch_context};
    use crate::vm_host::stand_in::RecordingHost;
    use std::fs;
    use std::sync::Arc;

    const STORE: &str = "C:\\Windows\\System32\\HostDriverStore\\FileRepository\\amdxc.inf_amd64_gpupv";

    /// A machine on an AMD card set up on 0.4.0 with a newer card, and a guest that has what the
    /// install disc gave it then: the user-mode driver's folder alone, in `names`.
    fn set_up_on_0_4(
        scratch: &Scratch,
        names: impl Fn(&Path) -> String,
    ) -> (Context, Arc<FilesGuest>, Machine) {
        let mut context = plain_task_context(scratch);
        lay_out_driver(scratch, "amd/WT6A_INF", "B026716", false);
        let folder = scratch.path("amd/WT6A_INF/B026716");
        write_amd_setup(&context, &folder, "32.0.32015.12012").unwrap();
        let guest = Arc::new(FilesGuest::default());
        *guest.other_answer.lock().unwrap() = names(&folder);
        context.agent = guest.clone();
        let machine = Machine { amd_gpu: "0000:7c:00.0".into(), ..crate::actions::tests::machine(&context) };
        crate::agent::make_token(&context, &machine).unwrap();
        (context, guest, machine)
    }

    /// The names in `folder` as the guest lists them, with Windows' line ends and its own case.
    fn listing(folder: &Path) -> String {
        let mut said = String::new();
        for entry in fs::read_dir(folder).unwrap().flatten() {
            said += &format!("{}\r\n", entry.file_name().to_string_lossy().to_uppercase());
        }
        said + LISTED_MARK + "\r\n"
    }

    #[test]
    fn a_guest_set_up_on_0_4_gets_vulkan_and_opengl_and_nothing_it_has() {
        let scratch = Scratch::new();
        let (context, guest, machine) = set_up_on_0_4(&scratch, listing);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine);
        bring_amd_driver_up_to_date(&mut task).unwrap();
        assert_eq!(task.error, "");
        let files = guest.files.lock().unwrap();
        let placed: Vec<&str> = files.keys().map(|path| path.strip_prefix(STORE).unwrap()).collect();
        let wanted = [
            "\\amd-vulkan32.json",
            "\\amd-vulkan64.json",
            "\\amdvlk32.dll",
            "\\amdvlk64.dll",
            "\\atio6axx.dll",
            "\\atioglxx.dll",
            "\\vulkan64.dll",
        ];
        assert_eq!(placed, wanted);
        let local = fs::read(scratch.path("amd/WT6A_INF/amdvlk/amdvlk64.dll")).unwrap();
        assert!(files[&format!("{STORE}\\amdvlk64.dll")] == local);
    }

    #[test]
    fn a_guest_that_has_everything_is_only_asked_for_its_list() {
        let scratch = Scratch::new();
        let (context, guest, machine) = set_up_on_0_4(&scratch, |folder| {
            add_components(folder).unwrap();
            listing(folder)
        });
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine);
        bring_amd_driver_up_to_date(&mut task).unwrap();
        assert_eq!((task.error.as_str(), guest.requests.lock().unwrap().len()), ("", 1));
    }

    #[test]
    fn a_guest_that_does_not_list_its_files_gets_nothing_sent() {
        let scratch = Scratch::new();
        let (context, guest, machine) = set_up_on_0_4(&scratch, |_| String::new());
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine);
        bring_amd_driver_up_to_date(&mut task).unwrap();
        assert_eq!((task.error.as_str(), guest.requests.lock().unwrap().len()), ("", 1));
    }

    #[test]
    fn a_guest_without_the_driver_itself_is_not_sent_all_of_it() {
        let scratch = Scratch::new();
        let (context, guest, machine) = set_up_on_0_4(&scratch, |_| format!("{LISTED_MARK}\r\n"));
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine);
        bring_amd_driver_up_to_date(&mut task).unwrap();
        assert_eq!((task.error.as_str(), guest.requests.lock().unwrap().len()), ("", 1));
    }

    #[test]
    fn a_file_that_does_not_arrive_is_said_and_the_start_goes_on() {
        let scratch = Scratch::new();
        let (context, guest, machine) = set_up_on_0_4(&scratch, listing);
        *guest.pieces_allowed.lock().unwrap() = Some(3);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", machine);
        bring_amd_driver_up_to_date(&mut task).unwrap();
        assert_eq!(task.error, NOT_PLACED);
    }

    #[test]
    fn a_machine_on_amd_gets_its_driver_completed_and_one_on_nvidia_is_left_alone() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        lay_out_driver(&scratch, "amd/WT6A_INF", "B026716", false);
        let folder = scratch.path("amd/WT6A_INF/B026716");
        write_amd_setup(&context, &folder, "32.0.32015.12012").unwrap();
        let host = RecordingHost::default();
        let windows = Machine::new("w", "W", "/d", "/v");
        complete_host_driver(&mut Task::new(&context, &host, "start", windows.clone())).unwrap();
        assert!(!folder.join("atio6axx.dll").exists(), "not for a machine on NVIDIA");
        let on_amd = Machine { amd_gpu: "0000:7c:00.0".into(), ..windows };
        let mut task = Task::new(&context, &host, "start", on_amd);
        complete_host_driver(&mut task).unwrap();
        assert_eq!(task.error, "");
        assert!(folder.join("atio6axx.dll").is_file());
    }

    #[test]
    fn a_driver_that_still_lacks_files_is_said_and_the_task_goes_on() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        lay_out_driver(&scratch, "amd/WT6A_INF", "B026716", false);
        fs::remove_dir_all(scratch.path("amd/WT6A_INF/amdvlk")).unwrap();
        write_amd_setup(&context, &scratch.path("amd/WT6A_INF/B026716"), "32.0.32015.12012").unwrap();
        let host = RecordingHost::default();
        let on_amd = Machine { amd_gpu: "0000:7c:00.0".into(), ..Machine::new("w", "W", "/d", "/v") };
        let mut task = Task::new(&context, &host, "start", on_amd);
        complete_host_driver(&mut task).unwrap();
        assert!(task.error.contains("amd-vulkan64.json"), "{}", task.error);
    }
}
