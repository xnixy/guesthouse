//! Makes a guest's disk. From a Windows image it needs no installer and no screen: a new VHDX is
//! attached, given the partitions UEFI boots from, the edition is applied to it with DISM, and its
//! boot files are written with bcdboot, all programs every Windows has. Then the answer file, the
//! report and the card's driver go on it, and the first start finishes the rest. An Ubuntu ISO goes
//! to `linux` instead, where Ubuntu's own installer does the work on the first boot.

use super::{
    absolute, chosen_card, copy_into, give_guest_folder, linux, need_elevation, system_program, volumes,
};
use crate::events::Sink;
use crate::guest;
use crate::machine::Guest;
use crate::record::Record;
use crate::settings::PrepareSettings;
use crate::wim_info;
use guesthouse_account_core::common::windows_name_problem;
use guesthouse_hcs::disk::{self, Kind, VirtualDisk};
use guesthouse_hcs::guest_volume::GuestVolume;
use guesthouse_hcs::host;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// UEFI's system partition, in MB, as Windows' own installer makes it.
const SYSTEM_PARTITION_MB: u32 = 260;
/// The partition Windows reserves for its own use on a GPT disk, in MB.
const RESERVED_PARTITION_MB: u32 = 16;
pub(super) const BYTES_PER_GB: u64 = 1 << 30;

#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
pub fn prepare(settings: &PrepareSettings, sink: &dyn Sink) -> Result<(), String> {
    need_elevation()?;
    let disk = absolute(&settings.disk)?;
    let image = absolute(&settings.image)?;
    if disk.exists() {
        return Err(format!(
            "{} is there already. prepare makes a new disk, and never writes over one.",
            disk.display()
        ));
    }
    let card = chosen_card(&settings.gpu)?;

    // An ISO stays attached while its image is applied or its files are copied.
    let is_iso = image.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("iso"));
    let (_iso, wim) = if is_iso {
        let (iso, root) = attach_iso(&image)?;
        if root.join(guest::linux::INSTALLER_KERNEL).is_file() {
            return linux::prepare(settings, &disk, &image, &root, card.as_ref(), sink);
        }
        let wim = ["install.wim", "install.esd"]
            .iter()
            .map(|name| root.join("sources").join(name))
            .find(|path| path.is_file())
            .ok_or_else(|| {
                format!(
                    "{} is neither a Windows ISO, with sources\\install.wim or install.esd, nor an \
                     Ubuntu installer, with casper\\vmlinuz.",
                    image.display()
                )
            })?;
        (Some(iso), wim)
    } else {
        (None, image.clone())
    };
    // Only now is it known to be Windows, so only now is Windows' rule for the name asked.
    if let Some(problem) = windows_name_problem(&settings.user) {
        return Err(problem);
    }

    let listing = run_captured(
        Command::new(system_program("dism.exe"))
            .arg("/English")
            .arg("/Get-WimInfo")
            .raw_arg(format!("/WimFile:\"{}\"", wim.display())),
    )?;
    let (index, edition) = wim_info::choose(&listing, &settings.edition)?;

    sink.note(format!("Making {} ({} GB at most).", disk.display(), settings.size_gb));
    disk::create_vhdx(&disk, settings.size_gb * BYTES_PER_GB).map_err(|error| error.to_string())?;
    let made = fill(
        &disk,
        &wim,
        (index, &edition),
        settings,
        card.as_ref().and_then(|card| card.driver_folder.as_deref()),
        sink,
    );
    if made.is_err() {
        // A half-made disk would only be refused by the next run, so it goes.
        let _ = fs::remove_file(&disk);
    }
    made
}

/// The ISO, attached until dropped, and the root of its volume. Programs are given the drive letter
/// Windows gave it, and the volume's own path only without one.
fn attach_iso(image: &Path) -> Result<(VirtualDisk, PathBuf), String> {
    let mut iso = VirtualDisk::open(image, Kind::Iso).map_err(|error| error.to_string())?;
    iso.attach(true).map_err(|error| error.to_string())?;
    let root = volumes(&iso)?
        .into_iter()
        .next()
        .map(|volume| volume.mount_points.into_iter().next().unwrap_or(volume.root))
        .ok_or_else(|| format!("Windows showed no volume for {}.", image.display()))?;
    Ok((iso, root))
}

fn fill(
    disk_path: &Path,
    wim: &Path,
    (index, edition): (u32, &str),
    settings: &PrepareSettings,
    driver: Option<&Path>,
    sink: &dyn Sink,
) -> Result<(), String> {
    let (_attached, number) = attach_new(disk_path)?;
    let [system, windows] = free_letters()?;
    diskpart(
        number,
        &format!(
            "create partition efi size={SYSTEM_PARTITION_MB}\n\
             format quick fs=fat32 label=\"System\"\n\
             assign letter={system}\n\
             create partition msr size={RESERVED_PARTITION_MB}\n\
             create partition primary\n\
             format quick fs=ntfs label=\"Windows\"\n\
             assign letter={windows}\n"
        ),
        &[system, windows],
    )?;
    let windows_root = PathBuf::from(format!("{windows}:\\"));

    sink.note(format!("Applying {edition} from {}. This takes a while.", wim.display()));
    run(Command::new(system_program("dism.exe"))
        .arg("/Apply-Image")
        .raw_arg(format!("/ImageFile:\"{}\"", wim.display()))
        .arg(format!("/Index:{index}"))
        .raw_arg(format!("/ApplyDir:{windows}:\\")))?;
    run(Command::new(system_program("bcdboot.exe")).arg(windows_root.join("Windows")).args([
        "/s",
        &format!("{system}:"),
        "/f",
        "UEFI",
    ]))?;

    let password = new_password()?;
    let windows_volume =
        GuestVolume::open(&windows_root).map_err(|error| format!("{}: {error}", windows_root.display()))?;
    give_guest_folder(&windows_volume)?;
    write(&windows_volume, guest::ANSWER_FILE_PATH, &guest::answer_file(&settings.user, &password))?;
    write(&windows_volume, guest::SETUP_COMPLETE_PATH, guest::SETUP_COMPLETE)?;
    write(&windows_volume, guest::REPORT_PATH, guest::REPORT)?;
    if let Some(folder) = driver {
        copy_into(folder, &windows_volume, true, sink)?;
    }
    drop(windows_volume);

    Record { guest: Guest::Windows, ..Record::default() }.save(disk_path)?;
    write_account(disk_path, &settings.user, &password, sink)?;
    sink.note("It's ready to boot.");
    Ok(())
}

/// Attaches a disk just made, and gives its number once it's certain to be that disk: diskpart
/// wipes the disk it's given, so only a disk backed by a file, with nothing on it yet, may go to it.
pub(super) fn attach_new(path: &Path) -> Result<(VirtualDisk, u32), String> {
    let mut attached = VirtualDisk::open(path, Kind::Vhdx).map_err(|error| error.to_string())?;
    attached.attach(false).map_err(|error| error.to_string())?;
    let number = attached.device_number().map_err(|error| error.to_string())?;
    let file_backed = disk::is_file_backed(number).map_err(|error| error.to_string())?;
    if !file_backed || !attached.volumes().map_err(|error| error.to_string())?.is_empty() {
        return Err(format!("Disk {number} is not the new, empty file. Nothing was written to it."));
    }
    Ok((attached, number))
}

/// A new password for a guest's administrator: 32 hex digits from a new GUID.
pub(super) fn new_password() -> Result<String, String> {
    Ok(host::guid_string(&host::new_guid().map_err(|error| error.to_string())?).replace('-', ""))
}

/// Keeps the guest's administrator and password beside its disk, the one place the host has them.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
pub(super) fn write_account(disk: &Path, user: &str, password: &str, sink: &dyn Sink) -> Result<(), String> {
    let account = disk.with_extension("account.txt");
    fs::write(&account, format!("user: {user}\npassword: {password}\n"))
        .map_err(|error| format!("{}: {error}", account.display()))?;
    sink.note(format!("Done. The guest's administrator and password are in {}.", account.display()));
    Ok(())
}

/// Drive letters nothing uses or keeps, from the end of the alphabet, for a new disk's partitions
/// while they're written.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
pub(super) fn free_letters<const N: usize>() -> Result<[char; N], String> {
    let taken = disk::letters_taken();
    let free: Vec<char> = ('D'..='Z')
        .rev()
        .filter(|letter| !taken.contains(letter) && !Path::new(&format!("{letter}:\\")).exists())
        .collect();
    free.get(..N).and_then(|letters| letters.try_into().ok()).ok_or_else(|| {
        format!("{N} free drive letters are needed while the disk is written, and there aren't.")
    })
}

/// Wipes disk `number`, makes it GPT, and runs `steps` on it, which must give the partitions these
/// `letters`. Only ever called on a disk `attach_new` vouched for.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
pub(super) fn diskpart(number: u32, steps: &str, letters: &[char]) -> Result<(), String> {
    // diskpart's list of disks can lag behind a disk attached a moment ago, most of all while
    // another diskpart is busy, and then it refuses the number. A rescan brings it up to date.
    let script = format!(
        "rescan\n\
         select disk {number}\n\
         online disk noerr\n\
         attributes disk clear readonly noerr\n\
         clean\n\
         convert gpt\n\
         {steps}\
         exit\n"
    );
    let path = std::env::temp_dir().join(format!("guesthouse-diskpart-{}.txt", std::process::id()));
    fs::write(&path, script).map_err(|error| format!("{}: {error}", path.display()))?;
    let result = run_captured(Command::new(system_program("diskpart.exe")).arg("/s").arg(&path));
    let _ = fs::remove_file(&path);
    result?;
    for letter in letters {
        if !Path::new(&format!("{letter}:\\")).exists() {
            return Err(format!("diskpart made no {letter}: drive."));
        }
    }
    Ok(())
}

/// Writes `contents` as the file at `relative` on a guest's volume, making the folders on the way.
pub(super) fn write(guest: &GuestVolume, relative: &str, contents: &str) -> Result<(), String> {
    guest.write(relative, contents).map_err(|error| error.to_string())
}

fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|error| format!("{}: {error}", program(command)))?;
    if status.success() { Ok(()) } else { Err(format!("{} failed ({status}).", program(command))) }
}

fn run_captured(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|error| format!("{}: {error}", program(command)))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "{} failed ({}):\n{text}{}",
            program(command),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn program(command: &Command) -> String {
    Path::new(command.get_program()).file_name().unwrap_or_default().to_string_lossy().into_owned()
}
