//! What sysfs says about the card, without asking the driver anything.
//!
//! The PCI identity is the card's own config space, as the kernel shows it. The frequencies are
//! xe's view of the render GT, in MHz, from its `freq0` folder. A file that isn't there is `None`:
//! older kernels have fewer of them.

use std::fs;
use std::path::Path;

const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";
/// Where a modular xe keeps the checksum of its source, which tells one build from another.
const XE_SOURCE_VERSION: &str = "/sys/module/xe/srcversion";
/// The render GT's frequency folder, below the device's.
const RENDER_FREQUENCIES: &str = "tile0/gt0/freq0";

/// How the card's PCI config space identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PciIdentity {
    pub vendor: u16,
    pub device: u16,
    pub subsystem_vendor: u16,
    pub subsystem_device: u16,
    pub revision: u8,
    pub class: u32,
}

impl PciIdentity {
    /// Reads the identity from a PCI device's sysfs folder. A missing file reads as zero.
    pub fn read(device: &Path) -> Self {
        let number = |name: &str| hex_file(&device.join(name)).unwrap_or(0);
        Self {
            vendor: number("vendor") as u16,
            device: number("device") as u16,
            subsystem_vendor: number("subsystem_vendor") as u16,
            subsystem_device: number("subsystem_device") as u16,
            revision: number("revision") as u8,
            class: number("class") as u32,
        }
    }
}

/// The render GT's frequencies, in MHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frequencies {
    /// The most the hardware can run at, RP0.
    pub highest: Option<u32>,
    /// The least it runs at, RPn.
    pub lowest: Option<u32>,
    /// The most efficient, RPe.
    pub efficient: Option<u32>,
    /// The limits the host has set, which a user can change.
    pub min_set: Option<u32>,
    pub max_set: Option<u32>,
}

impl Frequencies {
    pub fn read(device: &Path) -> Self {
        let folder = device.join(RENDER_FREQUENCIES);
        let mhz = |name: &str| decimal_file(&folder.join(name));
        Self {
            highest: mhz("rp0_freq"),
            lowest: mhz("rpn_freq"),
            efficient: mhz("rpe_freq"),
            min_set: mhz("min_freq"),
            max_set: mhz("max_freq"),
        }
    }
}

/// The running kernel's release, as `uname -r` gives it.
pub fn kernel_release() -> Option<String> {
    Some(fs::read_to_string(KERNEL_RELEASE).ok()?.trim().to_owned())
}

/// xe's source checksum, when xe is a module.
pub fn xe_source_version() -> Option<String> {
    Some(fs::read_to_string(XE_SOURCE_VERSION).ok()?.trim().to_owned())
}

/// The PCI address a device folder stands for, from where its link leads.
pub fn pci_address(device: &Path) -> Option<String> {
    Some(fs::canonicalize(device).ok()?.file_name()?.to_string_lossy().into_owned())
}

fn hex_file(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    u64::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

fn decimal_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, process};

    #[test]
    fn the_identity_and_frequencies_come_from_their_files() {
        let device = env::temp_dir().join(format!("guesthouse-intel-sysfs-{}", process::id()));
        let _ = fs::remove_dir_all(&device);
        fs::create_dir_all(device.join(RENDER_FREQUENCIES)).unwrap();
        for (name, text) in [
            ("vendor", "0x8086\n"),
            ("device", "0x64a0\n"),
            ("subsystem_vendor", "0x17aa\n"),
            ("subsystem_device", "0x2339\n"),
            ("revision", "0x04\n"),
            ("class", "0x030000\n"),
        ] {
            fs::write(device.join(name), text).unwrap();
        }
        fs::write(device.join(RENDER_FREQUENCIES).join("rp0_freq"), "2050\n").unwrap();
        fs::write(device.join(RENDER_FREQUENCIES).join("rpn_freq"), "400\n").unwrap();
        let identity = PciIdentity::read(&device);
        assert_eq!((identity.vendor, identity.device, identity.revision), (0x8086, 0x64A0, 4));
        assert_eq!((identity.subsystem_vendor, identity.subsystem_device), (0x17AA, 0x2339));
        assert_eq!(identity.class, 0x030000);
        let frequencies = Frequencies::read(&device);
        assert_eq!(
            (frequencies.highest, frequencies.lowest, frequencies.efficient),
            (Some(2050), Some(400), None)
        );
        fs::remove_dir_all(device).unwrap();
    }
}
