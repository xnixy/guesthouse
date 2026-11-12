//! What the code needs from the PC, in one value that a test can fill with stand-ins.

use crate::agent::{GuestAgent, TcpAgent};
use crate::bridge::BridgeHost;
use crate::clock::{Clock, RealClock};
use crate::guest_memory::GuestMemory;
use crate::jailing::Offer;
use crate::monitor::{Monitor, QmpMonitor};
use crate::paths::Paths;
use crate::vm_args::VmPlan;
use guesthouse_daemon::slot::Slot;
use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PRIVATE_FOLDER: u32 = 0o700;

/// A folder that already exists is made private too, as the first run could have made it otherwise.
fn make_private_folder(folder: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(PRIVATE_FOLDER).create(folder)?;
    fs::set_permissions(folder, Permissions::from_mode(PRIVATE_FOLDER))
}

/// The variable that names the slot of a task.
pub const SLOT_VARIABLE: &str = guesthouse_daemon::slot::VARIABLE;

#[derive(Clone)]
pub struct Context {
    pub paths: Paths,
    /// This process's slot. A task has its VM's. The window has the first, and names others.
    pub slot: Slot,
    pub processes: PathBuf,
    pub cpus: PathBuf,
    pub usb_devices: PathBuf,
    pub usb_nodes: PathBuf,
    /// The kernel's block devices, one folder for each disk with its partitions inside.
    pub block_devices: PathBuf,
    /// The names that stay with a drive, as links to wherever the kernel put it this time.
    pub drive_ids: PathBuf,
    pub mount_table: PathBuf,
    pub swaps: PathBuf,
    /// What udev knows of each block device, a file for each by its major and minor numbers.
    pub udev_data: PathBuf,
    pub pci_devices: PathBuf,
    pub pci_names: PathBuf,
    /// The NVIDIA driver's own list of its cards, a folder for each by PCI address.
    pub nvidia_cards: PathBuf,
    /// The DRM class in sysfs, where each render node links to its PCI device.
    pub drm_class: PathBuf,
    pub kvm: PathBuf,
    pub driver_version_file: PathBuf,
    pub meminfo: PathBuf,
    /// Makes a GPU guest's memory and claims its RAM. A test has no gigabytes to spare.
    pub reserve_memory: fn(&Context, Slot, u64) -> crate::Result<GuestMemory>,
    /// What this PC offers a GPU machine's jail. A test's pretend programs can't be jailed.
    pub jail_offer: fn(&VmPlan) -> Offer,
    /// Stands in for what this PC can give a GPU machine, which depends on everything else running.
    pub ram_limit: Option<u64>,
    pub bridge: BridgeHost,
    pub monitor: Arc<dyn Monitor>,
    pub agent: Arc<dyn GuestAgent>,
    pub clock: Arc<dyn Clock>,
    /// Replaces the room an install needs, for a test on a small disk.
    pub install_room: Option<u64>,
}

impl Context {
    pub fn new(paths: Paths, slot: Slot) -> Self {
        Self {
            paths,
            slot,
            processes: "/proc".into(),
            cpus: "/sys/devices/system/cpu".into(),
            usb_devices: "/sys/bus/usb/devices".into(),
            usb_nodes: "/dev/bus/usb".into(),
            block_devices: "/sys/class/block".into(),
            drive_ids: "/dev/disk/by-id".into(),
            mount_table: "/proc/self/mountinfo".into(),
            swaps: "/proc/swaps".into(),
            udev_data: "/run/udev/data".into(),
            pci_devices: "/sys/bus/pci/devices".into(),
            pci_names: "/usr/share/hwdata/pci.ids".into(),
            nvidia_cards: "/proc/driver/nvidia/gpus".into(),
            drm_class: "/sys/class/drm".into(),
            kvm: "/dev/kvm".into(),
            driver_version_file: "/sys/module/nvidia/version".into(),
            meminfo: "/proc/meminfo".into(),
            reserve_memory: GuestMemory::claim,
            jail_offer: Offer::here,
            ram_limit: None,
            bridge: BridgeHost::default(),
            monitor: Arc::new(QmpMonitor),
            agent: Arc::new(TcpAgent),
            clock: Arc::new(RealClock::default()),
            install_room: None,
        }
    }

    /// For this process. A slot that can't be read stops it, as a guess could reach another VM's files.
    pub fn from_process() -> crate::Result<Self> {
        let slot = Slot::from_environment().map_err(|error| crate::Failure::new(error.to_string()))?;
        Ok(Self::new(Paths::from_process(), slot))
    }

    pub fn second_instance(&self) -> bool {
        self.paths.second_instance()
    }

    pub fn run_folder(&self, slot: Option<Slot>) -> PathBuf {
        self.paths.run_folder(slot.unwrap_or(self.slot))
    }

    /// Makes a run folder that only this user can open, as its sockets give the run of a VM.
    pub fn make_run_folder(&self, slot: Option<Slot>) -> std::io::Result<()> {
        make_private_folder(&self.paths.runtime)?;
        make_private_folder(&self.run_folder(slot))
    }

    pub fn agent_port(&self, slot: Option<Slot>) -> u16 {
        self.paths.agent_port(slot.unwrap_or(self.slot))
    }

    pub fn install_room_gib(&self) -> u64 {
        self.install_room.unwrap_or(crate::library::INSTALL_ROOM_GIB)
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{Scratch, scratch_context};
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;

    const OTHERS: u32 = 0o077;

    #[test]
    fn a_run_folder_is_private_even_if_it_was_there_already() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        fs::create_dir_all(context.run_folder(None)).unwrap();
        fs::set_permissions(&context.paths.runtime, Permissions::from_mode(0o755)).unwrap();
        context.make_run_folder(None).unwrap();
        for folder in [context.paths.runtime.clone(), context.run_folder(None)] {
            assert_eq!(fs::metadata(folder).unwrap().permissions().mode() & OTHERS, 0);
        }
    }
}
