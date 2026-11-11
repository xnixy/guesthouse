//! Intel's Windows driver, for Windows machines on an Intel card: checking Intel's installer, and
//! taking the user-mode drivers out of it.
//!
//! The installer is a program with Intel's whole package attached as a RAR archive, which `7z` lists
//! on any system but can only unpack where it was built with RAR support. In the archive, one INF in
//! the `Graphics` folder serves every Intel card, and the driver's files sit beside it in that folder.
//! The files a card gets are the ones that INF copies for it (see `intel_inf`), so the guest gets
//! them and not the kernel driver, the firmware or the media software.

use super::driver::Extraction;
use super::inf::inf_driver_version;
use super::intel::{SYSTEM_LIST, folder_problem, write_intel_setup};
use super::guest_files::{GuestFile, file_list};
use super::intel_inf::{decode, guest_files, lists_device, system_files};
use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::vm_args::on_path;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// The release of Intel's Windows driver the Intel daemon takes, which refuses any other. Intel's
/// package for it is `gfx_win_101.9175.exe`.
pub const INTEL_WINDOWS_DRIVER: &str = "32.0.101.9175";
/// The folder of the package that holds the driver, and the INF that names its files.
const DRIVER_ENTRY: &str = "Graphics";
const INF_FILE: &str = "iigd_dch.inf";
const PATH_LINE: &str = "Path = ";
const READ_SECONDS: &str = "120";
/// What `timeout` exits with.
const TIMED_OUT: i32 = 124;
/// What 7z says about an archive it has no code for.
const NO_METHOD: &str = "Unsupported Method";
const NO_RAR: &str = "This copy of 7z can't unpack Intel's installer, which keeps its files in a RAR archive. \
                      7z from 7-zip.org can, and many distributions' builds leave that part out. Install it as \
                      `7z`, or put it first on your PATH, and try again.";

/// What is known of an installer before it is unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntelPackage {
    pub version: String,
    /// The driver's files this card needs, by their paths in the archive. That is its own folder's
    /// files, and the files the INF renames into Windows' system folders.
    pub entries: Vec<String>,
    /// What goes to System32 and SysWOW64, from which of the entries and under what name.
    pub system_files: Vec<GuestFile>,
}

fn inf_entry() -> String {
    format!("{DRIVER_ENTRY}/{INF_FILE}")
}

/// Says why a package with this INF won't do for the card `device`, if it won't.
pub fn package_problem(inf: &str, device: u32) -> Option<String> {
    let version = match inf_driver_version(inf) {
        Ok(version) => version,
        Err(failure) => return Some(failure.0),
    };
    if version != INTEL_WINDOWS_DRIVER {
        return Some(format!(
            "This is Intel's driver {version}. Guesthouse was built for {INTEL_WINDOWS_DRIVER}, so get that \
             release from Intel."
        ));
    }
    (!lists_device(inf, device))
        .then(|| format!("This package of Intel's driver doesn't list this card, device {device:04X}."))
}

fn seven_zip(args: &[&str]) -> Result<Vec<u8>> {
    if !on_path("7z") {
        return Err(Failure::new("Reading the installer needs 7z, which is not installed."));
    }
    let result = Command::new("timeout").arg(READ_SECONDS).arg("7z").args(args).output()?;
    if result.status.code() == Some(TIMED_OUT) {
        return Err(Failure::new("The installer took too long to read."));
    }
    if !result.status.success() {
        let said = String::from_utf8_lossy(&[&result.stdout[..], &result.stderr[..]].concat()).into_owned();
        let reason =
            if said.contains(NO_METHOD) { NO_RAR } else { "This file is not Intel's driver installer." };
        return Err(Failure::new(reason));
    }
    Ok(result.stdout)
}

/// The paths in a `7z l -slt` listing.
fn listed_paths(listing: &str) -> Vec<&str> {
    listing.lines().filter_map(|line| line.strip_prefix(PATH_LINE)).collect()
}

/// Reads Intel's installer at `path`, and says whether it will do for the card `device`.
pub fn read_intel_installer(path: &str, device: u32) -> Result<IntelPackage> {
    let listing = String::from_utf8_lossy(&seven_zip(&["l", "-ba", "-slt", path])?).into_owned();
    let paths = listed_paths(&listing);
    let inf_path = inf_entry();
    if !paths.contains(&inf_path.as_str()) {
        return Err(Failure::new("This file is not Intel's driver installer."));
    }
    let inf = decode(&seven_zip(&["e", "-so", path, &inf_path])?);
    if let Some(problem) = package_problem(&inf, device) {
        return Err(Failure(problem));
    }
    let system = system_files(&inf, device);
    let mut names = guest_files(&inf, device);
    names.extend(system.iter().map(|file| file.source.clone()));
    let mut seen = std::collections::BTreeSet::new();
    names.retain(|name| seen.insert(name.to_lowercase()));
    let entries: Vec<String> = names.iter().map(|name| format!("{DRIVER_ENTRY}/{name}")).collect();
    if let Some(absent) = entries.iter().find(|entry| !paths.contains(&entry.as_str())) {
        return Err(Failure(format!("Intel's package lacks {absent}, which its INF lists for this card.")));
    }
    Ok(IntelPackage { version: inf_driver_version(&inf)?, entries, system_files: system })
}

/// Takes the card's user-mode drivers out of Intel's installer and writes down where they are.
/// `progress` gets the percentage done.
pub fn unpack_intel(
    extraction: &Extraction,
    context: &Context,
    package: &IntelPackage,
    progress: &(impl Fn(u32) + ?Sized),
) -> Result<PathBuf> {
    let target = context.paths.setup_folder().join(format!("intel-driver-{}", package.version));
    let check = |unpacked: &std::path::Path| folder_problem(&unpacked.join(DRIVER_ENTRY));
    let entries: Vec<&str> = package.entries.iter().map(String::as_str).collect();
    extraction
        .unpack_entries(context, &entries, &target, &check, progress)
        .map_err(|failure| if failure.0.contains(NO_METHOD) { Failure::new(NO_RAR) } else { failure })?;
    let folder = target.join(DRIVER_ENTRY);
    // The guest's scripts place these from the driver's copy in its store, so the list travels with it.
    fs::write(folder.join(SYSTEM_LIST), file_list(&package.system_files))?;
    write_intel_setup(context, &folder, &package.version)?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::intel::{intel_driver_folder, missing_files};
    use crate::setup::intel_inf::stand_in::{DEVICE, GUEST_FILES, SYSTEM_FILES, fake_inf};
    use crate::testing::{Scratch, scratch_context};
    use std::fs;
    use std::process::Stdio;

    /// An archive laid out like Intel's package, which 7z can read: the INF in UTF-16 and a file for
    /// each name in it, with the kernel driver and firmware that a guest doesn't get.
    fn make_installer(scratch: &Scratch, inf: &str) -> String {
        let mut text = vec![0xFF, 0xFE];
        text.extend(inf.encode_utf16().flat_map(u16::to_le_bytes));
        scratch.write(&format!("source/{DRIVER_ENTRY}/{INF_FILE}"), text);
        let extras = ["igdkmdn64.sys", "not-ours.dll", "mfxplugin64_hw.dll", "ControlLib.dll", "vulkaninfo-64.exe"];
        let sources = SYSTEM_FILES.iter().map(|file| &file.1);
        for name in GUEST_FILES.iter().chain(sources).chain(&extras) {
            scratch.write(&format!("source/{DRIVER_ENTRY}/{name}"), format!("content of {name}"));
        }
        let manifest = |library| crate::setup::intel::stand_in::manifest(library);
        scratch.write(&format!("source/{DRIVER_ENTRY}/igvk64.json"), manifest("igvk64.dll"));
        scratch.write(&format!("source/{DRIVER_ENTRY}/igvk32.json"), manifest("igvk32.dll"));
        scratch.write("source/Graphics/ifwi/firmware.bin", "firmware");
        scratch.write("source/readme.txt", "x");
        let path = scratch.path("intel.exe");
        let done = Command::new("7z")
            .args(["a", "-t7z"])
            .arg(&path)
            .arg(".")
            .current_dir(scratch.path("source"))
            .stdout(Stdio::null())
            .status();
        assert!(done.unwrap().success());
        path.display().to_string()
    }

    fn inf_for(version: &str) -> String {
        fake_inf().replace(INTEL_WINDOWS_DRIVER, version)
    }

    #[test]
    fn a_package_is_judged_by_its_version_and_by_listing_the_card() {
        assert_eq!(package_problem(&fake_inf(), DEVICE), None);
        let other_card = package_problem(&fake_inf(), 0x1234).unwrap();
        assert!(other_card.contains("device 1234"), "{other_card}");
        let older = package_problem(&inf_for("32.0.101.8135"), DEVICE).unwrap();
        assert!(older.contains("32.0.101.8135") && older.contains(INTEL_WINDOWS_DRIVER), "{older}");
        assert!(package_problem("[Version]\n", DEVICE).unwrap().contains("no version"));
    }

    #[test]
    fn the_cards_user_mode_drivers_are_unpacked_and_written_down() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let installer = make_installer(&scratch, &fake_inf());
        let package = read_intel_installer(&installer, DEVICE).unwrap();
        assert_eq!(package.version, INTEL_WINDOWS_DRIVER);
        assert_eq!(package.entries.len(), GUEST_FILES.len() + SYSTEM_FILES.len());
        assert_eq!(package.system_files.len(), SYSTEM_FILES.len());
        let folder = unpack_intel(&Extraction::new(&installer), &context, &package, &|_| {}).unwrap();
        assert_eq!(intel_driver_folder(&context), Some(folder.clone()));
        assert_eq!(missing_files(&folder), Vec::<String>::new());
        let mut names: Vec<String> = fs::read_dir(&folder)
            .unwrap()
            .flatten()
            .map(|file| file.file_name().display().to_string())
            .collect();
        names.sort();
        let mut wanted: Vec<String> = GUEST_FILES.iter().map(|name| (*name).to_owned()).collect();
        wanted.extend(SYSTEM_FILES.iter().map(|file| file.1.to_owned()));
        wanted.push(SYSTEM_LIST.to_owned());
        wanted.sort();
        assert_eq!(
            names, wanted,
            "nothing but the files the INF copies for the card, no kernel driver, and the list of the system ones"
        );
        let list = fs::read_to_string(folder.join(SYSTEM_LIST)).unwrap();
        assert_eq!(list.lines().count(), SYSTEM_FILES.len());
        assert!(list.contains("System32	vulkan-1-64.dll	vulkan-1.dll
"), "{list}");
        assert!(list.contains("SysWOW64	msvcp140_32.dll	msvcp140.dll
"), "{list}");
        assert!(!folder.join("ifwi").exists(), "no firmware");
    }

    #[test]
    fn an_installer_for_another_release_or_card_is_refused_before_unpacking() {
        let scratch = Scratch::new();
        let installer = make_installer(&scratch, &inf_for("32.0.101.8135"));
        let refused = read_intel_installer(&installer, DEVICE).unwrap_err();
        assert!(refused.0.contains("32.0.101.8135"), "{}", refused.0);
        let scratch = Scratch::new();
        let installer = make_installer(&scratch, &fake_inf());
        let refused = read_intel_installer(&installer, 0x1234).unwrap_err();
        assert!(refused.0.contains("doesn't list this card"), "{}", refused.0);
    }

    #[test]
    fn a_file_that_is_not_intels_installer_is_said_to_be_none() {
        let scratch = Scratch::new();
        let not_it = scratch.write("other.txt", "hello");
        let refused = read_intel_installer(&not_it.display().to_string(), DEVICE).unwrap_err();
        assert!(refused.0.contains("not Intel's driver installer"), "{}", refused.0);
    }

    #[test]
    fn a_file_the_inf_lists_but_the_package_lacks_is_said() {
        let scratch = Scratch::new();
        let installer = make_installer(&scratch, &fake_inf().replace("igdrcl64.dll", "igdrcl-missing.dll"));
        let refused = read_intel_installer(&installer, DEVICE).unwrap_err();
        assert!(refused.0.contains("Graphics/igdrcl-missing.dll"), "{}", refused.0);
    }

    /// Intel's own installer, named in `GUESTHOUSE_INTEL_INSTALLER`, with the card in
    /// `GUESTHOUSE_INTEL_DEVICE` as hex. Run by hand, as the installer is most of a gigabyte and not ours.
    #[test]
    #[ignore = "needs Intel's installer"]
    fn a_real_installer_is_read() {
        let path = std::env::var("GUESTHOUSE_INTEL_INSTALLER").unwrap();
        let device = u32::from_str_radix(&std::env::var("GUESTHOUSE_INTEL_DEVICE").unwrap(), 16).unwrap();
        match read_intel_installer(&path, device) {
            Ok(package) => {
                println!("covers {device:04X}: {} files, {}", package.entries.len(), package.version)
            }
            Err(failure) => println!("refused {device:04X}: {}", failure.0),
        }
    }

    /// The release is the Intel daemon's, which lives in the backend.
    #[test]
    fn the_release_is_the_one_the_daemon_takes() {
        let path = format!("{}/../backend-intel/src/protocol/guest_driver.rs", env!("CARGO_MANIFEST_DIR"));
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains(&format!("GUEST_RELEASES: [&str; 1] = [\"{INTEL_WINDOWS_DRIVER}\"]")));
    }
}
