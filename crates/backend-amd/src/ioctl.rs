//! The amdgpu render node and the DRM ioctls it answers.
//!
//! Each request is a plain structure with a fixed layout, written here by offset. The numbers and
//! layouts come from the kernel's public uAPI headers, `include/uapi/drm/amdgpu_drm.h` and `drm.h`,
//! which are GPL-2.0 and the only description the open driver has. An ioctl's number packs its
//! direction, the request's size and a per-driver command number, the same encoding the NVIDIA
//! backend uses for its own DRM calls.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const DRM_CLASS_FOLDER: &str = "/sys/class/drm";
const DRM_DEVICE_FOLDER: &str = "/dev/dri";
const RENDER_NODE_PREFIX: &str = "renderD";
const VENDOR_FILE: &str = "device/vendor";
/// The PCI vendor id AMD's cards carry. The node number is never hardcoded: it moves with whatever
/// else is in the machine, so the card is found by this id.
pub const AMD_VENDOR: u16 = 0x1002;

const IOCTL_WRITE: u64 = 1 << 30;
const IOCTL_READ_WRITE: u64 = 3 << 30;
const IOCTL_SIZE_SHIFT: u32 = 16;
const DRM_IOCTL_MAGIC: u64 = b'd' as u64;
/// Where a DRM driver's own ioctls begin, after the ones every driver shares.
const DRM_COMMAND_BASE: u64 = 0x40;

// Core DRM ioctls, shared by every driver.
const DRM_GEM_CLOSE: u64 = 0x09;
const DRM_PRIME_HANDLE_TO_FD: u64 = 0x2D;
const DRM_SYNCOBJ_CREATE: u64 = 0xBF;
const DRM_SYNCOBJ_DESTROY: u64 = 0xC0;
const DRM_SYNCOBJ_EVENTFD: u64 = 0xCF;

// amdgpu's own ioctls, at DRM_COMMAND_BASE plus the driver's command number.
const DRM_AMDGPU_GEM_CREATE: u64 = 0x00;
const DRM_AMDGPU_GEM_MMAP: u64 = 0x01;
const DRM_AMDGPU_CTX: u64 = 0x02;
const DRM_AMDGPU_CS: u64 = 0x04;
const DRM_AMDGPU_INFO: u64 = 0x05;
const DRM_AMDGPU_GEM_METADATA: u64 = 0x06;
const DRM_AMDGPU_GEM_VA: u64 = 0x08;
const DRM_AMDGPU_GEM_USERPTR: u64 = 0x11;

fn number(direction: u64, command: u64, size: usize) -> u64 {
    direction | (size as u64) << IOCTL_SIZE_SHIFT | DRM_IOCTL_MAGIC << 8 | command
}

/// The ioctl numbers this backend calls, each for the fixed size of its request.
pub mod call {
    use super::*;

    pub fn gem_create() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_CREATE, 32)
    }
    pub fn gem_mmap() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_MMAP, 8)
    }
    pub fn gem_userptr() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_USERPTR, 24)
    }
    pub fn gem_metadata() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_METADATA, 288)
    }
    pub fn gem_va() -> u64 {
        number(IOCTL_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_VA, 64)
    }
    pub fn ctx() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_CTX, 16)
    }
    pub fn cs() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_CS, 24)
    }
    pub fn info() -> u64 {
        number(IOCTL_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_INFO, 32)
    }
    pub fn gem_close() -> u64 {
        number(IOCTL_WRITE, DRM_GEM_CLOSE, 8)
    }
    pub fn prime_handle_to_fd() -> u64 {
        number(IOCTL_READ_WRITE, DRM_PRIME_HANDLE_TO_FD, 12)
    }
    pub fn syncobj_create() -> u64 {
        number(IOCTL_READ_WRITE, DRM_SYNCOBJ_CREATE, 8)
    }
    pub fn syncobj_destroy() -> u64 {
        number(IOCTL_READ_WRITE, DRM_SYNCOBJ_DESTROY, 8)
    }
    pub fn syncobj_eventfd() -> u64 {
        number(IOCTL_READ_WRITE, DRM_SYNCOBJ_EVENTFD, 24)
    }
}

pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

pub fn get64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// A DRM ioctl against an open file, tried again if a signal cuts it short, as DRM's own library
/// does. The request buffer is the size the number says, and the kernel writes only inside it.
pub fn drm_ioctl(file: &impl AsRawFd, ioctl: u64, request: &mut [u8]) -> io::Result<()> {
    loop {
        // SAFETY: the request is a live buffer of the size the ioctl number carries, and the kernel
        // reads and writes only inside it.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), ioctl as _, request.as_mut_ptr()) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The render node of an AMD card. It is found by the vendor id on its PCI device, as the node
/// numbers change with what else is in the machine. With an address given, only the node of the card
/// at that address is accepted.
pub fn amd_render_node(address: Option<&str>) -> Option<PathBuf> {
    amd_render_node_in(Path::new(DRM_CLASS_FOLDER), Path::new(DRM_DEVICE_FOLDER), address)
}

/// The sysfs folder of a render node's PCI device, where what INFO leaves out can be read.
pub fn device_folder(node: &Path) -> Option<PathBuf> {
    Some(Path::new(DRM_CLASS_FOLDER).join(node.file_name()?).join("device"))
}

fn amd_render_node_in(class_folder: &Path, device_folder: &Path, address: Option<&str>) -> Option<PathBuf> {
    let mut names: Vec<_> = fs::read_dir(class_folder)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with(RENDER_NODE_PREFIX))
        .collect();
    names.sort();
    let is_amd = |name: &&std::ffi::OsString| {
        let vendor = fs::read_to_string(class_folder.join(name).join(VENDOR_FILE));
        vendor
            .is_ok_and(|text| u16::from_str_radix(text.trim().trim_start_matches("0x"), 16) == Ok(AMD_VENDOR))
    };
    let at_address = |name: &&std::ffi::OsString| {
        let Some(wanted) = address else { return true };
        let device = fs::canonicalize(class_folder.join(name).join("device"));
        device.ok().and_then(|path| Some(path.file_name()?.to_string_lossy() == *wanted)).unwrap_or(false)
    };
    names.iter().filter(at_address).find(is_amd).map(|name| device_folder.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::{env, process};

    #[test]
    fn ioctl_numbers_follow_the_kernels_encoding() {
        // Checked against the header's own DRM_IOCTL_AMDGPU_* and DRM_IOCTL_SYNCOBJ_* macros.
        assert_eq!(call::gem_create(), 0xC020_6440);
        assert_eq!(call::gem_mmap(), 0xC008_6441);
        assert_eq!(call::ctx(), 0xC010_6442);
        assert_eq!(call::cs(), 0xC018_6444);
        assert_eq!(call::info(), 0x4020_6445);
        assert_eq!(call::gem_va(), 0x4040_6448);
        assert_eq!(call::gem_metadata(), 0xC120_6446);
        assert_eq!(call::gem_userptr(), 0xC018_6451);
        assert_eq!(call::gem_close(), 0x4008_6409);
        assert_eq!(call::prime_handle_to_fd(), 0xC00C_642D);
        assert_eq!(call::syncobj_create(), 0xC008_64BF);
        assert_eq!(call::syncobj_eventfd(), 0xC018_64CF);
    }

    fn class_folder(name: &str, nodes: &[(&str, &str, u16)]) -> (PathBuf, PathBuf) {
        let base = env::temp_dir().join(format!("guesthouse-amd-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let class = base.join("class");
        let dev = base.join("dev");
        fs::create_dir_all(&dev).unwrap();
        for (node, pci, vendor) in nodes {
            let device = base.join("devices").join(pci);
            fs::create_dir_all(&device).unwrap();
            fs::write(device.join("vendor"), format!("0x{vendor:04x}\n")).unwrap();
            fs::create_dir_all(class.join(node)).unwrap();
            symlink(&device, class.join(node).join("device")).unwrap();
        }
        (class, dev)
    }

    #[test]
    fn the_render_node_is_the_amd_one() {
        let (class, dev) = class_folder(
            "pick",
            &[("renderD128", "0000:7c:00.0", 0x1002), ("renderD129", "0000:02:00.0", 0x10DE)],
        );
        assert_eq!(amd_render_node_in(&class, &dev, None), Some(dev.join("renderD128")));
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }

    #[test]
    fn with_no_amd_card_there_is_no_node() {
        let (class, dev) = class_folder("none", &[("renderD128", "0000:02:00.0", 0x10DE)]);
        assert_eq!(amd_render_node_in(&class, &dev, None), None);
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_address_narrows_it_to_one_card() {
        let (class, dev) = class_folder(
            "addr",
            &[("renderD128", "0000:03:00.0", 0x1002), ("renderD129", "0000:04:00.0", 0x1002)],
        );
        assert_eq!(amd_render_node_in(&class, &dev, Some("0000:04:00.0")), Some(dev.join("renderD129")));
        assert_eq!(amd_render_node_in(&class, &dev, Some("0000:09:00.0")), None);
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }
}
