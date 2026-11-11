//! The Windows driver: checking an installer, and taking its files out.

use super::inf::{inf_driver_version, latin1, retail_version};
use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::log::write_log;
use crate::settings::write_setup;
use crate::vm_args::on_path;
use rustix::process::{Pid, Signal, kill_process};
use serde_json::json;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The Windows driver this was built for.
pub const WINDOWS_DRIVER: &str = "617.42";
/// The file a driver folder is known by, which also says its version.
pub const INF_FILE: &str = "nv_dispi.inf";
pub const INSTALLER_FOLDER: &str = "Display.Driver";
const READ_SECONDS: &str = "120";
const OUTPUT_KEPT: usize = 4000;
const LOOKED_AT: usize = 200;
const CHUNK: usize = 4096;

pub fn read_inf(folder: &Path) -> Result<String> {
    Ok(latin1(&fs::read(folder.join(INF_FILE))?))
}

/// The driver's INFs, `nv_dispi.inf` first and then the others by name. Laptops' cards are only in
/// their makers' INFs.
pub fn read_infs(folder: &Path) -> Result<Vec<String>> {
    let mut others: Vec<String> = fs::read_dir(folder)?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.to_lowercase().ends_with(".inf") && !name.eq_ignore_ascii_case(INF_FILE))
        .collect();
    others.sort();
    let mut found = vec![read_inf(folder)?];
    for name in others {
        found.push(latin1(&fs::read(folder.join(name))?));
    }
    Ok(found)
}

/// The installer's driver version, from the INF inside it, as its name proves nothing.
pub fn read_installer(path: &str) -> Result<String> {
    if !on_path("7z") {
        return Err(Failure::new("Reading the installer needs 7z, which is not installed."));
    }
    let entry = format!("{INSTALLER_FOLDER}/{INF_FILE}");
    let result = Command::new("timeout").args([READ_SECONDS, "7z", "e", "-so", path, &entry]).output()?;
    if result.status.code() == Some(TIMED_OUT) {
        return Err(Failure::new("The installer took too long to read."));
    }
    if !result.status.success() || result.stdout.is_empty() {
        return Err(Failure::new("This file is not NVIDIA's driver installer."));
    }
    Ok(retail_version(&inf_driver_version(&latin1(&result.stdout))?))
}

/// What `timeout` exits with.
const TIMED_OUT: i32 = 124;

pub fn check_installer(path: &str) -> Result<()> {
    let version = read_installer(path)?;
    if version != WINDOWS_DRIVER {
        return Err(Failure(format!(
            "This is driver {version}. Guesthouse was built for {WINDOWS_DRIVER}, so get that version from NVIDIA."
        )));
    }
    Ok(())
}

/// Says what is wrong with a folder of the driver's files, or `None` if it's the right driver.
pub fn driver_problem(folder: &Path) -> Option<String> {
    if !fs::metadata(folder.join(INF_FILE)).is_ok_and(|found| found.is_file()) {
        return Some(format!("{INF_FILE} is missing from the driver's folder."));
    }
    let version =
        read_inf(folder).and_then(|text| inf_driver_version(&text)).map(|found| retail_version(&found));
    match version {
        Ok(version) if version == WINDOWS_DRIVER => None,
        Ok(version) => Some(format!("These files are driver {version}.")),
        Err(error) => Some(error.0),
    }
}

/// Takes the driver's files out of the installer. `run` blocks, and `cancel` can come from another
/// thread. It works beside the final folder, so stopping at any moment leaves that as it was.
pub struct Extraction {
    installer: String,
    process: Mutex<Option<u32>>,
    cancelled: AtomicBool,
}

impl Extraction {
    pub fn new(installer: &str) -> Self {
        Self { installer: installer.to_owned(), process: Mutex::new(None), cancelled: AtomicBool::new(false) }
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.stop_process();
    }

    fn stop_process(&self) {
        let pid = self.process.lock().ok().and_then(|pid| *pid).and_then(|pid| Pid::from_raw(pid as i32));
        if let Some(pid) = pid {
            let _ = kill_process(pid, Signal::TERM);
        }
    }

    /// 7z writes its percentage over itself, with backspaces, so lines are no use here.
    fn read_output(stream: &mut impl Read, progress: &(impl Fn(u32) + ?Sized)) -> String {
        let mut said = String::new();
        let mut chunk = [0; CHUNK];
        while let Ok(size) = stream.read(&mut chunk) {
            if size == 0 {
                break;
            }
            said.push_str(&String::from_utf8_lossy(&chunk[..size]));
            said = tail(&said, OUTPUT_KEPT).to_owned();
            if let Some(percent) = last_percentage(tail(&said, LOOKED_AT)) {
                progress(percent);
            }
        }
        said
    }

    /// Returns the folder holding the files. `progress` gets the percentage done.
    pub fn run(&self, context: &Context, progress: &(impl Fn(u32) + ?Sized)) -> Result<PathBuf> {
        let target = context.paths.setup_folder().join(format!("driver-{WINDOWS_DRIVER}"));
        let check = |unpacked: &Path| driver_problem(&unpacked.join(INSTALLER_FOLDER));
        self.unpack(context, INSTALLER_FOLDER, &target, &check, progress)?;
        let folder = target.join(INSTALLER_FOLDER);
        let changes = json!({"driver_version": WINDOWS_DRIVER, "driver_folder": folder});
        write_setup(context, changes.as_object().cloned().unwrap_or_default())?;
        Ok(folder)
    }

    /// Takes `entry` out of the installer into `target`, if `check` finds nothing wrong with what came
    /// out. Until then the files are beside `target`, which is only replaced at the end.
    pub(super) fn unpack(
        &self,
        context: &Context,
        entry: &str,
        target: &Path,
        check: &dyn Fn(&Path) -> Option<String>,
        progress: &(impl Fn(u32) + ?Sized),
    ) -> Result<()> {
        self.unpack_entries(context, &[entry], target, check, progress)
    }

    /// The same for several entries, which a driver whose files sit among many others needs.
    pub(super) fn unpack_entries(
        &self,
        context: &Context,
        entries: &[&str],
        target: &Path,
        check: &dyn Fn(&Path) -> Option<String>,
        progress: &(impl Fn(u32) + ?Sized),
    ) -> Result<()> {
        let new = crate::settings::with_suffix(target, ".new");
        let _ = fs::remove_dir_all(&new);
        fs::create_dir_all(&new)?;
        let mut child = Command::new("7z")
            .args(["x", &self.installer])
            .arg(format!("-o{}", new.display()))
            .args(entries)
            .args(["-y", "-bso0", "-bse1", "-bsp1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Ok(mut process) = self.process.lock() {
            *process = Some(child.id());
        }
        if self.cancelled() {
            self.stop_process();
        }
        let said = child
            .stdout
            .take()
            .map_or_else(String::new, |mut stream| Self::read_output(&mut stream, progress));
        let code = child.wait()?;
        if let Ok(mut process) = self.process.lock() {
            *process = None;
        }
        if self.cancelled() {
            let _ = fs::remove_dir_all(&new);
            return Err(Failure::new("Stopped."));
        }
        let problem = if code.success() {
            check(&new)
        } else {
            Some(format!("7z failed: {}", tail(said.trim(), LOOKED_AT)))
        };
        if let Some(problem) = problem {
            let _ = write_log(context, &format!("setup: {said}"));
            let _ = fs::remove_dir_all(&new);
            return Err(Failure(problem));
        }
        let _ = fs::remove_dir_all(target);
        fs::rename(&new, target)?;
        Ok(())
    }
}

fn tail(text: &str, size: usize) -> &str {
    let start = text.char_indices().rev().nth(size.saturating_sub(1)).map_or(0, |found| found.0);
    &text[start..]
}

/// The last `12%` in the text.
fn last_percentage(text: &str) -> Option<u32> {
    let before: Vec<&str> = text.split('%').collect();
    before[..before.len() - 1].iter().rev().find_map(|part| {
        let digits: String = part.chars().rev().take_while(char::is_ascii_digit).collect();
        digits.chars().rev().collect::<String>().parse().ok()
    })
}

#[cfg(test)]
pub mod stand_in {
    use super::*;
    use crate::setup::inf::stand_in::FAKE_INF;
    use crate::testing::Scratch;

    /// An archive laid out like the installer, which 7z can read.
    pub fn make_installer(scratch: &Scratch, name: &str, inf: &str, files: &[&str]) -> String {
        let source = format!("{name}-source");
        scratch.write(&format!("{source}/{INSTALLER_FOLDER}/{INF_FILE}"), inf);
        for file in files {
            scratch.write(&format!("{source}/{INSTALLER_FOLDER}/{file}"), [7u8; 1 << 16]);
        }
        scratch.write(&format!("{source}/NvApp/not-needed.exe"), "x");
        let path = scratch.path(&format!("{name}.exe"));
        let done = Command::new("7z")
            .args(["a", "-t7z"])
            .arg(&path)
            .arg(".")
            .current_dir(scratch.path(&source))
            .stdout(Stdio::null())
            .status();
        assert!(done.unwrap().success());
        path.display().to_string()
    }

    pub fn good_installer(scratch: &Scratch) -> String {
        make_installer(scratch, "good", FAKE_INF, &["other.dll"])
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::{good_installer, make_installer};
    use super::*;
    use crate::machine::sorted_names;
    use crate::settings::{driver_folder, read_setup};
    use crate::setup::inf::stand_in::FAKE_INF;
    use crate::testing::{Scratch, scratch_context};
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn newer() -> String {
        FAKE_INF.replace("16.1742", "16.2101")
    }

    const NO_PROGRESS: fn(u32) = |_| {};

    #[test]
    fn an_installer_is_judged_by_what_is_inside_it() {
        let scratch = Scratch::new();
        let good = good_installer(&scratch);
        assert_eq!(read_installer(&good).unwrap(), WINDOWS_DRIVER);
        check_installer(&good).unwrap();
        let newer = make_installer(&scratch, "newer", &newer(), &["other.dll"]);
        assert!(check_installer(&newer).unwrap_err().0.contains("621.01"));
        let junk = scratch.write("not-one.exe", "MZ, but not an archive").display().to_string();
        assert!(check_installer(&junk).unwrap_err().0.contains("not NVIDIA's"));
        let gone = scratch.path("nothing.exe").display().to_string();
        assert!(check_installer(&gone).unwrap_err().0.contains("not NVIDIA's"));
    }

    #[test]
    fn only_the_drivers_files_come_out_and_setup_points_at_them() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let made = Extraction::new(&good_installer(&scratch)).run(&context, &NO_PROGRESS).unwrap();
        assert_eq!(
            made,
            context.paths.store.join(format!("setup/driver-{WINDOWS_DRIVER}/{INSTALLER_FOLDER}"))
        );
        assert_eq!(sorted_names(&made), ["nv_dispi.inf", "other.dll"]);
        assert!(!made.parent().unwrap().with_extension("417.42.new").exists());
        assert_eq!(driver_folder(&context), made, "the install must use what setup extracted");
        assert_eq!(read_setup(&context)["driver_version"], WINDOWS_DRIVER);
        assert_eq!(driver_problem(&made), None);
    }

    #[test]
    fn a_failure_leaves_the_good_folder_alone() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let made = Extraction::new(&good_installer(&scratch)).run(&context, &NO_PROGRESS).unwrap();
        let before = fs::metadata(made.join("other.dll")).unwrap().ino();
        let new = crate::settings::with_suffix(made.parent().unwrap(), ".new");
        let wrong = make_installer(&scratch, "wrong", &newer(), &["other.dll"]);
        assert!(Extraction::new(&wrong).run(&context, &NO_PROGRESS).unwrap_err().0.contains("621.01"));
        assert_eq!(fs::metadata(made.join("other.dll")).unwrap().ino(), before);
        assert!(!new.exists());
    }

    #[test]
    fn stopping_first_leaves_the_good_folder_alone() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let good = good_installer(&scratch);
        let made = Extraction::new(&good).run(&context, &NO_PROGRESS).unwrap();
        let before = fs::metadata(made.join("other.dll")).unwrap().ino();
        let extraction = Extraction::new(&good);
        extraction.cancel();
        assert_eq!(extraction.run(&context, &NO_PROGRESS).unwrap_err().0, "Stopped.");
        assert_eq!(fs::metadata(made.join("other.dll")).unwrap().ino(), before);
    }

    #[test]
    fn percentages_are_found_among_backspaces() {
        assert_eq!(last_percentage("  5% 1 - file\u{8}\u{8}\u{8}"), Some(5));
        assert_eq!(last_percentage(" 12% 13%"), Some(13));
        assert_eq!(last_percentage("nothing"), None);
    }

    #[test]
    fn stopping_ends_a_slow_process_halfway() {
        // Stands in for 7z, which a real extraction is too quick to catch.
        let extraction = Arc::new(Extraction::new("unused"));
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        *extraction.process.lock().unwrap() = Some(child.id());
        let stopper = extraction.clone();
        thread::spawn(move || stopper.cancel());
        let end = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < end, "stopping must end the process");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(extraction.cancelled());
    }
}
