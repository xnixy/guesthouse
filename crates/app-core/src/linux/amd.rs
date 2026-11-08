//! An AMD card for a Linux guest. The guest gets a virtio GPU whose 3D runs on the host's card, through
//! virglrenderer in QEMU. Native context hands the guest's own radeonsi and RADV the card's kernel
//! driver, and needs a Mesa built for it in the guest. Venus is the fallback, which runs the guest's
//! Vulkan on the host's RADV and needs nothing built: a guest whose build failed boots on it next.
//!
//! A card is found by AMD's vendor ID in the DRM class, as render node numbers move between boots.

use crate::context::Context;
use crate::machine::{Machine, sorted_names};
use crate::setup::cards::{find_cards, product_name};
use crate::usb::sysfs_text;
use std::fs;
use std::path::{Path, PathBuf};

const AMD_VENDOR: &str = "0x1002";
const AMD_NAMES_START: &str = "\n1002  ";
const RENDER_PREFIX: &str = "renderD";
const DRM_DEVICE_LINK: &str = "device";
/// Where the render nodes are, which the DRM class names.
const RENDER_NODES: &str = "/dev/dri";
/// Written in the machine's folder when the guest said native context failed, and why.
const FALLBACK_FILE: &str = "amd-fallback";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmdCard {
    /// Where the card is on the PCI bus, as sysfs names it.
    pub pci: String,
    /// Its render node, as `/dev/dri/renderD128`.
    pub render_node: PathBuf,
    pub name: String,
}

/// How the guest's 3D reaches the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmdMode {
    /// The guest's own radeonsi and RADV, on the card's kernel driver.
    Native,
    /// The guest's Vulkan on the host's RADV, which works with the distro's own Mesa.
    Venus,
}

impl AmdMode {
    /// The word the guest reads from fw_cfg.
    pub fn word(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Venus => "venus",
        }
    }
}

fn hex_number(text: &str) -> Option<u32> {
    u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

fn card_of(drm_class: &Path, names: &Path, node: &str) -> Option<AmdCard> {
    let device = drm_class.join(node).join(DRM_DEVICE_LINK);
    if sysfs_text(&device.join("vendor")) != AMD_VENDOR {
        return None;
    }
    let pci = fs::read_link(&device).ok()?.file_name()?.to_string_lossy().into_owned();
    let number = hex_number(&sysfs_text(&device.join("device")))?;
    let product =
        product_name(AMD_NAMES_START, number, names).unwrap_or_else(|| format!("card {number:04x}"));
    Some(AmdCard { pci, render_node: Path::new(RENDER_NODES).join(node), name: format!("AMD {product}") })
}

/// The AMD cards with a render node, by PCI address. Any of them will do, iGPU or not, and so will the
/// one that drives the host's desktop.
pub fn amd_cards(context: &Context) -> Vec<AmdCard> {
    let nodes = sorted_names(&context.drm_class).into_iter().filter(|name| name.starts_with(RENDER_PREFIX));
    let mut cards: Vec<AmdCard> =
        nodes.filter_map(|node| card_of(&context.drm_class, &context.pci_names, &node)).collect();
    cards.sort_by(|one, other| one.pci.cmp(&other.pci));
    cards
}

/// The card at this address, if it's still in this PC.
pub fn amd_card(context: &Context, pci: &str) -> Option<AmdCard> {
    amd_cards(context).into_iter().find(|card| card.pci == pci)
}

/// The GPU a new machine's Linux gets: the NVIDIA card if this PC has one, and else the first AMD card.
/// Empty is NVIDIA's, as for a machine from before AMD cards.
pub fn default_linux_gpu(context: &Context) -> String {
    if !find_cards(context).is_empty() {
        return String::new();
    }
    amd_cards(context).into_iter().next().map(|card| card.pci).unwrap_or_default()
}

fn fallback_file(context: &Context, machine: &Machine) -> PathBuf {
    machine.folder(context).join(FALLBACK_FILE)
}

/// Venus if the guest said native context failed, and native context until then.
pub fn amd_mode(context: &Context, machine: &Machine) -> AmdMode {
    if fallback_file(context, machine).exists() { AmdMode::Venus } else { AmdMode::Native }
}

/// Why the machine is on Venus, if it is.
pub fn fallback_reason(context: &Context, machine: &Machine) -> Option<String> {
    fs::read_to_string(fallback_file(context, machine)).ok().map(|text| text.trim().to_owned())
}

/// The next boot goes to Venus, for this reason.
pub fn fall_back(context: &Context, machine: &Machine, reason: &str) -> std::io::Result<()> {
    fs::write(fallback_file(context, machine), format!("{reason}\n"))
}

/// The next boot tries native context again.
pub fn try_native_again(context: &Context, machine: &Machine) {
    let _ = fs::remove_file(fallback_file(context, machine));
}

#[cfg(test)]
pub mod stand_in {
    use crate::testing::Scratch;
    use std::os::unix::fs::symlink;

    /// A render node in the DRM class and the PCI device it belongs to.
    pub fn fake_render_node(scratch: &Scratch, node: u32, pci: &str, vendor: &str, device: u32) {
        scratch.write(&format!("pci/{pci}/vendor"), format!("{vendor}\n"));
        scratch.write(&format!("pci/{pci}/device"), format!("0x{device:04x}\n"));
        let folder = scratch.path(&format!("drm/renderD{node}"));
        std::fs::create_dir_all(&folder).unwrap();
        symlink(scratch.path(&format!("pci/{pci}")), folder.join("device")).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::fake_render_node;
    use super::*;
    use crate::testing::{Scratch, scratch_context};

    const NAMES: &str = "1001  Other\n\t13c0  Not this one\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\
                         \t13c0  Granite Ridge [Radeon 610M]\n\t7550  Navi 48 [Radeon RX 9070/9070 XT/9070 GRE]\n\
                         \t67df  Ellesmere [Radeon RX 470/480/570/570X/580/580X/590]\n1003  Next\n";

    fn context(scratch: &Scratch) -> Context {
        let mut context = scratch_context(scratch);
        context.drm_class = scratch.path("drm");
        context.pci_names = scratch.write("pci.ids", NAMES);
        context
    }

    #[test]
    fn every_amd_card_is_found_by_its_vendor_and_not_its_node_number() {
        let scratch = Scratch::new();
        fake_render_node(&scratch, 128, "0000:7c:00.0", "0x1002", 0x13c0);
        fake_render_node(&scratch, 129, "0000:02:00.0", "0x10de", 0x2b85);
        fake_render_node(&scratch, 130, "0000:03:00.0", "0x1002", 0x7550);
        let cards = amd_cards(&context(&scratch));
        let found: Vec<(&str, &str)> =
            cards.iter().map(|card| (card.pci.as_str(), card.render_node.to_str().unwrap())).collect();
        assert_eq!(found, [("0000:03:00.0", "/dev/dri/renderD130"), ("0000:7c:00.0", "/dev/dri/renderD128")]);
        assert_eq!(cards[0].name, "AMD Radeon RX 9070/9070 XT/9070 GRE");
        assert_eq!(cards[1].name, "AMD Radeon 610M");
    }

    #[test]
    fn a_card_pci_ids_does_not_know_still_has_a_name() {
        let scratch = Scratch::new();
        fake_render_node(&scratch, 128, "0000:05:00.0", "0x1002", 0x7fff);
        assert_eq!(amd_card(&context(&scratch), "0000:05:00.0").unwrap().name, "AMD card 7fff");
        assert_eq!(amd_card(&context(&scratch), "0000:06:00.0"), None);
    }

    #[test]
    fn a_new_machine_gets_the_nvidia_card_if_there_is_one_and_else_the_first_amd_card() {
        let scratch = Scratch::new();
        let mut context = context(&scratch);
        context.pci_devices = scratch.path("pci");
        assert_eq!(default_linux_gpu(&context), "");
        fake_render_node(&scratch, 128, "0000:7c:00.0", "0x1002", 0x13c0);
        assert_eq!(default_linux_gpu(&context), "0000:7c:00.0");
        crate::setup::cards::stand_in::fake_pci(&scratch, "0000:02:00.0", 0x2b85, "0x10de", "0x030000");
        assert_eq!(default_linux_gpu(&context), "");
    }

    #[test]
    fn a_failed_native_context_means_venus_until_it_is_tried_again() {
        let scratch = Scratch::new();
        let context = context(&scratch);
        let machine = Machine::new("ubuntu", "Ubuntu", "/m/disk.qcow2", "");
        fs::create_dir_all(machine.folder(&context)).unwrap();
        assert_eq!(amd_mode(&context, &machine), AmdMode::Native);
        fall_back(&context, &machine, "the Mesa build failed").unwrap();
        assert_eq!(amd_mode(&context, &machine), AmdMode::Venus);
        assert_eq!(fallback_reason(&context, &machine).unwrap(), "the Mesa build failed");
        try_native_again(&context, &machine);
        assert_eq!(amd_mode(&context, &machine), AmdMode::Native);
    }
}
