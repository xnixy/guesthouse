//! The library of machines: making one, importing the first, and taking them away.

use crate::account;
use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::gpu_maker::GpuMaker;
use crate::gpu_support::{NO_GPU_PART, gpu_ready};
use crate::host::{free_gib, host_ram_limit, plain_ram_limit};
use crate::machine::{Kind, Machine, load_all, sorted_names};
use crate::monitor::{Command, failed};
use crate::network;
use crate::settings::with_suffix;
use crate::setup::guest_driver::guest_driver;
use crate::snapshots::{list_snapshots, snapshots_folder};
use crate::tasks::{State, slot_of, status};
use serde_json::json;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process;

/// The entry made for the VM from before the library. The user can remove it, and this name in a
/// mark file then keeps it from being imported again.
pub const EXISTING_ID: &str = "windows-11";
const REMOVED_MARK: &str = ".existing-removed";
pub const MIN_WINDOWS_DISK_GIB: u32 = 64;
pub const MIN_PLAIN_DISK_GIB: u32 = 1;
pub const MAX_DISK_GIB: u32 = 16384;
/// A finished install, and the install disc beside it.
pub const INSTALL_ROOM_GIB: u64 = 40;
pub const NO_DRIVER_FILES: &str = "Windows needs NVIDIA's driver first. Run Setup Again from the menu.";
const SHUT_DOWN_FIRST: &str = "Shut the VM down first.";
const NO_FIRMWARE: &str = "QEMU's UEFI firmware isn't installed. Install ovmf, which Fedora calls edk2-ovmf.";
/// The file a disk is called in its machine's folder.
const DISK_FILE: &str = "disk.qcow2";
const FIRMWARE_FILE: &str = "vars.fd";

/// Makes the first entry from the VM from before the library. Its files stay where they are.
/// Anything in the folder stops this, which includes the mark that the user removed this entry.
pub fn import_existing(context: &Context) -> Result<Option<Machine>> {
    if !sorted_names(&context.paths.machines).is_empty() {
        return Ok(None);
    }
    let disk = context.paths.store.join("disk/win.qcow2");
    if !disk.exists() {
        return Ok(None);
    }
    let firmware = context.paths.store.join("disk/vars.fd");
    let machine =
        Machine::new(EXISTING_ID, "Windows 11", &disk.to_string_lossy(), &firmware.to_string_lossy());
    machine.save(context)?;
    Ok(Some(machine))
}

/// A folder name from the machine's name, which can hold anything.
pub fn new_id(context: &Context, name: &str) -> String {
    let lower = name.to_lowercase();
    let words = lower.split(|letter: char| !letter.is_ascii_alphanumeric()).filter(|word| !word.is_empty());
    let base = words.collect::<Vec<_>>().join("-");
    let base = if base.is_empty() { "machine".to_owned() } else { base };
    let taken = sorted_names(&context.paths.machines);
    let mut found = base.clone();
    let mut number = 1;
    while taken.contains(&found) {
        number += 1;
        found = format!("{base}-{number}");
    }
    found
}

pub fn unused_mac(context: &Context) -> String {
    let machines = load_all(context).0;
    network::new_mac(machines.iter().map(|machine| machine.mac.as_str()))
}

/// What a new machine is to be.
#[derive(Debug, Clone)]
pub struct NewMachine {
    pub name: String,
    pub iso: String,
    pub kind: Kind,
    pub cores: u32,
    pub ram_gib: u32,
    pub disk_gib: u32,
    /// The guest account's name, or empty for the one the scripts have always made.
    pub user: String,
    /// Its password, or empty for a random one.
    pub password: String,
    /// The AMD card its GPU runs on, by PCI address, or empty for the NVIDIA card. None takes this
    /// PC's usual choice.
    pub amd_gpu: Option<String>,
    /// Whose card that is, for a Windows machine. None is what the address says.
    pub gpu_maker: Option<GpuMaker>,
}

/// Says why this new machine can't be made now.
pub fn create_problems(context: &Context, new: &NewMachine, machine: &Machine) -> Result<Vec<String>> {
    let mut found = machine.problems(crate::settings::max_vram_gib(context));
    found.extend(account::password_problem(&new.password));
    let disk_gib = new.disk_gib;
    let least = if machine.kind == Kind::Windows { MIN_WINDOWS_DISK_GIB } else { MIN_PLAIN_DISK_GIB };
    if !(least..=MAX_DISK_GIB).contains(&disk_gib) {
        found.push(format!("The disk must be between {least} and {MAX_DISK_GIB} GiB."));
    }
    if !Path::new(&machine.iso).is_file() {
        found.push("Choose an ISO first.".to_owned());
    }
    // What counts is what it will boot with after the install, and Windows needs more then.
    let limit =
        if machine.kind == Kind::Windows { host_ram_limit(context)? } else { plain_ram_limit(context) };
    if u64::from(machine.ram_gib) > limit {
        found.push(format!(
            "This PC has room for {limit} GiB right now. Give the machine that much, or stop another VM."
        ));
    }
    let firmware = &context.paths.firmware;
    if !machine.vars.is_empty()
        && !(if machine.tpm { &firmware.vars_secure } else { &firmware.vars }).exists()
    {
        found.push(NO_FIRMWARE.to_owned());
    }
    fs::create_dir_all(&context.paths.machines)?;
    if machine.kind == Kind::Windows && !gpu_ready(context) {
        found.push(NO_GPU_PART.to_owned());
    }
    if machine.kind == Kind::Windows {
        let room = free_gib(&context.paths.machines)?;
        if room < context.install_room_gib() as f64 {
            found.push(format!(
                "An install needs {} GiB free here, and there is {room:.0} GiB.",
                context.install_room_gib()
            ));
        }
        if let Err(missing) = guest_driver(context, machine) {
            found.push(missing.to_owned());
        }
    }
    Ok(found)
}

pub fn make_disk(context: &Context, machine: &Machine, gib: u32) -> Result<()> {
    let result = process::Command::new("qemu-img")
        .args(["create", "-q", "-f", "qcow2", &machine.disk, &format!("{gib}G")])
        .output()?;
    if !result.status.success() {
        let said = String::from_utf8_lossy(&result.stderr);
        return Err(Failure(format!("The disk could not be made: {}", said.trim())));
    }
    let firmware = &context.paths.firmware;
    if !machine.vars.is_empty() {
        fs::copy(if machine.tpm { &firmware.vars_secure } else { &firmware.vars }, &machine.vars)?;
    }
    let _ = fs::remove_dir_all(machine.tpm_folder(context));
    Ok(())
}

/// Makes a machine's folder, disk and firmware file. Installing Windows is a task of its own.
pub fn create(context: &Context, new: &NewMachine) -> Result<Machine> {
    let id = new_id(context, &new.name);
    let folder = context.paths.machines.join(&id);
    let disk = folder.join(DISK_FILE);
    let firmware = folder.join(FIRMWARE_FILE);
    let windows = new.kind == Kind::Windows;
    let installs = windows || new.kind == Kind::Linux;
    // Ubuntu's firmware is in QEMU, so it has no settings file.
    let firmware =
        if new.kind == Kind::Linux { String::new() } else { firmware.to_string_lossy().into_owned() };
    let machine = Machine {
        cores: new.cores,
        ram_gib: new.ram_gib,
        kind: new.kind,
        iso: new.iso.clone(),
        installing: installs,
        tpm: windows,
        mac: unused_mac(context),
        amd_gpu: new.amd_gpu.clone().unwrap_or_else(|| crate::linux::amd::default_linux_gpu(context)),
        gpu_maker: new.gpu_maker.and_then(GpuMaker::recorded).filter(|_| windows),
        user: if installs { new.user.trim().to_owned() } else { String::new() },
        ..Machine::new(&id, new.name.trim(), &disk.to_string_lossy(), &firmware)
    };
    if let Some(problem) = create_problems(context, new, &machine)?.into_iter().next() {
        return Err(Failure(problem));
    }
    machine.save(context)?;
    let made = if installs && !new.password.is_empty() {
        store_password(&machine.password_file(context), &new.password)
    } else {
        Ok(())
    };
    if let Err(failure) = made.and_then(|()| make_disk(context, &machine, new.disk_gib)) {
        let _ = fs::remove_dir_all(&folder);
        return Err(failure);
    }
    Ok(machine)
}

/// The password the user chose, where the install will look for one and keep it: a file only this
/// user can read, the same as the random one it would otherwise make.
fn store_password(file: &Path, password: &str) -> Result<()> {
    if let Some(folder) = file.parent() {
        fs::create_dir_all(folder)?;
    }
    let mut new = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(file)?;
    writeln!(new, "{password}")?;
    Ok(())
}

/// Says why this machine's files can't go now. A running VM or task still needs them.
fn removal_problem(context: &Context, machine: &Machine) -> Option<Failure> {
    let current = status(context, machine);
    (current.state != State::Off || current.busy).then(|| Failure::new(SHUT_DOWN_FIRST))
}

/// Deletes the machine's folder, with its disk. A disk outside the folder is never touched, and
/// such a machine can only lose its entry.
pub fn delete(context: &Context, machine: &Machine) -> Result<()> {
    if let Some(problem) = removal_problem(context, machine) {
        return Err(problem);
    }
    if !machine.owns_files(context) {
        return Err(Failure::new(
            "This machine's disk is not in its folder, so only its entry can be removed.",
        ));
    }
    let folder = machine.folder(context);
    let linked = folder.is_symlink();
    let real_parent = fs::canonicalize(&folder).ok().and_then(|real| Some(real.parent()?.to_owned()));
    if linked || real_parent != fs::canonicalize(&context.paths.machines).ok() {
        return Err(Failure::new("This machine's folder is not where it should be, so it was left alone."));
    }
    fs::remove_dir_all(folder)?;
    Ok(())
}

/// Forgets a machine whose files live elsewhere. Only its own settings file goes.
pub fn remove_entry(context: &Context, machine: &Machine) -> Result<()> {
    if let Some(problem) = removal_problem(context, machine) {
        return Err(problem);
    }
    if machine.owns_files(context) {
        return Err(Failure::new("This machine's disk is in its folder. Delete the machine instead."));
    }
    if !list_snapshots(context, machine).is_empty() {
        return Err(Failure::new("Delete its snapshots first. They are inside the disk, which stays."));
    }
    let _ = fs::remove_dir_all(snapshots_folder(context, machine));
    let thumbnail = machine.thumbnail(context);
    for path in [with_suffix(&thumbnail, ".new"), thumbnail] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let folder = machine.folder(context);
    fs::remove_file(folder.join("machine.json"))?;
    // Anything else in there isn't for here to remove, and then the folder stays.
    let _ = fs::remove_dir(&folder);
    if machine.id == EXISTING_ID {
        File::create(context.paths.machines.join(REMOVED_MARK))?;
    }
    Ok(())
}

/// Takes the ISO out, at once if the VM is running, so that it boots from its disk.
pub fn eject(context: &Context, machine: &mut Machine) -> Result<()> {
    if status(context, machine).state == State::Running {
        let command = Command::new("eject", json!({"device": "cd0", "force": true}));
        if failed(&context.ask_monitor(slot_of(context, machine), &command)) {
            return Err(Failure::new("QEMU would not eject the ISO."));
        }
    }
    machine.iso.clear();
    machine.save(context)
}

/// Only for this user, as it shows what was on the guest's screen.
pub fn save_thumbnail(context: &Context, machine: &Machine, png: &[u8]) -> Result<()> {
    const PRIVATE: u32 = 0o600;
    let path = machine.thumbnail(context);
    let new = with_suffix(&path, ".new");
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(PRIVATE).open(&new)?;
    file.set_permissions(fs::Permissions::from_mode(PRIVATE))?;
    file.write_all(png)?;
    fs::rename(new, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::load;
    use crate::monitor::stand_in::RecordingMonitor;
    use crate::processes::stand_in::fake_process;
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::sync::Arc;

    fn plain(scratch: &Scratch, name: &str, iso: Option<&str>) -> NewMachine {
        let iso = iso.map_or_else(|| scratch.write("linux.iso", "disc").display().to_string(), str::to_owned);
        NewMachine {
            name: name.into(),
            iso,
            kind: Kind::Plain,
            cores: 1,
            ram_gib: 1,
            disk_gib: 2,
            user: String::new(),
            password: String::new(),
            amd_gpu: None,
            gpu_maker: None,
        }
    }

    fn import(scratch: &Scratch, context: &Context) -> Machine {
        scratch.write("home/.local/share/guesthouse/disk/win.qcow2", "user data");
        scratch.write("home/.local/share/guesthouse/disk/vars.fd", "user data");
        import_existing(context).unwrap().unwrap()
    }

    /// Makes the running VM use this machine's disk.
    fn run(context: &Context, machine: &Machine) {
        let qemu = context.paths.qemu.display().to_string();
        let socket =
            format!("unix:{}/qmp.sock,server=on", context.paths.run_folder(slots::first()).display());
        let drive = format!("file={},format=qcow2", machine.disk);
        fake_process(context, 60, &[&qemu, "-qmp", &socket, "-drive", &drive], &[], "");
    }

    #[test]
    fn a_chosen_account_is_kept_for_the_install_and_a_bad_one_refused() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let iso = scratch.write("ubuntu.iso", "disc").display().to_string();
        let new = NewMachine {
            name: "Ubuntu".into(),
            iso,
            kind: Kind::Linux,
            cores: 2,
            ram_gib: 2,
            disk_gib: 20,
            user: "ana".into(),
            password: "chosen".into(),
            amd_gpu: None,
            gpu_maker: None,
        };
        let made = create(&context, &new).unwrap();
        assert_eq!(made.account(), "ana");
        let file = made.password_file(&context);
        assert_eq!(fs::read_to_string(&file).unwrap(), "chosen\n");
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600, "the password is this user's alone");
        let refused = create(&context, &NewMachine { name: "Two".into(), user: "Ana".into(), ..new.clone() });
        assert!(refused.unwrap_err().0.contains("user name"));
        let refused =
            create(&context, &NewMachine { name: "Two".into(), password: "a\nb".into(), ..new.clone() });
        assert!(refused.unwrap_err().0.contains("password"));
        assert!(!context.paths.machines.join("two").exists(), "a refused machine leaves nothing");
        let random =
            create(&context, &NewMachine { name: "Three".into(), password: String::new(), ..new }).unwrap();
        assert!(!random.password_file(&context).exists(), "the install makes a random one later");
    }

    #[test]
    fn the_import_takes_the_scripts_values_and_leaves_the_files_alone() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        assert_eq!(import_existing(&context).unwrap(), None, "no disk, so nothing to import");
        let mut made = import(&scratch, &context);
        let disk = context.paths.store.join("disk/win.qcow2");
        assert_eq!((made.cores, made.ram_gib, made.refresh_hz, made.vcache_pin), (8, 16, 144, false));
        assert_eq!(made.disk, disk.display().to_string());
        assert_eq!(fs::read(&disk).unwrap(), b"user data");
        made.cores = 12;
        made.save(&context).unwrap();
        assert_eq!(import_existing(&context).unwrap(), None, "a second run must not import again");
        assert_eq!(load(&context, EXISTING_ID).unwrap().cores, 12);
    }

    #[test]
    fn a_broken_file_is_never_replaced_by_the_import() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        import(&scratch, &context);
        fs::write(context.paths.machines.join("windows-11/machine.json"), "{").unwrap();
        assert_eq!(import_existing(&context).unwrap(), None);
    }

    #[test]
    fn a_plain_machine_gets_a_folder_disk_and_firmware() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let made = create(&context, &plain(&scratch, " Tiny, Linux! ", None)).unwrap();
        assert_eq!((made.id.as_str(), made.name.as_str()), ("tiny-linux", "Tiny, Linux!"));
        assert!(made.owns_files(&context) && !made.installing && !made.has_gpu() && !made.tpm);
        assert_eq!(fs::read_to_string(&made.vars).unwrap(), "blank firmware settings");
        assert_eq!(load(&context, "tiny-linux").unwrap(), made);
        let second = create(&context, &plain(&scratch, "Tiny Linux", None)).unwrap();
        assert_eq!(second.id, "tiny-linux-2", "a second machine with this name gets its own folder");
        assert_eq!(new_id(&context, "???"), "machine");
    }

    #[test]
    fn an_install_that_wouldnt_fit_offers_a_number_that_would() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.install_room = Some(0);
        context.ram_limit = Some(6);
        fs::create_dir_all(crate::settings::driver_folder(&context)).unwrap();
        let iso = scratch.write("win.iso", "disc").display().to_string();
        let new = NewMachine {
            name: "Win".into(),
            iso,
            kind: Kind::Windows,
            cores: 8,
            ram_gib: 16,
            disk_gib: 80,
            user: String::new(),
            password: String::new(),
            amd_gpu: None,
            gpu_maker: None,
        };
        assert!(create(&context, &new).unwrap_err().0.contains("room for 6 GiB"));
    }

    #[test]
    fn with_no_firmware_it_says_what_to_install() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.paths.firmware.vars = scratch.path("not-installed");
        let iso = scratch.write("disc.iso", "disc").display().to_string();
        let new = NewMachine {
            name: "VM".into(),
            iso,
            kind: Kind::Plain,
            cores: 2,
            ram_gib: 2,
            disk_gib: 4,
            user: String::new(),
            password: String::new(),
            amd_gpu: None,
            gpu_maker: None,
        };
        assert_eq!(create(&context, &new).unwrap_err().0, NO_FIRMWARE);
    }

    #[test]
    fn a_new_windows_machine_installs_with_a_tpm() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.install_room = Some(0);
        fs::create_dir_all(crate::settings::driver_folder(&context)).unwrap();
        let iso = scratch.write("win.iso", "disc").display().to_string();
        let new = NewMachine {
            name: "Win".into(),
            iso,
            kind: Kind::Windows,
            cores: 2,
            ram_gib: 4,
            disk_gib: 64,
            user: String::new(),
            password: String::new(),
            amd_gpu: None,
            gpu_maker: None,
        };
        let made = create(&context, &new).unwrap();
        assert!(made.installing && made.tpm && !made.has_gpu());
        assert_eq!(fs::read_to_string(&made.vars).unwrap(), "firmware settings with keys");
        for (change, text) in [
            (NewMachine { cores: 1, ..new.clone() }, "Cores"),
            (NewMachine { ram_gib: 3, ..new.clone() }, "RAM"),
            (NewMachine { disk_gib: 63, ..new.clone() }, "disk"),
            (NewMachine { name: " ".into(), ..new.clone() }, "name"),
            (NewMachine { iso: scratch.path("gone.iso").display().to_string(), ..new.clone() }, "ISO"),
        ] {
            let before = sorted_names(&context.paths.machines);
            assert!(create(&context, &change).unwrap_err().0.contains(text));
            assert_eq!(sorted_names(&context.paths.machines), before, "a refused machine leaves nothing");
        }
    }

    #[test]
    fn ubuntu_installs_with_no_firmware_file_and_no_windows_driver() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let iso = scratch.write("server.iso", "disc").display().to_string();
        let new = NewMachine {
            name: "Ubuntu".into(),
            iso,
            kind: Kind::Linux,
            cores: 8,
            ram_gib: 8,
            disk_gib: 2,
            user: String::new(),
            password: String::new(),
            amd_gpu: None,
            gpu_maker: None,
        };
        let made = create(&context, &new).unwrap();
        assert!(made.installing && !made.tpm && made.vars.is_empty());
        assert!(made.owns_files(&context));
    }

    #[test]
    fn windows_needs_the_drivers_files() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.install_room = Some(0);
        let iso = scratch.write("win.iso", "disc").display().to_string();
        let new = NewMachine {
            name: "Win".into(),
            iso,
            kind: Kind::Windows,
            cores: 2,
            ram_gib: 4,
            disk_gib: 64,
            user: String::new(),
            password: String::new(),
            amd_gpu: None,
            gpu_maker: None,
        };
        assert_eq!(create(&context, &new).unwrap_err().0, NO_DRIVER_FILES);
    }

    #[test]
    fn a_disk_that_cannot_be_made_says_why() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = Machine { kind: Kind::Plain, ..Machine::new("x", "x", "/nowhere/d", "/nowhere/v") };
        assert!(make_disk(&context, &machine, 1).unwrap_err().0.contains("could not be made"));
    }

    #[test]
    fn an_imported_disk_is_never_deleted() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let imported = import(&scratch, &context);
        let disk = fs::metadata(&imported.disk).unwrap().ino();
        assert!(!imported.owns_files(&context));
        assert!(delete(&context, &imported).unwrap_err().0.contains("only its entry"));
        remove_entry(&context, &imported).unwrap();
        assert!(!imported.folder(&context).exists());
        assert_eq!(fs::metadata(&imported.disk).unwrap().ino(), disk);
        assert_eq!(import_existing(&context).unwrap(), None, "a removed entry does not come back by itself");
    }

    #[test]
    fn a_disk_in_the_folder_only_through_a_link_is_not_the_machines() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let outside = scratch.write("outside.qcow2", "user data");
        let folder = context.paths.machines.join("linked");
        let disk = folder.join("disk.qcow2");
        let linked = Machine::new("linked", "Linked", &disk.to_string_lossy(), &outside.to_string_lossy());
        linked.save(&context).unwrap();
        symlink(&outside, &disk).unwrap();
        assert!(!linked.owns_files(&context));
        assert!(delete(&context, &linked).unwrap_err().0.contains("only its entry"));
        remove_entry(&context, &linked).unwrap();
        assert!(disk.is_symlink(), "what it does not own stays");
    }

    #[test]
    fn a_folder_that_is_a_link_is_left_alone() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        fs::create_dir_all(&context.paths.machines).unwrap();
        let outside = scratch.path("outside");
        fs::create_dir_all(&outside).unwrap();
        let folder = context.paths.machines.join("moved");
        symlink(&outside, &folder).unwrap();
        let (disk, firmware) = (folder.join("disk.qcow2"), folder.join("vars.fd"));
        fs::write(&disk, "data").unwrap();
        fs::write(&firmware, "data").unwrap();
        let moved = Machine::new("moved", "Moved", &disk.to_string_lossy(), &firmware.to_string_lossy());
        assert!(delete(&context, &moved).unwrap_err().0.contains("left alone"));
        assert_eq!(sorted_names(&outside), ["disk.qcow2", "vars.fd"]);
    }

    #[test]
    fn a_running_machine_keeps_its_files() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let made = create(&context, &plain(&scratch, "Busy", None)).unwrap();
        run(&context, &made);
        assert!(delete(&context, &made).unwrap_err().0.contains("Shut the VM down"));
        assert!(remove_entry(&context, &made).unwrap_err().0.contains("Shut the VM down"));
        assert!(Path::new(&made.disk).exists(), "a refused delete takes nothing");
        fs::remove_dir_all(context.processes.join("60")).unwrap();
        delete(&context, &made).unwrap();
        assert!(!made.folder(&context).exists());
    }

    #[test]
    fn eject_reaches_a_running_vm_and_a_refusal_changes_nothing() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        let monitor = Arc::new(RecordingMonitor::answering(Some(json!({"return": {}}))));
        context.monitor = monitor.clone();
        let iso = plain(&scratch, "Eject", None).iso.clone();
        let mut machine = create(&context, &plain(&scratch, "Eject", None)).unwrap();
        eject(&context, &mut machine).unwrap();
        assert!(monitor.asked().is_empty(), "with the VM off, this is only the setting");
        assert_eq!(load(&context, &machine.id).unwrap().iso, "");
        machine.iso = iso.clone();
        run(&context, &machine);
        eject(&context, &mut machine).unwrap();
        assert_eq!(monitor.asked()[0].name, "eject");
        machine.iso = iso.clone();
        machine.save(&context).unwrap();
        monitor.set_answer(Some(json!({"error": {"desc": "locked"}})));
        assert!(eject(&context, &mut machine).unwrap_err().0.contains("would not eject"));
        assert_eq!(load(&context, &machine.id).unwrap().iso, iso, "a failed eject changes nothing");
    }

    #[test]
    fn a_thumbnail_is_for_this_user_only() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let imported = import(&scratch, &context);
        save_thumbnail(&context, &imported, b"png one").unwrap();
        save_thumbnail(&context, &imported, b"png two").unwrap();
        let path = imported.thumbnail(&context);
        assert_eq!(fs::read(&path).unwrap(), b"png two");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(!with_suffix(&path, ".new").exists());
        remove_entry(&context, &imported).unwrap();
        assert!(!imported.folder(&context).exists(), "it goes with the entry");
    }
}
