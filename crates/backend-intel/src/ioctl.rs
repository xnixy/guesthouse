//! The xe render node and the one DRM ioctl this stage calls on it.
//!
//! The number and the request's layout come from the kernel's public uAPI header,
//! `include/uapi/drm/xe_drm.h`, which is MIT, and from `drm.h`. An ioctl's number packs its
//! direction, the request's size and a per-driver command number, the same encoding the other
//! backends use. This stage only asks the driver questions: nothing here makes an address space, a
//! buffer or a queue, so nothing can run on the card.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const DRM_CLASS_FOLDER: &str = "/sys/class/drm";
const DRM_DEVICE_FOLDER: &str = "/dev/dri";
const RENDER_NODE_PREFIX: &str = "renderD";
const VENDOR_FILE: &str = "device/vendor";
const DRIVER_LINK: &str = "device/driver";
/// The PCI vendor id Intel's cards carry. The card is found by it, as node numbers move with
/// whatever else is in the machine.
pub const INTEL_VENDOR: u16 = 0x8086;
/// The kernel driver this backend speaks to. An Intel card on i915 has a different uAPI.
pub const XE_DRIVER: &str = "xe";

const IOCTL_READ_WRITE: u64 = 3 << 30;
const IOCTL_SIZE_SHIFT: u32 = 16;
const DRM_IOCTL_MAGIC: u64 = b'd' as u64;
/// Where a DRM driver's own ioctls begin, after the ones every driver shares.
const DRM_COMMAND_BASE: u64 = 0x40;
/// xe's command number for a device query, DRM_XE_DEVICE_QUERY.
const DRM_XE_DEVICE_QUERY: u64 = 0x00;
/// The size of `struct drm_xe_device_query`.
pub const DEVICE_QUERY_SIZE: usize = 40;

/// The ioctl number of a device query.
pub fn device_query() -> u64 {
    IOCTL_READ_WRITE
        | (DEVICE_QUERY_SIZE as u64) << IOCTL_SIZE_SHIFT
        | DRM_IOCTL_MAGIC << 8
        | (DRM_COMMAND_BASE + DRM_XE_DEVICE_QUERY)
}

/// A DRM ioctl against an open file, tried again if a signal cuts it short. The request buffer is
/// the size the number says, and any pointer inside it points at a live buffer of the size it gives.
pub fn drm_ioctl(file: &impl AsRawFd, ioctl: u64, request: &mut [u8]) -> io::Result<()> {
    loop {
        // SAFETY: the request is a live buffer of the size the ioctl number carries, and the caller
        // keeps any buffer it points at alive and as long as it says. The kernel writes only there.
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

/// The render node of an Intel card on xe, and its sysfs device folder. With an address given,
/// only the card at that address is taken.
pub fn intel_render_node(address: Option<&str>) -> Option<RenderNode> {
    intel_render_node_in(Path::new(DRM_CLASS_FOLDER), Path::new(DRM_DEVICE_FOLDER), address)
}

/// Where a card's device file and its sysfs folder are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderNode {
    pub device_file: PathBuf,
    pub sysfs: PathBuf,
    /// The kernel driver bound to it, which [`intel_render_node`] only gives as `xe`.
    pub driver: String,
}

/// The name of the driver bound to a node's device, from the link sysfs keeps.
fn driver_of(class_folder: &Path, node: &std::ffi::OsStr) -> Option<String> {
    let link = fs::read_link(class_folder.join(node).join(DRIVER_LINK)).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// The Intel nodes under `class_folder`, each with the driver bound to it, for the probe to say why
/// a card was passed over.
pub fn intel_nodes_in(class_folder: &Path) -> Vec<(std::ffi::OsString, Option<String>)> {
    let mut names: Vec<_> = fs::read_dir(class_folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with(RENDER_NODE_PREFIX))
        .collect();
    names.sort();
    let is_intel = |name: &std::ffi::OsString| {
        let vendor = fs::read_to_string(class_folder.join(name).join(VENDOR_FILE));
        vendor.is_ok_and(|text| {
            u16::from_str_radix(text.trim().trim_start_matches("0x"), 16) == Ok(INTEL_VENDOR)
        })
    };
    names.into_iter().filter(is_intel).map(|name| (name.clone(), driver_of(class_folder, &name))).collect()
}

fn intel_render_node_in(
    class_folder: &Path,
    device_folder: &Path,
    address: Option<&str>,
) -> Option<RenderNode> {
    let at_address = |name: &std::ffi::OsString| {
        let Some(wanted) = address else { return true };
        let device = fs::canonicalize(class_folder.join(name).join("device"));
        device.ok().and_then(|path| Some(path.file_name()?.to_string_lossy() == *wanted)).unwrap_or(false)
    };
    intel_nodes_in(class_folder)
        .into_iter()
        .filter(|(name, driver)| at_address(name) && driver.as_deref() == Some(XE_DRIVER))
        .map(|(name, driver)| RenderNode {
            device_file: device_folder.join(&name),
            sysfs: class_folder.join(&name).join("device"),
            driver: driver.unwrap_or_default(),
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::{env, process};

    #[test]
    fn the_query_number_follows_the_kernels_encoding() {
        // Checked against the header's own DRM_IOCTL_XE_DEVICE_QUERY.
        assert_eq!(device_query(), 0xC028_6440);
    }

    /// Nodes as `(node, pci address, vendor, driver)`.
    fn class_folder(name: &str, nodes: &[(&str, &str, u16, &str)]) -> (PathBuf, PathBuf) {
        let base = env::temp_dir().join(format!("guesthouse-intel-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let class = base.join("class");
        let dev = base.join("dev");
        fs::create_dir_all(&dev).unwrap();
        for (node, pci, vendor, driver) in nodes {
            let device = base.join("devices").join(pci);
            fs::create_dir_all(&device).unwrap();
            fs::write(device.join("vendor"), format!("0x{vendor:04x}\n")).unwrap();
            let driver_folder = base.join("drivers").join(driver);
            fs::create_dir_all(&driver_folder).unwrap();
            symlink(&driver_folder, device.join("driver")).unwrap();
            fs::create_dir_all(class.join(node)).unwrap();
            symlink(&device, class.join(node).join("device")).unwrap();
        }
        (class, dev)
    }

    #[test]
    fn the_render_node_is_the_intel_one_on_xe() {
        let (class, dev) = class_folder(
            "pick",
            &[("renderD128", "0000:01:00.0", 0x10DE, "nvidia"), ("renderD129", "0000:00:02.0", 0x8086, "xe")],
        );
        let node = intel_render_node_in(&class, &dev, None).unwrap();
        assert_eq!(node.device_file, dev.join("renderD129"));
        assert_eq!(node.driver, "xe");
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_intel_card_on_i915_is_passed_over_and_the_probe_can_say_so() {
        let (class, dev) = class_folder("i915", &[("renderD128", "0000:00:02.0", 0x8086, "i915")]);
        assert_eq!(intel_render_node_in(&class, &dev, None), None);
        assert_eq!(intel_nodes_in(&class), [("renderD128".into(), Some("i915".into()))]);
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_address_narrows_it_to_one_card() {
        let (class, dev) = class_folder(
            "addr",
            &[("renderD128", "0000:00:02.0", 0x8086, "xe"), ("renderD129", "0000:03:00.0", 0x8086, "xe")],
        );
        let node = intel_render_node_in(&class, &dev, Some("0000:03:00.0")).unwrap();
        assert_eq!(node.device_file, dev.join("renderD129"));
        assert_eq!(intel_render_node_in(&class, &dev, Some("0000:09:00.0")), None);
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }
}
