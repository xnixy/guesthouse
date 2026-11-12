//! The host's drives: which ones a VM can be given, what the host is using them for, and handing one to
//! QEMU open.

use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::machine::{Drive, Machine, sorted_names};
use crate::usb::sysfs_text;
use guesthouse_daemon::handover::{KEPT_ABOVE, leave_open_at};
use rustix::fs::{Access, OFlags, access};
use rustix::io::{Errno, fcntl_dupfd_cloexec};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where QEMU finds the first drive open. The memory is at 12 and 13, the network at 10 and 11 and the
/// unix sockets a jail's launcher opens from 20, so a drive's numbers are clear of all three.
pub const FIRST_DRIVE_FD: RawFd = 40;
/// A sysfs size is always in these, whatever the drive's own sectors are.
const SECTOR_BYTES: u64 = 512;
/// Drives are sold in thousands, not in powers of two.
const KB: f64 = 1000.0;
/// Folders in sysfs that aren't drives anyone gives a VM: loop devices, RAM disks, volumes made of
/// others, and optical drives.
const NOT_OFFERED: [&str; 6] = ["loop", "ram", "zram", "dm-", "md", "sr"];
/// Names that are the same drive's hardware number again. The one with its model and serial reads better.
const NUMBER_NAMES: [&str; 2] = ["wwn-", "nvme-eui."];
const PARTITION_FILE: &str = "partition";
/// What udev's rule and `udevadm` can be told without quoting.
const RULE_VALUE_PUNCTUATION: &str = "_-.:+";

/// A disk of this PC. Only a whole disk can be given: Windows can't use a partition without its disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDrive {
    /// Its name under `/dev/disk/by-id`, which stays with it from one boot to the next.
    pub id: String,
    /// What the kernel calls it now, as `sdb` or `nvme0n1`.
    pub kernel_name: String,
    pub bytes: u64,
    pub model: String,
    pub removable: bool,
    /// Why the host can't let it go, as the end of a sentence about it: it, or a partition of it, is
    /// mounted, swap, or held by a volume. None if nothing uses it.
    pub in_use: Option<String>,
    /// What udev matches the disk by. Empty if it has none.
    pub serial: String,
}

impl HostDrive {
    /// Its size as the maker would say it, as `1.0 TB` or `500 GB`.
    pub fn size_text(&self) -> String {
        let bytes = self.bytes as f64;
        if bytes >= KB.powi(4) {
            format!("{:.1} TB", bytes / KB.powi(4))
        } else if bytes >= KB.powi(3) {
            format!("{:.0} GB", bytes / KB.powi(3))
        } else {
            format!("{:.0} MB", bytes / KB.powi(2))
        }
    }

    /// What to call it: its make and size.
    pub fn title(&self) -> String {
        let model = if self.model.is_empty() { "Disk" } else { &self.model };
        format!("{model}, {}", self.size_text())
    }

    /// The one step that needs root. uaccess gives the drive to whoever is logged in. It's matched by the
    /// serial number, as `sdb` and its like change between boots, and only for the disk: the partitions
    /// in it carry the serial too.
    pub fn access_command(&self) -> Result<String> {
        let value = &self.serial;
        let plain = |text: &String| {
            text.chars()
                .all(|letter| letter.is_ascii_alphanumeric() || RULE_VALUE_PUNCTUATION.contains(letter))
        };
        if value.is_empty() || !plain(value) {
            return Err(Failure(format!("{} has no number that tells it apart for a rule.", self.id)));
        }
        let rule = format!(
            "SUBSYSTEM==\"block\", ENV{{DEVTYPE}}==\"disk\", ENV{{ID_SERIAL}}==\"{value}\", TAG+=\"uaccess\""
        );
        Ok(format!(
            "echo '{rule}' | sudo tee /etc/udev/rules.d/70-guesthouse-drive-{value}.rules\n\
             sudo udevadm control --reload\n\
             sudo udevadm trigger --subsystem-match=block --property-match=ID_SERIAL={value}"
        ))
    }
}

/// Whether a kernel name is a drive worth offering.
pub fn is_offered(kernel_name: &str) -> bool {
    !NOT_OFFERED.iter().any(|start| kernel_name.starts_with(start))
}

/// One line of the mount table.
struct Mount {
    /// Major and minor number, as `8:1`.
    device: String,
    source: String,
    point: String,
}

/// What the host holds drives for, read once.
struct Uses {
    mounts: Vec<Mount>,
    swaps: Vec<String>,
}

impl Uses {
    fn read(context: &Context) -> Self {
        let table = fs::read_to_string(&context.mount_table).unwrap_or_default();
        let swaps = fs::read_to_string(&context.swaps).unwrap_or_default();
        Self {
            mounts: table.lines().filter_map(mount_of).collect(),
            // The first line says what the columns are.
            swaps: swaps
                .lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().next())
                .map(str::to_owned)
                .collect(),
        }
    }

    /// What the drive in `folder` is used for by itself, if anything.
    fn own_use(&self, context: &Context, name: &str, folder: &Path) -> Option<String> {
        let device = sysfs_text(&folder.join("dev"));
        let node = format!("/dev/{name}");
        // The source is also checked, as a filesystem that spans devices shows a number of its own.
        if let Some(mount) = self.mounts.iter().find(|mount| mount.device == device || mount.source == node) {
            return Some(format!("mounted at {}", mount.point));
        }
        if self.swaps.contains(&node) {
            return Some("used for swap".to_owned());
        }
        let holders = sorted_names(&folder.join("holders"));
        let named: Vec<String> = holders.iter().map(|holder| volume_name(context, holder)).collect();
        (!named.is_empty()).then(|| format!("held by {}", named.join(", ")))
    }
}

fn mount_of(line: &str) -> Option<Mount> {
    let (before, after) = line.split_once(" - ")?;
    let mut fields = before.split(' ');
    let device = fields.nth(2)?.to_owned();
    let point = unescape(fields.nth(1)?);
    let source = after.split(' ').nth(1)?.to_owned();
    Some(Mount { device, source, point })
}

/// The mount table writes a space, a tab, a newline and a backslash as their codes in octal.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut text = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let code = bytes.get(at + 1..at + 4).filter(|_| bytes[at] == b'\\').and_then(|digits| {
            let digits = std::str::from_utf8(digits).ok()?;
            u8::from_str_radix(digits, 8).ok()
        });
        match code {
            Some(byte) => {
                text.push(byte);
                at += 4;
            }
            None => {
                text.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&text).into_owned()
}

/// What a volume made of a drive is called, which is the name its owner gave it if it has one.
fn volume_name(context: &Context, holder: &str) -> String {
    let name = sysfs_text(&context.block_devices.join(holder).join("dm/name"));
    if name.is_empty() { holder.to_owned() } else { name }
}

/// The names under `/dev/disk/by-id`, by the kernel name each leads to.
fn names_by_kernel_name(context: &Context) -> BTreeMap<String, Vec<String>> {
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in sorted_names(&context.drive_ids) {
        let target = fs::read_link(context.drive_ids.join(&name)).ok();
        let kernel_name = target.as_deref().and_then(Path::file_name).map(|found| found.to_string_lossy());
        if let Some(kernel_name) = kernel_name {
            found.entry(kernel_name.into_owned()).or_default().push(name);
        }
    }
    found
}

/// The name to show and keep: one with the model and serial in it, if there is one.
fn best_name(names: &[String]) -> Option<&String> {
    let readable = names.iter().find(|name| !NUMBER_NAMES.iter().any(|start| name.starts_with(start)));
    readable.or(names.first())
}

/// What udev knows of a block device, by the major and minor number in `dev`.
fn udev_value(context: &Context, device: &str, property: &str) -> String {
    let data = fs::read_to_string(context.udev_data.join(format!("b{device}"))).unwrap_or_default();
    let prefix = format!("E:{property}=");
    let found = data.lines().find_map(|line| line.strip_prefix(prefix.as_str()));
    found.unwrap_or_default().to_owned()
}

/// Every disk that can be offered. One with no name under `/dev/disk/by-id` isn't, as there is nothing
/// to keep it by. Its partitions aren't drives of their own, but what the host uses them for is the
/// disk's.
pub fn host_drives(context: &Context) -> Vec<HostDrive> {
    let names = names_by_kernel_name(context);
    let uses = Uses::read(context);
    let mut found = Vec::new();
    for disk in sorted_names(&context.block_devices) {
        let folder = context.block_devices.join(&disk);
        // The kernel lists partitions beside their disk as well, and they're found inside it.
        if !is_offered(&disk) || folder.join(PARTITION_FILE).exists() {
            continue;
        }
        let Some(id) = names.get(&disk).and_then(|names| best_name(names)) else { continue };
        let bytes = sysfs_text(&folder.join("size")).parse::<u64>().unwrap_or(0) * SECTOR_BYTES;
        // An empty card reader has a disk and nothing in it.
        if bytes == 0 {
            continue;
        }
        let partitions =
            sorted_names(&folder).into_iter().filter(|name| folder.join(name).join(PARTITION_FILE).exists());
        let partition_use = partitions.into_iter().find_map(|name| {
            let reason = uses.own_use(context, &name, &folder.join(&name))?;
            Some(format!("in use, as {name} is {reason}"))
        });
        found.push(HostDrive {
            id: id.clone(),
            kernel_name: disk.clone(),
            bytes,
            model: sysfs_text(&folder.join("device/model")),
            removable: sysfs_text(&folder.join("removable")) == "1",
            in_use: uses.own_use(context, &disk, &folder).or(partition_use),
            serial: udev_value(context, &sysfs_text(&folder.join("dev")), "ID_SERIAL"),
        });
    }
    found
}

fn missing(id: &str) -> String {
    format!("The drive {id} is missing. Plug it in again, or remove it in the settings.")
}

fn cannot_open(id: &str) -> String {
    format!("This user cannot open the drive {id}. Give yourself access to it in the machine's settings.")
}

fn busy(id: &str) -> String {
    format!("The drive {id} is in use on this PC. Unmount it, or stop what else has it, and start again.")
}

fn not_a_drive(id: &str) -> String {
    format!("{id} is not a drive.")
}

fn is_a_partition(id: &str) -> String {
    format!(
        "{id} is a partition. Windows can't use one without its disk, and writes to it anyway, so only \
         whole disks can be given."
    )
}

fn node(context: &Context, drive: &Drive) -> PathBuf {
    context.drive_ids.join(&drive.id)
}

/// QEMU runs as this user. It would only say it failed to open the drive.
pub fn can_open(context: &Context, drive: &Drive) -> bool {
    access(node(context, drive), Access::READ_OK | Access::WRITE_OK).is_ok()
}

/// Whether the name leads to a partition. It's the device that says so, in sysfs, and not how the name
/// is spelled.
fn is_partition(context: &Context, drive: &Drive) -> bool {
    let Ok(real) = fs::canonicalize(node(context, drive)) else { return false };
    let Some(name) = real.file_name() else { return false };
    context.block_devices.join(name).join(PARTITION_FILE).exists()
}

pub fn add_drive(context: &Context, machine: &mut Machine, drive: Drive) -> Result<()> {
    machine.drives.push(drive);
    let saved = machine.save(context);
    if saved.is_err() {
        machine.drives.pop();
    }
    saved
}

pub fn remove_drive(context: &Context, machine: &mut Machine, id: &str) -> Result<()> {
    machine.drives.retain(|drive| drive.id != id);
    machine.save(context)
}

/// Why a drive can't be added to this machine, if it can't: the host has it, or this machine or
/// another that is running has it.
pub fn why_not_offered(drive: &HostDrive, machine: &Machine, others: &[Machine]) -> Option<String> {
    let has_it = |theirs: &Drive| theirs.id == drive.id;
    if machine.drives.iter().any(has_it) {
        return Some("This machine has it already.".to_owned());
    }
    if let Some(other) = others.iter().find(|other| other.drives.iter().any(has_it)) {
        return Some(format!("In use by {}, which is running.", other.name));
    }
    let reason = drive.in_use.as_deref()?;
    let mut letters = reason.chars();
    let first = letters.next()?.to_uppercase();
    Some(format!("{first}{}.", letters.as_str()))
}

/// What to say beside a drive a machine has, if it can't be used now. The start check says the same
/// at greater length.
pub fn drive_note(
    context: &Context,
    drive: &Drive,
    listed: &[HostDrive],
    others: &[Machine],
) -> Option<String> {
    if !node(context, drive).exists() {
        return Some("Not plugged in.".to_owned());
    }
    if is_partition(context, drive) {
        return Some("A partition. Only whole disks can be given.".to_owned());
    }
    let Some(host) = listed.iter().find(|host| host.id == drive.id) else {
        return Some(not_a_drive(&drive.id));
    };
    let nobody = Machine::new("", "", "", "");
    why_not_offered(host, &nobody, others)
}

/// Why this machine's drives can't be given to it now. A machine is only judged by what is on the PC:
/// whether the settings are possible is [`Machine::problems`].
pub fn drive_problems(context: &Context, machine: &Machine, others: &[Machine]) -> Vec<String> {
    if machine.drives.is_empty() {
        return Vec::new();
    }
    let listed = host_drives(context);
    let mut found = Vec::new();
    for drive in &machine.drives {
        let id = &drive.id;
        let host = listed.iter().find(|host| host.id == *id);
        let held_by = others.iter().find(|other| other.drives.iter().any(|theirs| theirs.id == *id));
        if !node(context, drive).exists() {
            found.push(missing(id));
        } else if is_partition(context, drive) {
            found.push(is_a_partition(id));
        } else if let Some(other) = held_by {
            found.push(format!("The drive {id} is in use by {}, which is running.", other.name));
        } else if let Some(host) = host.filter(|host| host.in_use.is_some()) {
            let reason = host.in_use.as_deref().unwrap_or_default();
            found
                .push(format!("The drive {id} is {reason}. Unmount it first, or remove it in the settings."));
        } else if host.is_none() {
            found.push(not_a_drive(id));
        } else if !can_open(context, drive) {
            found.push(cannot_open(id));
        }
    }
    found
}

/// The machine's drives, opened by the app for QEMU. A jailed QEMU can't open one itself, and with the
/// drive open and exclusive the host can't mount it while the machine runs.
#[derive(Debug, Default)]
pub struct OpenDrives(Vec<OwnedFd>);

impl OpenDrives {
    /// Opens each, or says why one can't be. A failure leaves none open.
    pub fn open(context: &Context, drives: &[Drive]) -> Result<Self> {
        let mut opened = Vec::new();
        for drive in drives {
            opened.push(open_drive(context, drive)?);
        }
        Ok(Self(opened))
    }

    /// Leaves each open in what this command starts, at the numbers [`FIRST_DRIVE_FD`] and on. The
    /// app's own copies close on exec as before.
    pub fn hand_to(&self, command: &mut Command) {
        if self.0.is_empty() {
            return;
        }
        let placed: Vec<(&OwnedFd, RawFd)> = self.0.iter().zip(FIRST_DRIVE_FD..).collect();
        leave_open_at(command, &placed);
    }
}

/// Exclusive, so the kernel refuses it while the host has it mounted or another machine has it, and
/// refuses a mount while this has it. Direct, as QEMU only finds the descriptor in its set for a
/// block node that is direct too.
fn open_drive(context: &Context, drive: &Drive) -> Result<OwnedFd> {
    // A name is only a link, and a hand-edited machine file could point it at any file. This looks
    // first, as opening some files (a pipe) would wait, and again on what was opened.
    let is_block = |kind: fs::FileType| kind.is_block_device();
    if is_partition(context, drive) {
        return Err(Failure(is_a_partition(&drive.id)));
    }
    if fs::metadata(node(context, drive)).is_ok_and(|found| !is_block(found.file_type())) {
        return Err(Failure(not_a_drive(&drive.id)));
    }
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags((OFlags::EXCL | OFlags::DIRECT).bits() as i32)
        .open(node(context, drive));
    let file = opened.map_err(|error| match Errno::from_io_error(&error) {
        Some(Errno::BUSY) => Failure(busy(&drive.id)),
        Some(Errno::ACCESS | Errno::PERM) => Failure(cannot_open(&drive.id)),
        Some(Errno::NOENT) => Failure(missing(&drive.id)),
        _ => Failure(format!("Could not open the drive {}: {error}.", drive.id)),
    })?;
    if !file.metadata().is_ok_and(|found| is_block(found.file_type())) {
        return Err(Failure(not_a_drive(&drive.id)));
    }
    // Kept above the numbers QEMU sees them at, so placing one can never land on another.
    Ok(fcntl_dupfd_cloexec(&file, KEPT_ABOVE).map_err(io::Error::from)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Scratch, scratch_context};
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::symlink;
    use std::process::Stdio;

    const SDA_SECTORS: u64 = 2_000_000;

    fn fake_disk(scratch: &Scratch, name: &str, dev: &str, sectors: u64, model: &str) {
        scratch.write(&format!("block/{name}/dev"), format!("{dev}\n"));
        scratch.write(&format!("block/{name}/size"), format!("{sectors}\n"));
        scratch.write(&format!("block/{name}/removable"), "0\n");
        scratch.write(&format!("block/{name}/device/model"), format!("{model}  \n"));
    }

    fn fake_partition(scratch: &Scratch, disk: &str, name: &str, dev: &str, sectors: u64) {
        scratch.write(&format!("block/{disk}/{name}/partition"), "1\n");
        scratch.write(&format!("block/{name}/partition"), "1\n");
        scratch.write(&format!("block/{disk}/{name}/dev"), format!("{dev}\n"));
        scratch.write(&format!("block/{disk}/{name}/size"), format!("{sectors}\n"));
    }

    /// A name for a drive, as a link to a file that stands in for its node in `/dev`.
    fn name_it(scratch: &Scratch, id: &str, kernel_name: &str) {
        let node = scratch.path(&format!("dev/{kernel_name}"));
        if !node.exists() {
            scratch.write(&format!("dev/{kernel_name}"), "");
        }
        fs::create_dir_all(scratch.path("drive-ids")).unwrap();
        symlink(node, scratch.path(&format!("drive-ids/{id}"))).unwrap();
    }

    /// A PC with one disk and two partitions, each with a name under by-id.
    fn pc_with_a_disk(scratch: &Scratch) -> Context {
        fake_disk(scratch, "sda", "8:0", SDA_SECTORS, "Samsung SSD 860");
        fake_partition(scratch, "sda", "sda1", "8:1", 1_000);
        fake_partition(scratch, "sda", "sda2", "8:2", 1_999_000);
        name_it(scratch, "ata-Samsung_SSD_860_S3Z", "sda");
        name_it(scratch, "ata-Samsung_SSD_860_S3Z-part1", "sda1");
        name_it(scratch, "ata-Samsung_SSD_860_S3Z-part2", "sda2");
        scratch_context(scratch)
    }

    fn drive_named<'a>(found: &'a [HostDrive], id: &str) -> &'a HostDrive {
        found.iter().find(|drive| drive.id == id).unwrap()
    }

    #[test]
    fn a_disk_is_listed_with_its_size_and_model_and_its_partitions_are_not() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let found = host_drives(&context);
        assert_eq!(found.len(), 1, "{found:?}");
        let disk = &found[0];
        assert_eq!(disk.kernel_name, "sda");
        assert_eq!(
            (disk.bytes, disk.model.as_str(), disk.removable),
            (SDA_SECTORS * 512, "Samsung SSD 860", false)
        );
    }
    #[test]
    fn a_name_with_the_model_in_it_wins_over_the_hardware_number() {
        let scratch = Scratch::new();
        fake_disk(&scratch, "nvme0n1", "259:0", 100, "Samsung SSD 970");
        name_it(&scratch, "nvme-eui.0025385b21404f1a", "nvme0n1");
        name_it(&scratch, "nvme-Samsung_SSD_970_S4EW", "nvme0n1");
        fake_disk(&scratch, "sdb", "8:16", 100, "Disk");
        name_it(&scratch, "wwn-0x5002538e", "sdb");
        let found = host_drives(&scratch_context(&scratch));
        assert_eq!(found[0].id, "nvme-Samsung_SSD_970_S4EW");
        assert_eq!(found[1].id, "wwn-0x5002538e", "the number is better than no name");
    }

    #[test]
    fn a_device_with_no_name_to_keep_it_by_is_not_offered() {
        let scratch = Scratch::new();
        fake_disk(&scratch, "sda", "8:0", 100, "Disk");
        assert!(host_drives(&scratch_context(&scratch)).is_empty());
    }

    #[test]
    fn loop_devices_volumes_and_optical_drives_are_not_offered() {
        for name in ["loop0", "ram0", "zram0", "dm-0", "md127", "sr0"] {
            assert!(!is_offered(name), "{name}");
        }
        for name in ["sda", "sda1", "nvme0n1", "nvme0n1p2", "vdb", "mmcblk0", "sdz"] {
            assert!(is_offered(name), "{name}");
        }
        let scratch = Scratch::new();
        fake_disk(&scratch, "loop3", "7:3", 100, "");
        name_it(&scratch, "loop-name", "loop3");
        assert!(host_drives(&scratch_context(&scratch)).is_empty());
    }

    #[test]
    fn a_removable_disk_says_so() {
        let scratch = Scratch::new();
        fake_disk(&scratch, "sdc", "8:32", 100, "Stick");
        scratch.write("block/sdc/removable", "1\n");
        name_it(&scratch, "usb-Stick_123-0:0", "sdc");
        assert!(host_drives(&scratch_context(&scratch))[0].removable);
    }

    #[test]
    fn a_disk_with_a_mounted_partition_says_which_and_where() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write(
            "mountinfo",
            "22 1 8:1 / /boot/efi rw,relatime shared:5 - vfat /dev/sda1 rw\n\
             23 1 0:30 / /run/user/1000 rw - tmpfs tmpfs rw\n",
        );
        let found = host_drives(&context);
        assert_eq!(found[0].in_use.as_deref(), Some("in use, as sda1 is mounted at /boot/efi"));
    }
    #[test]
    fn a_mount_is_found_by_the_source_when_the_filesystem_has_a_number_of_its_own() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("mountinfo", "40 1 0:41 /@home /home rw - btrfs /dev/sda2 rw,subvol=/@home\n");
        assert_eq!(host_drives(&context)[0].in_use.as_deref(), Some("in use, as sda2 is mounted at /home"));
    }
    #[test]
    fn a_mount_point_with_a_space_in_it_is_read_back_as_it_was() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("mountinfo", "41 1 8:2 / /media/my\\040disk\\134 rw - ext4 /dev/sda2 rw\n");
        let reason = host_drives(&context)[0].in_use.clone();
        assert_eq!(reason.as_deref(), Some("in use, as sda2 is mounted at /media/my disk\\"));
    }
    #[test]
    fn swap_is_in_use() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write(
            "swaps",
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/sda2\t\t\t\tpartition\t8388604\t\t0\t\t-2\n",
        );
        assert_eq!(host_drives(&context)[0].in_use.as_deref(), Some("in use, as sda2 is used for swap"));
    }
    #[test]
    fn a_partition_under_a_volume_is_held_by_it_under_its_name() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("block/sda/sda2/holders/dm-0/x", "");
        scratch.write("block/dm-0/dm/name", "vg-root\n");
        let reason = host_drives(&context)[0].in_use.clone();
        assert_eq!(reason.as_deref(), Some("in use, as sda2 is held by vg-root"));
    }
    #[test]
    fn a_volume_with_no_name_of_its_own_is_shown_by_the_kernels() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("block/sda/holders/md0/x", "");
        let found = host_drives(&context);
        assert_eq!(drive_named(&found, "ata-Samsung_SSD_860_S3Z").in_use.as_deref(), Some("held by md0"));
    }

    #[test]
    fn a_drive_nothing_uses_has_no_reason() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("mountinfo", "23 1 0:30 / /run rw - tmpfs tmpfs rw\n");
        assert!(host_drives(&context).iter().all(|drive| drive.in_use.is_none()));
    }

    #[test]
    fn the_rule_for_a_disk_is_matched_by_its_serial() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("udev/b8:0", "S:disk/by-id/x\nE:ID_SERIAL=Samsung_SSD_860_S3Z\nE:ID_BUS=ata\n");
        scratch.write("udev/b8:1", "E:ID_SERIAL=Samsung_SSD_860_S3Z\nE:ID_PART_ENTRY_UUID=0f1e-22\n");
        let rule = host_drives(&context)[0].access_command().unwrap();
        assert!(
            rule.contains(
                "ENV{DEVTYPE}==\"disk\", ENV{ID_SERIAL}==\"Samsung_SSD_860_S3Z\", TAG+=\"uaccess\""
            )
        );
        assert!(rule.contains("--property-match=ID_SERIAL=Samsung_SSD_860_S3Z"), "{rule}");
        assert!(rule.contains("70-guesthouse-drive-Samsung_SSD_860_S3Z.rules"), "{rule}");
    }
    #[test]
    fn no_rule_is_written_for_a_drive_with_no_number_or_one_that_needs_quoting() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let found = host_drives(&context);
        let error = found[0].access_command().unwrap_err();
        assert!(error.0.contains("no number that tells it apart"), "{error}");
        scratch.write("udev/b8:0", "E:ID_SERIAL=a'; rm -rf ~; echo '\n");
        assert!(host_drives(&context)[0].access_command().is_err());
    }

    fn machine_with(drives: &[&str]) -> Machine {
        let drives = drives.iter().map(|id| Drive { id: (*id).to_owned() }).collect();
        Machine { drives, ..Machine::new("vm", "VM", "/d", "/v") }
    }

    fn says(found: &[String], text: &str) -> bool {
        found.iter().any(|problem| problem.contains(text))
    }

    #[test]
    fn a_drive_that_is_plugged_in_and_free_has_nothing_against_it() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let machine = machine_with(&["ata-Samsung_SSD_860_S3Z"]);
        assert_eq!(drive_problems(&context, &machine, &[]), Vec::<String>::new());
    }

    #[test]
    fn a_drive_that_is_gone_is_said_to_be_missing() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let found = drive_problems(&context, &machine_with(&["ata-Gone_1"]), &[]);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("The drive ata-Gone_1 is missing."), "{found:?}");
    }

    #[test]
    fn a_drive_the_host_has_mounted_is_refused_with_the_place() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("mountinfo", "22 1 8:1 / /boot rw - ext4 /dev/sda1 rw\n");
        let found = drive_problems(&context, &machine_with(&["ata-Samsung_SSD_860_S3Z"]), &[]);
        assert!(says(&found, "is in use, as sda1 is mounted at /boot. Unmount it first"), "{found:?}");
    }
    #[test]
    fn a_drive_another_running_machine_has_is_refused_with_its_name() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let other = Machine { name: "Games".into(), ..machine_with(&["ata-Samsung_SSD_860_S3Z"]) };
        let found = drive_problems(&context, &machine_with(&["ata-Samsung_SSD_860_S3Z"]), &[other]);
        assert!(says(&found, "in use by Games, which is running"), "{found:?}");
    }

    #[test]
    fn a_name_that_is_not_a_drive_the_app_offers_is_refused() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        scratch.write("drive-ids/dm-name-vg-root", "");
        let found = drive_problems(&context, &machine_with(&["dm-name-vg-root"]), &[]);
        assert!(says(&found, "dm-name-vg-root is not a drive"), "{found:?}");
    }

    #[test]
    fn a_drive_this_user_cannot_open_says_how_to_get_access() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        fs::set_permissions(scratch.path("dev/sda"), fs::Permissions::from_mode(0o000)).unwrap();
        let found = drive_problems(&context, &machine_with(&["ata-Samsung_SSD_860_S3Z"]), &[]);
        // Root opens anything, so only a user that isn't root can see this.
        let is_root = rustix::process::getuid().is_root();
        assert_eq!(
            says(&found, "Give yourself access to it in the machine's settings"),
            !is_root,
            "{found:?}"
        );
    }

    /// Keeps a file open as the app keeps a drive.
    fn kept(scratch: &Scratch, name: &str, text: &str) -> OwnedFd {
        let file = fs::File::open(scratch.write(name, text)).unwrap();
        fcntl_dupfd_cloexec(&file, KEPT_ABOVE).unwrap()
    }

    #[test]
    fn a_child_finds_each_drive_at_its_number_and_none_of_the_apps_copies() {
        let scratch = Scratch::new();
        let open = OpenDrives(vec![kept(&scratch, "one", "first"), kept(&scratch, "two", "second")]);
        let mut command = Command::new("sh");
        command.args(["-c", "cat /proc/self/fd/40; echo; cat /proc/self/fd/41; echo; ls /proc/self/fd"]);
        open.hand_to(command.stdout(Stdio::piped()));
        let mut said = String::new();
        command.spawn().unwrap().stdout.take().unwrap().read_to_string(&mut said).unwrap();
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines[..2], ["first", "second"]);
        let own: Vec<String> = open.0.iter().map(|fd| fd.as_raw_fd().to_string()).collect();
        assert!(
            !lines[2..].iter().any(|fd| own.contains(&fd.to_string())),
            "the app's own copies close on exec"
        );
    }

    #[test]
    fn a_drive_that_is_not_there_says_so() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let error = OpenDrives::open(&context, &[Drive { id: "gone".into() }]).unwrap_err();
        assert!(error.0.starts_with("The drive gone is missing."), "{error}");
    }

    #[test]
    fn nothing_is_handed_when_there_are_no_drives() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let open = OpenDrives::open(&context, &[]).unwrap();
        let mut command = Command::new("true");
        open.hand_to(&mut command);
        assert!(command.status().unwrap().success());
    }

    #[test]
    fn a_partition_is_refused_to_start_and_to_open_whatever_its_name_says() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        // The name says nothing: it's the device behind it that is a partition.
        name_it(&scratch, "ata-Samsung_SSD_860_S3Z-looks-like-a-disk", "sda1");
        for id in ["ata-Samsung_SSD_860_S3Z-part1", "ata-Samsung_SSD_860_S3Z-looks-like-a-disk"] {
            let found = drive_problems(&context, &machine_with(&[id]), &[]);
            assert_eq!(found.len(), 1, "{found:?}");
            assert!(
                found[0].starts_with(&format!("{id} is a partition. Windows can't use one")),
                "{found:?}"
            );
            let error = OpenDrives::open(&context, &[Drive { id: id.into() }]).unwrap_err();
            assert_eq!(error.0, found[0]);
        }
        let disk = drive_problems(&context, &machine_with(&["ata-Samsung_SSD_860_S3Z"]), &[]);
        assert!(!says(&disk, "partition"), "{disk:?}");
    }

    #[test]
    fn a_disk_another_running_machine_has_is_refused_with_its_name_too() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let games = Machine { name: "Games".into(), ..machine_with(&["ata-Samsung_SSD_860_S3Z"]) };
        let found = drive_problems(&context, &machine_with(&["ata-Samsung_SSD_860_S3Z"]), &[games]);
        assert!(says(&found, "in use by Games, which is running"), "{found:?}");
    }
    #[test]
    fn a_drive_with_nothing_in_it_is_not_offered() {
        let scratch = Scratch::new();
        fake_disk(&scratch, "sdc", "8:32", 0, "Card reader");
        name_it(&scratch, "usb-Card_reader-0:0", "sdc");
        assert!(host_drives(&scratch_context(&scratch)).is_empty());
    }

    #[test]
    fn a_name_that_leads_to_an_ordinary_file_is_not_a_drive_and_nothing_is_opened() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        let drives = [Drive { id: "ata-Samsung_SSD_860_S3Z".into() }];
        let error = OpenDrives::open(&context, &drives).unwrap_err();
        assert_eq!(error.0, "ata-Samsung_SSD_860_S3Z is not a drive.");
    }

    #[test]
    fn a_size_is_said_as_the_maker_says_it() {
        let drive = |bytes| HostDrive { bytes, ..pc_drive() };
        assert_eq!(drive(1_000_204_886_016).size_text(), "1.0 TB");
        assert_eq!(drive(500_107_862_016).size_text(), "500 GB");
        assert_eq!(drive(DRIVE_MIB_BYTES).size_text(), "67 MB");
    }

    const DRIVE_MIB_BYTES: u64 = 64 << 20;

    fn pc_drive() -> HostDrive {
        let scratch = Scratch::new();
        host_drives(&pc_with_a_disk(&scratch)).remove(0)
    }

    #[test]
    fn a_disk_is_called_by_its_make_and_size() {
        let scratch = Scratch::new();
        let found = host_drives(&pc_with_a_disk(&scratch));
        assert_eq!(found[0].title(), "Samsung SSD 860, 1 GB");
    }
    #[test]
    fn the_list_says_why_a_drive_cannot_be_added() {
        let scratch = Scratch::new();
        let context = pc_with_a_disk(&scratch);
        scratch.write("mountinfo", "22 1 8:1 / /boot rw - ext4 /dev/sda1 rw\n");
        let disk = host_drives(&context).remove(0);
        let nobody = machine_with(&[]);
        assert_eq!(
            why_not_offered(&disk, &nobody, &[]).as_deref(),
            Some("In use, as sda1 is mounted at /boot.")
        );
        let free = HostDrive { in_use: None, ..disk.clone() };
        assert_eq!(why_not_offered(&free, &nobody, &[]), None);
        let games = Machine { name: "Games".into(), ..machine_with(&[&disk.id]) };
        let reason = why_not_offered(&free, &nobody, std::slice::from_ref(&games));
        assert_eq!(reason.as_deref(), Some("In use by Games, which is running."));
        assert_eq!(why_not_offered(&free, &games, &[]).as_deref(), Some("This machine has it already."));
    }
    #[test]
    fn drives_are_added_and_removed_in_the_saved_file() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let mut machine = Machine { kind: crate::machine::Kind::Plain, ..machine_with(&[]) };
        add_drive(&context, &mut machine, Drive { id: "a".into() }).unwrap();
        add_drive(&context, &mut machine, Drive { id: "b".into() }).unwrap();
        assert_eq!(crate::machine::load(&context, "vm").unwrap().drives, machine.drives);
        remove_drive(&context, &mut machine, "a").unwrap();
        let saved = crate::machine::load(&context, "vm").unwrap();
        assert_eq!(saved.drives, [Drive { id: "b".into() }]);
    }

    #[test]
    fn a_drive_that_would_not_save_is_not_kept() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let mut machine = machine_with(&["a", "b", "c"]);
        let error = add_drive(&context, &mut machine, Drive { id: "d".into() }).unwrap_err();
        assert!(error.0.contains("3 drives at most"), "{error}");
        assert_eq!(machine.drives.len(), 3);
    }
}
