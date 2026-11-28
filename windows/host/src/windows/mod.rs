//! The work that calls into Windows: making a machine's disks, and running the machine.

pub mod boot;
pub mod hyper_v;
pub mod linux;
pub mod network;
pub mod prepare;
pub mod ssh;

use crate::events::{Event, Sink};
use crate::machine::{Guest, Machine};
use crate::record::{Record, files_disk};
use crate::screen;
use crate::settings::{BootSettings, GpuChoice, Share};
use guesthouse_hcs::disk::{Kind, VirtualDisk, Volume};
use guesthouse_hcs::drivers::copy_driver;
use guesthouse_hcs::gpu::{self, Card};
use guesthouse_hcs::guest_volume::{Grant, GuestVolume};
use guesthouse_hcs::host;
use guesthouse_hcs::schema::{Attachment, AttachmentType};
use guesthouse_remote_view_core::trust;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// How long Windows may take to show the volumes of a disk it has just attached.
const VOLUMES_SECONDS: u64 = 15;
/// The NVAPI wrapper a Windows guest on an NVIDIA card gets, from the program's folder.
const NVAPI_WRAPPER: &str = r"guest\nvapi64.dll";

/// The machine `settings` asks for, on this disk, with this record and network card, its pipes
/// named with `pipe_tag`.
pub fn machine(
    settings: &BootSettings,
    disk: PathBuf,
    record: &Record,
    network: Option<guesthouse_hcs::schema::NetworkAdapter>,
    pipe_tag: String,
) -> Machine {
    Machine {
        name: settings.name.clone(),
        pipe_tag,
        more_disks: more_disks(&disk, record),
        disk,
        memory_mb: settings.memory_mb,
        processors: settings.processors,
        tpm: settings.tpm,
        nested_virtualization: settings.nested_virtualization,
        network,
        guest: record.guest,
        console_access: settings.console_access.clone(),
        shares: settings.shares.clone(),
    }
}

/// The program a Windows guest mounts the host's share with, and WinFsp's installer, which it needs:
/// both in the program's guest folder, as the NVAPI wrapper is.
const SHARE_PROGRAM: &str = r"guest\guesthouse-guest-share.exe";
const WINFSP_INSTALLER: &str = r"guest\winfsp.msi";
/// Where the guest has them, from the root of its Windows volume: `report.ps1` installs WinFsp from
/// the one and runs the other, as `C:\Guesthouse\share\...`.
const SHARE_FOLDER: &str = r"Guesthouse\share";
const SHARE_PROGRAM_NAME: &str = "guesthouse-guest-share.exe";
const WINFSP_INSTALLER_NAME: &str = "winfsp.msi";

/// Where a Windows guest finds what the host shares: `report.ps1` reads this file, from the root of
/// its Windows volume, mounts each share as a drive and links the drive onto the desktop. Its lines
/// are `crate::guest::share_marker`'s. With no share it isn't there.
pub const SHARE_FILE: &str = r"Guesthouse\share.txt";

/// The folder Guesthouse keeps its files in, from the root of a Windows guest's volume:
/// `C:\Guesthouse`, where the guest's SYSTEM runs `report.ps1`, the screen's script and the
/// installers from.
const GUESTHOUSE_FOLDER: &str = "Guesthouse";
/// Who may use that folder: the guest's SYSTEM and its administrators. Whoever owns a file in it
/// may only read its permissions.
const GUEST_FOLDER_GRANTS: [Grant; 3] = [("SY", "FA"), ("BA", "FA"), ("OW", "RC")];
/// The most of a guest's report that is read back.
const MOST_REPORT_BYTES: u64 = 16 * 1024 * 1024;

/// Lets a stopped machine's disk grow to `size_gb`. The guest's own last partition is grown into
/// the room at its next start: `report.ps1` does it in Windows, and `gpu.sh` in Ubuntu.
pub fn grow_disk(disk: &Path, size_gb: u64) -> Result<(), String> {
    need_elevation()?;
    if crate::snapshots::made_on_another(disk)? {
        return Err("A disk with snapshots can't grow, as it's made on them. Delete them first.".into());
    }
    let bytes = size_gb * prepare::BYTES_PER_GB;
    let opened = VirtualDisk::open(disk, Kind::Vhdx).map_err(|error| error.to_string())?;
    let now = opened.size().map_err(|error| error.to_string())?;
    if bytes < now {
        return Err(format!(
            "The disk is {} GB already, and a disk only grows: a smaller one would cut into the guest's partitions.",
            now / prepare::BYTES_PER_GB
        ));
    }
    if bytes == now {
        return Ok(());
    }
    opened.grow(bytes).map_err(|error| error.to_string())
}

/// Snapshots' disks, through virtdisk.dll.
pub struct RealDisks;

impl crate::snapshots::Disks for RealDisks {
    fn differencing(&self, child: &Path, parent: &Path) -> Result<(), String> {
        guesthouse_hcs::disk::create_differencing_vhdx(child, parent).map_err(|error| error.to_string())
    }

    fn merge_into_parent(&self, child: &Path) -> Result<(), String> {
        let opened = VirtualDisk::open(child, Kind::Vhdx).map_err(|error| error.to_string())?;
        opened.merge_into_parent().map_err(|error| error.to_string())
    }

    fn set_parent(&self, child: &Path, parent: &Path) -> Result<(), String> {
        let opened = VirtualDisk::open_alone(child).map_err(|error| error.to_string())?;
        opened.set_parent(parent).map_err(|error| error.to_string())
    }

    fn parent_of(&self, child: &Path) -> Result<PathBuf, String> {
        let opened = VirtualDisk::open(child, Kind::Vhdx).map_err(|error| error.to_string())?;
        opened.parent().map_err(|error| error.to_string())
    }
}

/// The largest a machine's disk may grow to now, in whole GB.
pub fn disk_size_gb(disk: &Path) -> Result<u64, String> {
    let opened = VirtualDisk::open(disk, Kind::Vhdx).map_err(|error| error.to_string())?;
    Ok(opened.size().map_err(|error| error.to_string())? / prepare::BYTES_PER_GB)
}

/// A Linux guest's files disk, and its installer until it has installed. The guest only reads the
/// files disk: what is on it is the host's, and the host reads it back before each start.
fn more_disks(disk: &Path, record: &Record) -> Vec<Attachment> {
    if record.guest != Guest::Linux {
        return Vec::new();
    }
    let attach = |kind, path: &Path, read_only| Attachment {
        r#type: kind,
        path: path.to_string_lossy().into_owned(),
        read_only,
    };
    let mut disks = vec![attach(AttachmentType::VirtualDisk, &files_disk(disk), Some(true))];
    if let Some(installer) = record.installer.as_deref().filter(|_| !record.installed) {
        disks.push(attach(AttachmentType::Iso, installer, None));
    }
    disks
}

/// The card asked for, or the one with the most memory of its own, or none.
pub fn chosen_card(choice: &GpuChoice) -> Result<Option<Card>, String> {
    let cards = gpu::cards().map_err(|error| error.to_string())?;
    let chosen = match choice {
        GpuChoice::None => return Ok(None),
        GpuChoice::Matching(text) => {
            let text = text.to_lowercase();
            cards.iter().find(|card| {
                card.name.to_lowercase().contains(&text) || card.instance_id.to_lowercase().contains(&text)
            })
        }
        GpuChoice::Default => gpu::default_card(&cards),
    };
    match chosen {
        Some(card) => Ok(Some(card.clone())),
        None if cards.is_empty() => {
            Err("No card on this host can be shared with a guest, so it can only boot with none.".into())
        }
        None => Err(format!(
            "No card matches the one asked for. These can be shared: {}",
            cards.iter().map(|card| card.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub(crate) fn need_elevation() -> Result<(), String> {
    if host::is_elevated() {
        Ok(())
    } else {
        Err("This needs an elevated prompt, as HCS, its network and attaching a disk all do. Open Terminal \
             with Run as administrator and run it again."
            .into())
    }
}

/// HCS and DISM resolve no relative paths, so every path they're given is made whole first.
pub fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// The volumes of an attached disk, once Windows shows any.
pub(crate) fn volumes(disk: &VirtualDisk) -> Result<Vec<Volume>, String> {
    let started = Instant::now();
    loop {
        let found = disk.volumes().map_err(|error| error.to_string())?;
        if !found.is_empty() || started.elapsed() > Duration::from_secs(VOLUMES_SECONDS) {
            return Ok(found);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Brings a Windows guest's disk up to date before it boots, which no machine may have open then:
/// the card's driver, the guest's report, which also listens for the host asking it to shut down,
/// and the guest's screen when there's a remoteman installer to give it. `nvapi_wrapper` is whether
/// the guest has the NVAPI wrapper or NVIDIA's own NVAPI.
fn refresh_windows_disk(
    disk: &Path,
    card: Option<&Card>,
    nvapi_wrapper: bool,
    shares: &[Share],
    hosts: Option<&str>,
    sink: &dyn Sink,
) -> Result<(), String> {
    let setup = screen_setup();
    // Attached at every boot, even with nothing else to give, as the share's file may have to go.
    let mut attached = VirtualDisk::open(disk, Kind::Vhdx).map_err(|error| error.to_string())?;
    attached.attach(false).map_err(|error| error.to_string())?;
    let windows = volumes(&attached)?
        .into_iter()
        .filter_map(|volume| GuestVolume::open(&volume.root).ok())
        .find(|volume| volume.is_dir(r"Windows\System32"))
        .ok_or_else(|| format!("{} has no volume with Windows on it.", disk.display()))?;
    // Before anything is written in it, at every start, so a guest made before it has it too.
    give_guest_folder(&windows)?;
    if let Some(card) = card {
        let folder = card.driver_folder.as_ref().ok_or_else(|| {
            format!(
                "The folder of {}'s driver could not be found, so the guest can't be given it.",
                card.name
            )
        })?;
        copy_into(folder, &windows, nvapi_wrapper, sink)?;
    }
    // Written again at every boot, as guests made before it changed have an older one.
    windows.write(crate::guest::REPORT_PATH, crate::guest::REPORT).map_err(|error| error.to_string())?;
    give_share(&windows, shares, sink)?;
    match setup {
        Some(setup) => give_screen(disk, &windows, &setup, card, hosts, sink),
        None => {
            keep_trust(disk, &windows, sink);
            Ok(())
        }
    }
}

/// Makes the guest's Guesthouse folder if it isn't there, and lets only the guest's SYSTEM and
/// administrators change it and what is in it.
pub(crate) fn give_guest_folder(guest: &GuestVolume) -> Result<(), String> {
    guest.give_only(GUESTHOUSE_FOLDER, &GUEST_FOLDER_GRANTS).map_err(|error| error.to_string())
}

/// Tells the guest of its shares, with the marker `report.ps1` reads, and gives it the program that
/// mounts them and WinFsp's installer. With no share the marker goes. A program or installer the
/// host hasn't got is said and leaves the guest without a drive, never without a boot.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn give_share(guest: &GuestVolume, shares: &[Share], sink: &dyn Sink) -> Result<(), String> {
    if shares.is_empty() {
        let _ = guest.remove_file(SHARE_FILE);
        return Ok(());
    }
    guest.write(SHARE_FILE, crate::guest::share_marker(shares)).map_err(|error| error.to_string())?;
    let folder = Path::new(SHARE_FOLDER);
    let beside = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.to_path_buf()));
    for (from, name, what) in [
        (SHARE_PROGRAM, SHARE_PROGRAM_NAME, "Guesthouse's share program"),
        (WINFSP_INSTALLER, WINFSP_INSTALLER_NAME, "WinFsp's installer"),
    ] {
        let source = beside.as_ref().map(|beside| beside.join(from)).filter(|path| path.is_file());
        let Some(source) = source else {
            sink.problem(format!(
                "The guest's shared folder has no drive: {what} ({from}) isn't beside the service."
            ));
            continue;
        };
        let copied = copy_if_changed(&source, guest, &folder.join(name)).map_err(|error| error.to_string());
        match copied {
            Ok(true) => sink.note(format!("The guest has {what}, from {}.", source.display())),
            Ok(false) => {}
            Err(error) => {
                sink.problem(format!("The guest's shared folder has no drive: {what} wasn't copied: {error}"))
            }
        }
    }
    Ok(())
}

/// remoteman's host's installer, beside the program that boots the machine, if it's there.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn screen_setup() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.parent()?.join(screen::SETUP);
    beside.is_file().then_some(beside)
}

/// Copies the host's file `from` to `to` in the guest's Guesthouse folder, unless the copy there
/// has its bytes already: the bytes are compared, not the size and time. Gives whether it copied.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn copy_if_changed(from: &Path, guest: &GuestVolume, to: &Path) -> io::Result<bool> {
    let given = std::fs::read(from)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", from.display())))?;
    let same = guest.read(to, given.len() as u64 + 1).is_ok_and(|kept| kept == given);
    if !same {
        guest.write(to, &given)?;
    }
    Ok(!same)
}

/// Puts what the guest needs for its screen in its screen folder: remoteman's installer, the script
/// that runs it, the devices the guest trusts, the encoder for its card, and its account,
/// so it signs in by itself. The report, written before this, runs the script.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn give_screen(
    disk: &Path,
    guest: &GuestVolume,
    setup: &Path,
    card: Option<&Card>,
    hosts: Option<&str>,
    sink: &dyn Sink,
) -> Result<(), String> {
    let folder = Path::new(screen::GUEST_FOLDER);
    let write =
        |name: &str, text: &str| guest.write(folder.join(name), text).map_err(|error| error.to_string());
    guest.create_dir_all(folder).map_err(|error| error.to_string())?;
    let state = screen::state_beside(disk);
    let viewer = screen::viewer_identity(&state)?;
    give_trust(guest, &viewer)?;
    write("encoder.txt", card.map_or("auto", |card| screen::encoder_for(&card.instance_id)))?;
    write("screen.ps1", screen::SCRIPT)?;
    // Who may reach remoteman's port. Without it the guest leaves its rules as they are.
    match hosts {
        Some(text) => write(screen::HOSTS_FILE, text)?,
        None => {
            let _ = guest.remove_file(folder.join(screen::HOSTS_FILE));
        }
    }
    let account = disk.with_extension("account.txt");
    if let Ok(text) = std::fs::read_to_string(&account) {
        write("logon.txt", &text)?;
    }
    let copied = copy_if_changed(setup, guest, &folder.join(screen::GUEST_SETUP))
        .map_err(|error| error.to_string())?;
    if copied {
        sink.note("remoteman's host's installer given to the guest.");
    }
    // A guest given remoteman's files before its installer could run unattended has them still.
    let _ = guest.remove_tree(folder.join("remoteman-host"));
    sink.note(format!("The guest has remoteman for its screen. The viewer's folder is {}.", state.display()));
    Ok(())
}

/// Writes whom the guest trusts in its screen folder: the viewer's line, which decides whether
/// remoteman's installer runs again, and the list it's given when it does, the viewer and every
/// device the guest has paired, read off its disk now (see `trust`).
fn give_trust(guest: &GuestVolume, viewer: &str) -> Result<(), String> {
    let current = guest.read(trust::GUEST_TRUST_FILE, trust::MOST_LIST_BYTES + 1).ok().map(trust::list_from);
    let given = trust::given_from(viewer, current.as_deref())?;
    for (name, text) in [("viewer.txt", &given.viewer), ("trusted", &given.trusted)] {
        let path = Path::new(screen::GUEST_FOLDER).join(name);
        guest.write(path, text).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// With no installer to give, a guest given one before still runs its script, which runs the
/// installer when remoteman's service has gone. Its list is made again from the guest's own, so the
/// installer never gets the one from an earlier boot, with a device revoked since on it.
fn keep_trust(disk: &Path, guest: &GuestVolume, sink: &dyn Sink) {
    if !guest.is_file(Path::new(screen::GUEST_FOLDER).join("screen.ps1")) {
        return;
    }
    let kept =
        screen::viewer_identity(&screen::state_beside(disk)).and_then(|viewer| give_trust(guest, &viewer));
    if let Err(error) = kept {
        sink.problem(format!("The guest's list of devices to trust wasn't written: {error}"));
    }
}

/// The NVAPI wrapper for NVIDIA guests, in the program's guest folder. Not beside the program, where
/// it would be what the program itself loads for NVAPI.
#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn nvapi_wrapper() -> Option<PathBuf> {
    let file = std::env::current_exe().ok()?.parent()?.join(NVAPI_WRAPPER);
    file.is_file().then_some(file)
}

/// With `nvapi_wrapper` off, NVIDIA's NVAPI goes where the wrapper was, as the placer replaces a
/// file of its own that isn't the driver's.
fn copy_into(folder: &Path, guest: &GuestVolume, nvapi_wrapper: bool, sink: &dyn Sink) -> Result<(), String> {
    let copied = copy_driver(folder, guest).map_err(|error| error.to_string())?;
    let name = folder.file_name().unwrap_or_default().to_string_lossy();
    sink.note(format!("Driver {name}: {} files copied, {} already there.", copied.copied, copied.kept));
    // The files the driver installs outside the driver store, such as its Vulkan and OpenCL loaders
    // and its maker's media libraries. Not having them leaves the card working for Direct3D but
    // not for programs that load those, so a failure is told and the boot goes on.
    let wrapper = if nvapi_wrapper { self::nvapi_wrapper() } else { None };
    match guesthouse_hcs::driver_files::place(folder, guest, wrapper.as_deref()) {
        Ok(placed) => {
            sink.note(format!(
                "Driver {name}'s files outside the driver store: {} copied, {} already there.",
                placed.copied, placed.kept
            ));
            if !placed.left.is_empty() {
                let left: Vec<String> = placed.left.iter().map(|path| path.display().to_string()).collect();
                sink.note(format!("The guest's own copies were kept: {}.", left.join(", ")));
            }
        }
        Err(error) => {
            sink.problem(format!("Driver {name}'s files outside the driver store weren't placed: {error}"))
        }
    }
    Ok(())
}

/// How long Windows may take to let go of a machine's disk once the machine has stopped.
const RELEASE_SECONDS: u64 = 30;

/// Prints the report a Windows guest wrote on its own disk, which says what it saw of its card
/// even when its serial port said nothing.
fn print_guest_report(disk: &Path, sink: &dyn Sink) {
    let started = Instant::now();
    let attached = loop {
        let opened =
            VirtualDisk::open(disk, Kind::Vhdx).and_then(|mut opened| opened.attach(false).map(|()| opened));
        match opened {
            Ok(opened) => break opened,
            Err(_) if started.elapsed() < Duration::from_secs(RELEASE_SECONDS) => {
                thread::sleep(Duration::from_secs(1))
            }
            Err(error) => return sink.problem(format!("The guest's report couldn't be read: {error}")),
        }
    };
    let report = volumes(&attached)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|volume| GuestVolume::open(&volume.root).ok())
        .find(|volume| volume.is_file(crate::guest::REPORT_FILE));
    match report.map(|volume| volume.read(crate::guest::REPORT_FILE, MOST_REPORT_BYTES)) {
        Some(Ok(bytes)) => {
            sink.note("The guest's report, from its disk:");
            for line in bytes.split(|&byte| byte == b'\n') {
                sink.event(Event::Report(crate::printable::printable(line)));
            }
        }
        Some(Err(error)) => sink.problem(format!("The guest's report couldn't be read: {error}")),
        None => sink.note(
            "The guest wrote no report on its disk, so it never got as far as its first sign-in screen.",
        ),
    }
}

/// A program of Windows' own, by its full path, so nothing else on PATH can stand in for it.
fn system_program(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Kept;

    const PAIRED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A folder standing in for the machine's disk beside its viewer's state, and its Windows volume.
    fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("gh-give-trust-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("volume");
        std::fs::create_dir_all(&root).unwrap();
        (base.join("machine.vhdx"), root.join(screen::GUEST_FOLDER), root)
    }

    #[test]
    fn the_guest_is_given_the_viewers_line_and_every_device_it_trusts() {
        let (disk, folder, root) = scratch("given");
        let viewer = screen::viewer_identity(&screen::state_beside(&disk)).unwrap();
        let live = root.join(trust::GUEST_TRUST_FILE);
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, format!("{viewer} Guesthouse\r\n{PAIRED} Kitchen tablet\r\n")).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        give_trust(&GuestVolume::open(&root).unwrap(), &viewer).unwrap();
        assert_eq!(std::fs::read_to_string(folder.join("viewer.txt")).unwrap(), screen::trust_list(&viewer));
        assert_eq!(
            std::fs::read_to_string(folder.join("trusted")).unwrap(),
            format!("{viewer} Guesthouse\n{PAIRED} Kitchen tablet\n")
        );
        let _ = std::fs::remove_dir_all(disk.parent().unwrap());
    }

    #[test]
    fn with_no_installer_a_guest_given_a_screen_before_has_its_list_made_again() {
        let (disk, folder, root) = scratch("kept");
        let sink = Kept::default();
        let guest = GuestVolume::open(&root).unwrap();
        keep_trust(&disk, &guest, &sink);
        assert!(
            !folder.exists() && !screen::state_beside(&disk).exists(),
            "a guest with no screen is left alone"
        );
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("screen.ps1"), screen::SCRIPT).unwrap();
        std::fs::write(folder.join("trusted"), format!("{PAIRED} Revoked since\n")).unwrap();
        keep_trust(&disk, &guest, &sink);
        let viewer = screen::viewer_identity(&screen::state_beside(&disk)).unwrap();
        assert_eq!(std::fs::read_to_string(folder.join("trusted")).unwrap(), screen::trust_list(&viewer));
        assert_eq!(std::fs::read_to_string(folder.join("viewer.txt")).unwrap(), screen::trust_list(&viewer));
        assert!(sink.0.lock().unwrap().is_empty());
        drop(guest);
        let _ = std::fs::remove_dir_all(disk.parent().unwrap());
    }

    #[test]
    fn a_linux_guest_has_its_files_disk_read_only() {
        let record = Record {
            guest: Guest::Linux,
            installer: Some(PathBuf::from(r"D:\ubuntu.iso")),
            ..Record::default()
        };
        let disks = more_disks(Path::new(r"D:\machines\ubuntu.vhdx"), &record);
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].path, r"D:\machines\ubuntu.files.vhdx");
        assert_eq!(disks[0].read_only, Some(true));
    }

    /// A junction at `link` to the folder `target`, as anyone may make one.
    pub(super) fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J: {}", String::from_utf8_lossy(&made.stderr));
    }

    #[test]
    fn a_screen_folder_that_is_a_link_gets_nothing_written_through_it() {
        let (disk, folder, root) = scratch("linked");
        let elsewhere = disk.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("screen.ps1"), screen::SCRIPT).unwrap();
        std::fs::create_dir_all(folder.parent().unwrap()).unwrap();
        junction(&folder, &elsewhere);
        let guest = GuestVolume::open(&root).unwrap();
        keep_trust(&disk, &guest, &Kept::default());
        let names: Vec<_> =
            std::fs::read_dir(&elsewhere).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(names, ["screen.ps1"], "nothing was written through the junction");
        drop(guest);
        let _ = std::fs::remove_dir_all(disk.parent().unwrap());
    }

    #[test]
    fn only_the_guests_system_and_administrators_may_change_its_guesthouse_folder() {
        let writers: Vec<&str> = GUEST_FOLDER_GRANTS
            .iter()
            .filter(|(_, rights)| *rights != "RC")
            .map(|(trustee, _)| *trustee)
            .collect();
        assert_eq!(writers, ["SY", "BA"]);
        assert!(
            GUEST_FOLDER_GRANTS.contains(&("OW", "RC")),
            "an owner gets no more than reading permissions"
        );
    }
}
