//! Which GPU draws the desktop. On most laptops it's the processor's own graphics, and the card the
//! guest runs on only renders. Another GPU can't take a buffer from that card's memory, so the viewer
//! asks for its frames in memory instead, and copies them.

use crate::context::Context;
use crate::gpu_maker::GpuMaker;
use crate::machine::Machine;
use crate::setup::cards::card_in_use;
use crate::usb::sysfs_text;
use std::fs;
use std::path::Path;

const NVIDIA_VENDOR: &str = "0x10de";
const CONNECTED: &str = "connected";
/// Set to 1 or 0 to choose, whatever the check says.
pub const MEMORY_FRAMES_VARIABLE: &str = "GUESTHOUSE_MEMORY_FRAMES";

/// Where on the PCI bus the card a machine's GPU-PV runs on is, as `0000:01:00.0`: the card it chose,
/// or the NVIDIA card setup chose. Empty if it isn't known.
pub fn machine_card(context: &Context, machine: &Machine) -> String {
    match machine.windows_maker() {
        Some(GpuMaker::Amd | GpuMaker::Intel) => return machine.amd_gpu.clone(),
        Some(GpuMaker::Nvidia) => {}
        // A guest that isn't Windows has its card in the same place, if it has one.
        None if !machine.amd_gpu.is_empty() => return machine.amd_gpu.clone(),
        None => {}
    }
    card_in_use(context).map(|card| card.pci).unwrap_or_default()
}

/// Whether a GPU other than `card` has a screen connected, from sysfs's DRM class folder. Its
/// connectors are named after their card, as `card1-eDP-1`. With no card known, any GPU that isn't
/// NVIDIA's counts, as that's the card a setup from before addresses were kept has.
pub fn screen_on_other_gpu(drm_class: &Path, card: &str) -> bool {
    let Ok(entries) = fs::read_dir(drm_class) else { return false };
    entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).any(|name| {
        let Some((drm_card, _)) = name.split_once('-').filter(|(drm_card, _)| drm_card.starts_with("card"))
        else {
            return false;
        };
        if sysfs_text(&drm_class.join(&name).join("status")) != CONNECTED {
            return false;
        }
        let device = drm_class.join(drm_card).join("device");
        if card.is_empty() {
            let vendor = sysfs_text(&device.join("vendor"));
            return !vendor.is_empty() && vendor != NVIDIA_VENDOR;
        }
        let address = fs::read_link(&device).ok();
        address.is_none_or(|address| address.file_name().is_none_or(|found| found != card))
    })
}

/// What the variable says, if it says 1 or 0.
pub fn chosen(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim) {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

/// Whether a viewer of this machine should ask for its frames in memory. A copy can't show HDR, so
/// then the machine's Windows stays in SDR.
pub fn memory_frames(context: &Context, machine: &Machine) -> bool {
    let variable = std::env::var(MEMORY_FRAMES_VARIABLE).ok();
    chosen(variable.as_deref())
        .unwrap_or_else(|| screen_on_other_gpu(&context.drm_class, &machine_card(context, machine)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;
    use std::os::unix::fs::symlink;

    fn card(scratch: &Scratch, card: &str, pci: &str, vendor: &str, connectors: &[(&str, &str)]) {
        scratch.write(&format!("pci/{pci}/vendor"), format!("{vendor}\n"));
        let folder = scratch.path(&format!("drm/{card}"));
        fs::create_dir_all(&folder).unwrap();
        symlink(scratch.path(&format!("pci/{pci}")), folder.join("device")).unwrap();
        for (connector, status) in connectors {
            scratch.write(&format!("drm/{card}-{connector}/status"), format!("{status}\n"));
        }
    }

    const NVIDIA: &str = "0000:01:00.0";
    const IGPU: &str = "0000:7c:00.0";
    const AMD_CARD: &str = "0000:03:00.0";

    #[test]
    fn a_laptops_screen_on_its_own_graphics_is_found() {
        let scratch = Scratch::new();
        card(&scratch, "card0", NVIDIA, "0x10de", &[]);
        card(&scratch, "card1", IGPU, "0x8086", &[("eDP-1", "connected"), ("DP-1", "disconnected")]);
        assert!(screen_on_other_gpu(&scratch.path("drm"), NVIDIA));
        assert!(screen_on_other_gpu(&scratch.path("drm"), ""));
    }

    #[test]
    fn an_amd_laptops_screen_counts_too() {
        let scratch = Scratch::new();
        card(&scratch, "card0", NVIDIA, "0x10de", &[]);
        card(&scratch, "card1", IGPU, "0x1002", &[("eDP-1", "connected")]);
        assert!(screen_on_other_gpu(&scratch.path("drm"), NVIDIA));
    }

    #[test]
    fn a_screen_on_the_guests_own_card_keeps_its_buffers() {
        let scratch = Scratch::new();
        card(&scratch, "card0", NVIDIA, "0x10de", &[("DP-1", "connected")]);
        card(&scratch, "card1", AMD_CARD, "0x1002", &[("HDMI-A-1", "disconnected")]);
        assert!(!screen_on_other_gpu(&scratch.path("drm"), NVIDIA));
        assert!(!screen_on_other_gpu(&scratch.path("drm"), ""));
        assert!(!screen_on_other_gpu(&scratch.path("none"), NVIDIA));
    }

    #[test]
    fn an_amd_guest_goes_by_its_own_card_not_by_maker() {
        let scratch = Scratch::new();
        card(&scratch, "card0", AMD_CARD, "0x1002", &[("DP-1", "connected")]);
        card(&scratch, "card1", IGPU, "0x1002", &[("eDP-1", "disconnected")]);
        assert!(!screen_on_other_gpu(&scratch.path("drm"), AMD_CARD), "its card draws the desktop");
        card(&scratch, "card2", NVIDIA, "0x10de", &[("HDMI-A-1", "connected")]);
        assert!(screen_on_other_gpu(&scratch.path("drm"), AMD_CARD), "a screen on the NVIDIA card");
    }

    #[test]
    fn the_variable_decides_when_it_says_one_or_zero() {
        assert_eq!(chosen(Some("1")), Some(true));
        assert_eq!(chosen(Some("0")), Some(false));
        assert_eq!(chosen(Some("yes")), None);
        assert_eq!(chosen(None), None);
    }
}
