//! A Linux guest: Ubuntu, installed by Ubuntu's own installer on the machine's first boot, with no
//! questions and no screen. Beside the guest's disk goes a small FAT disk, its files disk, which does
//! three jobs. Its UEFI fallback is the installer's own Shim and GRUB, which start the installer from
//! the ISO with a menu of ours. Its label, CIDATA, makes it the installer's answer file. And once the
//! guest runs, it holds the host's GPU files, which the guest mounts at /usr/lib/wsl as WSL has them:
//! the libraries from WSL and from the GPU's driver, and the driver's folder from the driver store.

use super::prepare::{BYTES_PER_GB, attach_new, diskpart, free_letters, new_password, write, write_account};
use super::volumes;
use crate::events::Sink;
use crate::guest::linux::{
    ACCOUNT, BOOT_FILES, FILES, INSTALLER_INITRD, LABEL, USER_DATA, WSL_DRIVERS, WSL_LIB, account, unix_text,
};
use crate::machine::Guest;
use crate::record::{Record, files_disk};
use crate::settings::{PrepareSettings, Share};
use guesthouse_account_core::common::linux_name_problem;
use guesthouse_hcs::disk::{self, Kind, VirtualDisk};
use guesthouse_hcs::drivers::copy_folder;
use guesthouse_hcs::gpu::Card;
use guesthouse_hcs::guest_volume::{Entry, GuestVolume};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The file on the files disk that names the shares for gpu.sh, a line each, as `NAME PORT ro` or
/// `NAME PORT rw`.
const SHARE: &str = r"guesthouse\share";

fn share_lines(shares: &[Share]) -> String {
    shares
        .iter()
        .map(|share| {
            let mode = if share.read_only { "ro" } else { "rw" };
            format!("{} {} {mode}\n", share.name(), guesthouse_hcs::schema::PLAN9_PORT)
        })
        .collect()
}

/// The folder on the files disk that holds the guest's own scripts.
const GUEST_SCRIPTS: &str = "guesthouse\\";
/// Room on the files disk beyond the GPU files, for a driver update that's larger.
const HEADROOM_BYTES: u64 = BYTES_PER_GB;
const SMALLEST_FILES_DISK_GB: u64 = 2;
/// The file system the files disk is made with, which has no links.
const FILE_SYSTEM: &str = "FAT32";
/// diskpart formats FAT32 up to this size.
const LARGEST_FILES_DISK_GB: u64 = 32;
/// The library nvidia-smi and CUDA load beside NVIDIA's. It comes with WSL, not with the driver.
const DXCORE: &str = "libdxcore.so";

#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
pub fn prepare(
    settings: &PrepareSettings,
    disk_path: &Path,
    iso: &Path,
    iso_root: &Path,
    card: Option<&Card>,
    sink: &dyn Sink,
) -> Result<(), String> {
    if let Some(problem) = linux_name_problem(&settings.user) {
        return Err(problem);
    }
    for needed in [INSTALLER_INITRD, BOOT_FILES[0].0, BOOT_FILES[1].0] {
        if !iso_root.join(needed).is_file() {
            return Err(format!("{} has no {needed}, which the install starts from.", iso.display()));
        }
    }
    let files = files_disk(disk_path);
    if files.exists() {
        return Err(format!("{} is there already, and prepare never writes over a disk.", files.display()));
    }
    let made = make(settings, disk_path, &files, iso, iso_root, card, sink);
    if made.is_err() {
        // Half-made disks would only be refused by the next run, so they go.
        let _ = fs::remove_file(disk_path);
        let _ = fs::remove_file(&files);
    }
    made
}

#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn make(
    settings: &PrepareSettings,
    disk_path: &Path,
    files: &Path,
    iso: &Path,
    iso_root: &Path,
    card: Option<&Card>,
    sink: &dyn Sink,
) -> Result<(), String> {
    sink.note(format!(
        "Making {} ({} GB at most), for Ubuntu's installer to fill.",
        disk_path.display(),
        settings.size_gb
    ));
    disk::create_vhdx(disk_path, settings.size_gb * BYTES_PER_GB).map_err(|error| error.to_string())?;

    let size = files_disk_bytes(card);
    sink.note(format!("Making its files disk, {} ({} GB).", files.display(), size / BYTES_PER_GB));
    disk::create_vhdx(files, size).map_err(|error| error.to_string())?;
    let (_attached, number) = attach_new(files)?;
    let [letter] = free_letters()?;
    diskpart(
        number,
        &format!("create partition efi\nformat quick fs=fat32 label=\"{LABEL}\"\nassign letter={letter}\n"),
        &[letter],
    )?;
    let root = PathBuf::from(format!("{letter}:\\"));
    let guest = GuestVolume::open(&root).map_err(|error| format!("{}: {error}", root.display()))?;

    for (from, to) in BOOT_FILES {
        let source = iso_root.join(from);
        // MokManager is only needed to enrol keys, which this never does, so an ISO without it is fine.
        if source.is_file() {
            guest.copy_in(&source, to).map_err(|error| error.to_string())?;
        }
    }
    for (path, text) in FILES {
        write(&guest, path, &unix_text(text))?;
    }
    let password = new_password()?;
    write(&guest, ACCOUNT, &account(&settings.user, &password))?;
    refresh(&guest, card, &[], sink)?;
    drop(guest);

    Record { guest: Guest::Linux, installer: Some(iso.to_owned()), ..Record::default() }.save(disk_path)?;
    write_account(disk_path, &settings.user, &password, sink)?;
    sink.note(
        "It's ready. The first boot installs Ubuntu, which takes a while, and then the machine turns \
         itself off. The boot after that starts it.",
    );
    Ok(())
}

/// Brings the host's GPU files on the files disk up to date with the host's, while no machine has
/// the disk open. Only the disk prepare made is touched: one FAT32 volume labelled CIDATA. The
/// guest has the disk read-only, so its installer can't take its account and answer file off it
/// once it has installed, and they're taken off here, at the first start after `installed`.
pub fn refresh_disk(
    files: &Path,
    card: Option<&Card>,
    shares: &[Share],
    installed: bool,
    sink: &dyn Sink,
) -> Result<(), String> {
    let mut attached = VirtualDisk::open(files, Kind::Vhdx).map_err(|error| error.to_string())?;
    attached.attach(false).map_err(|error| error.to_string())?;
    let found = volumes(&attached)?;
    let [volume] = found.as_slice() else {
        return Err(format!(
            "{} has {} volumes, and the files disk Guesthouse makes has one. It was left as it is.",
            files.display(),
            found.len()
        ));
    };
    let guest = GuestVolume::open(&volume.root).map_err(|error| format!("{}: {error}", files.display()))?;
    let made = guest.file_system().map_err(|error| format!("{}: {error}", files.display()))?;
    if made.name != FILE_SYSTEM || !made.label.eq_ignore_ascii_case(LABEL) {
        return Err(format!(
            "{} isn't the files disk Guesthouse made: its volume is {} labelled \"{}\", not {FILE_SYSTEM} \
             labelled {LABEL}. It was left as it is.",
            files.display(),
            made.name,
            made.label
        ));
    }
    if installed {
        for path in [ACCOUNT, USER_DATA] {
            match guest.remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.to_string()),
                _ => {}
            }
        }
    }
    refresh(&guest, card, shares, sink)
}

/// The GPU files on the files disk at `root`. WSL's own libraries go first and the driver's on top,
/// as WSL lays one over the other. Only the card's driver folder is kept, so an old driver's goes.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn refresh(
    guest: &GuestVolume,
    card: Option<&Card>,
    shares: &[Share],
    sink: &dyn Sink,
) -> Result<(), String> {
    // The guest runs its scripts from here at every start, so these are always this build's.
    for (path, text) in FILES.iter().filter(|(path, _)| path.starts_with(GUEST_SCRIPTS)) {
        write(guest, path, &unix_text(text))?;
    }
    // gpu.sh mounts the shares it names, and unmounts those it doesn't.
    if shares.is_empty() {
        let _ = guest.remove_file(SHARE);
    } else {
        write(guest, SHARE, &share_lines(shares))?;
    }
    let lib = Path::new(WSL_LIB);
    for source in library_folders().into_iter().filter(|folder| folder.is_dir()) {
        let copied = copy_folder(&source, guest, lib).map_err(|error| error.to_string())?;
        sink.note(format!(
            "{}: {} files copied, {} already there.",
            source.display(),
            copied.copied,
            copied.kept
        ));
    }
    if !guest.is_file(lib.join(DXCORE)) {
        sink.problem(format!(
            "This host has no {DXCORE}, which comes with WSL, so nvidia-smi and CUDA won't run in the \
             guest. Installing WSL gives it."
        ));
    }

    let drivers = Path::new(WSL_DRIVERS);
    guest.create_dir_all(drivers).map_err(|error| error.to_string())?;
    let Some(folder) = card.and_then(|card| card.driver_folder.as_deref()) else { return Ok(()) };
    let name = folder.file_name().expect("a driver folder has a name");
    let copied = copy_folder(folder, guest, &drivers.join(name)).map_err(|error| error.to_string())?;
    sink.note(format!(
        "Driver {}: {} files copied, {} already there.",
        name.to_string_lossy(),
        copied.copied,
        copied.kept
    ));
    for (entry, kind) in guest.read_dir(drivers).map_err(|error| error.to_string())? {
        if entry != name && matches!(kind, Entry::Folder | Entry::Link) {
            guest.remove_tree(drivers.join(&entry)).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Where the host keeps the libraries a guest's /usr/lib/wsl/lib needs: WSL's own (DXCore and
/// D3D12), then those the GPU's driver puts there for WSL.
fn library_folders() -> [PathBuf; 2] {
    let programs = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .unwrap_or_else(|| r"C:\Program Files".into());
    let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    [PathBuf::from(programs).join(r"WSL\lib"), PathBuf::from(windows).join(r"System32\lxss\lib")]
}

/// Room for the GPU files with some to spare, in whole GB, within what diskpart can format as FAT32.
fn files_disk_bytes(card: Option<&Card>) -> u64 {
    let driver = card.and_then(|card| card.driver_folder.as_deref());
    let needed: u64 = library_folders()
        .iter()
        .chain(driver.map(Path::to_path_buf).iter())
        .map(|folder| size_of_folder(folder))
        .sum();
    let gb = (needed + HEADROOM_BYTES).div_ceil(BYTES_PER_GB);
    gb.clamp(SMALLEST_FILES_DISK_GB, LARGEST_FILES_DISK_GB) * BYTES_PER_GB
}

#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn size_of_folder(folder: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(folder) else { return 0 };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => size_of_folder(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |metadata| metadata.len()),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_disk_is_never_smaller_or_larger_than_fat32_allows() {
        let size = files_disk_bytes(None);
        assert!(
            (SMALLEST_FILES_DISK_GB * BYTES_PER_GB..=LARGEST_FILES_DISK_GB * BYTES_PER_GB).contains(&size)
        );
        assert_eq!(size % BYTES_PER_GB, 0);
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use crate::events::Kept;
    use crate::windows::tests::junction;

    /// A folder standing in for the files disk's volume, one for the host's own files beside it,
    /// and a card whose driver folder is there too.
    fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf, Card) {
        let base = std::env::temp_dir().join(format!("gh-files-links-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (root, host) = (base.join("volume"), base.join("host"));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&host).unwrap();
        let driver = base.join(r"store\card.inf_amd64_0123456789abcdef");
        fs::create_dir_all(&driver).unwrap();
        fs::write(driver.join("card.dll"), "driver").unwrap();
        let card = Card {
            name: "Card".into(),
            instance_id: r"PCI\VEN_10DE&DEV_0000".into(),
            interface_path: String::new(),
            service: "card".into(),
            driver_folder: Some(driver),
            memory: None,
        };
        (base, root, host, card)
    }

    #[test]
    fn a_wsl_folder_that_is_a_link_has_nothing_copied_into_or_removed_from_what_it_names() {
        let (base, root, host, card) = scratch("drivers");
        fs::create_dir_all(host.join(r"drivers\Alice\Documents")).unwrap();
        fs::write(host.join(r"drivers\Alice\Documents\letter.txt"), "the host's own").unwrap();
        junction(&root.join("wsl"), &host);
        let guest = GuestVolume::open(&root).unwrap();

        let refreshed = refresh(&guest, Some(&card), &[], &Kept::default());
        assert_eq!(
            fs::read_to_string(host.join(r"drivers\Alice\Documents\letter.txt")).unwrap(),
            "the host's own"
        );
        let names: Vec<_> = fs::read_dir(&host).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(names, ["drivers"], "nothing was copied through the link");
        assert!(refreshed.is_err());

        drop(guest);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_script_that_is_a_link_to_a_host_file_leaves_the_file_as_it_was() {
        let (base, root, host, _) = scratch("script");
        let hosts_own = host.join("settings.ini");
        fs::write(&hosts_own, "the host's own").unwrap();
        fs::create_dir_all(root.join("guesthouse")).unwrap();
        std::os::windows::fs::symlink_file(&hosts_own, root.join(r"guesthouse\gpu.sh")).unwrap();
        let guest = GuestVolume::open(&root).unwrap();

        let refreshed = refresh(&guest, None, &[], &Kept::default());
        assert_eq!(fs::read_to_string(&hosts_own).unwrap(), "the host's own");
        assert!(refreshed.is_err());

        drop(guest);
        let _ = fs::remove_dir_all(base);
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;
    use crate::events::Kept;

    /// Reads this host's own WSL and driver libraries, so it's run by hand only.
    #[test]
    #[ignore = "copies this host's WSL libraries"]
    fn the_files_disk_gets_wsl_and_the_drivers_libraries_with_dxcore() {
        let root = std::env::temp_dir().join(format!("gh-files-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let guest = GuestVolume::open(&root).unwrap();
        refresh(&guest, None, &[], &Kept::default()).unwrap();
        let lib = root.join(WSL_LIB);
        assert!(lib.join(DXCORE).is_file(), "WSL's DXCore");
        assert!(lib.join("libcuda.so.1").is_file(), "the driver's CUDA");
        assert!(lib.join("nvidia-smi").is_file(), "the driver's nvidia-smi");
        assert!(root.join(WSL_DRIVERS).is_dir());
        refresh(&guest, None, &[], &Kept::default()).unwrap();
        drop(guest);
        let _ = fs::remove_dir_all(root);
    }
}
