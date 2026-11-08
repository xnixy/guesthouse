//! A Linux guest on an AMD card: its screen is a virtio GPU whose 3D virglrenderer runs on the card,
//! in the plain screen's place on the bus, and QEMU's D-Bus display reads the frames back for the
//! viewer. Nothing of NVIDIA's goes in, and QEMU's Mesa can only see the AMD card: its jail grants that
//! one render node, and its environment names RADV and Mesa's EGL as the only drivers to load.

use super::arguments::words;
use super::{Boot, UbuntuBoot, VmPlan};
use crate::linux::amd::{AmdCard, AmdMode};
use crate::text::qemu_value;
use guesthouse_jail::{Grant, Plan};
use std::path::{Path, PathBuf};

const GL_DEVICE: &str = "virtio-vga-gl";
/// The window the guest maps the card's memory through.
const HOST_MEMORY: &str = "8G";
/// Where the guest reads which way its 3D goes.
const MODE_NAME: &str = "opt/guesthouse/amd-mode";
/// Mesa's EGL, so glvnd never loads another vendor's.
const MESA_EGL: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
/// Where Venus's render server is, beside QEMU. The library names where it was built.
pub const RENDER_SERVER: &str = "virgl_render_server";
/// The shaders QEMU's Mesa compiles, kept in the run folder, which is QEMU's to write.
const SHADER_CACHE: &str = "cache";
/// Where the Vulkan loader finds its drivers, and RADV's manifest among them.
const VULKAN_DRIVER_FOLDERS: [&str; 2] = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"];
const RADV_MANIFEST_PREFIX: &str = "radeon_icd";
/// A Linux guest's own setup image, when the user installed it.
const SETUP_DRIVE: &str = "gsetup";

/// What a boot on an AMD card needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmdBoot {
    pub card: AmdCard,
    pub mode: AmdMode,
    /// The guest's scripts, for a Linux the user installed to set itself up from.
    pub setup_image: Option<PathBuf>,
}

/// RADV's manifest on this PC, the only Vulkan driver QEMU may load. With none, the loader is pointed
/// at nothing, so Venus finds no device rather than another vendor's.
pub fn radv_manifest(folders: &[&Path]) -> PathBuf {
    let found = folders.iter().find_map(|folder| {
        let names = crate::machine::sorted_names(folder);
        let name = names.into_iter().find(|name| name.starts_with(RADV_MANIFEST_PREFIX))?;
        Some(folder.join(name))
    });
    found.unwrap_or_else(|| PathBuf::from(VULKAN_DRIVER_FOLDERS[0]).join("radeon_icd.none.json"))
}

impl VmPlan {
    /// The AMD card, if this boot has it.
    pub fn amd_boot(&self) -> Option<&AmdBoot> {
        let (Boot::Ubuntu(UbuntuBoot::Amd(amd)) | Boot::OwnLinux { gpu: UbuntuBoot::Amd(amd), .. }) =
            &self.boot
        else {
            return None;
        };
        Some(amd)
    }

    pub fn has_amd_gpu(&self) -> bool {
        self.amd_boot().is_some()
    }

    /// The screen in the plain one's place: `-vga virtio` is the GL device on a GL display, with its
    /// settings as globals, as the plain screen has its own.
    pub(super) fn amd_screen_arguments(amd: &AmdBoot, size: Option<(u64, u64)>) -> Vec<String> {
        let context = match amd.mode {
            AmdMode::Native => "drm_native_context",
            AmdMode::Venus => "venus",
        };
        let mut arguments = words(&["-vga", "virtio"]);
        let mut settings =
            vec![format!("{context}=on"), "blob=on".to_owned(), format!("hostmem={HOST_MEMORY}")];
        if let Some((width, height)) = size {
            settings.extend([format!("xres={width}"), format!("yres={height}")]);
        }
        for setting in settings {
            arguments.extend(words(&["-global", &format!("{GL_DEVICE}.{setting}")]));
        }
        let node = qemu_value(&amd.card.render_node.to_string_lossy());
        arguments.extend(words(&["-display", &format!("dbus,p2p=yes,gl=on,rendernode={node}")]));
        arguments
    }

    /// What the guest is told, and for a Linux the user installed, its setup image.
    pub(super) fn amd_guest_arguments(amd: &AmdBoot) -> Vec<String> {
        let mut arguments = words(&["-fw_cfg", &format!("name={MODE_NAME},string={}", amd.mode.word())]);
        if let Some(image) = &amd.setup_image {
            let file = qemu_value(&image.to_string_lossy());
            arguments.extend(words(&[
                "-drive",
                &format!("if=none,id={SETUP_DRIVE},file={file},format=raw,readonly=on"),
                "-device",
                &format!("virtio-blk-pci,drive={SETUP_DRIVE}"),
            ]));
        }
        arguments
    }

    fn render_server(&self) -> PathBuf {
        self.qemu.with_file_name(RENDER_SERVER)
    }

    /// QEMU's Mesa and virglrenderer may load RADV and Mesa's EGL and nothing else. Venus's render
    /// server is the one beside QEMU.
    pub(super) fn amd_environment(&self) -> Vec<(String, String)> {
        if !self.has_amd_gpu() {
            return Vec::new();
        }
        let folders: Vec<&Path> = VULKAN_DRIVER_FOLDERS.iter().map(Path::new).collect();
        let cache = self.run_folder.join(SHADER_CACHE);
        vec![
            ("VK_DRIVER_FILES".to_owned(), radv_manifest(&folders).display().to_string()),
            ("__EGL_VENDOR_LIBRARY_FILENAMES".to_owned(), MESA_EGL.to_owned()),
            ("RENDER_SERVER_EXEC_PATH".to_owned(), self.render_server().display().to_string()),
            ("XDG_CACHE_HOME".to_owned(), cache.display().to_string()),
        ]
    }

    /// The card's render node is the only device of a GPU's it gets, and its folder can only be listed.
    pub(super) fn grant_amd(&self, plan: &mut Plan) {
        let Some(amd) = self.amd_boot() else { return };
        plan.grant(Grant::Device, &amd.card.render_node);
        // libdrm finds the card's other nodes by listing its folder, and opens none of them.
        if let Some(folder) = amd.card.render_node.parent() {
            plan.grant(Grant::List, folder);
        }
        if let Some(image) = &amd.setup_image {
            plan.grant(Grant::Read, image);
        }
    }

    /// Venus starts its render server, and native context runs in QEMU.
    pub(super) fn starts_render_server(&self) -> bool {
        self.amd_boot().is_some_and(|amd| amd.mode == AmdMode::Venus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{Kind, Machine};
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};

    fn card() -> AmdCard {
        AmdCard { pci: "0000:7c:00.0".into(), render_node: "/dev/dri/renderD128".into(), name: "AMD".into() }
    }

    fn amd(mode: AmdMode) -> AmdBoot {
        AmdBoot { card: card(), mode, setup_image: None }
    }

    fn ubuntu(scratch: &Scratch, mode: AmdMode) -> VmPlan {
        let machine = Machine { kind: Kind::Linux, ..Machine::new("ubuntu", "Ubuntu", "/m/disk.qcow2", "") };
        let boot = Boot::Ubuntu(UbuntuBoot::Amd(amd(mode)));
        VmPlan::new(&scratch_context(scratch), &machine, boot, slots::first(), 0)
    }

    fn value_after(arguments: &[String], flag: &str) -> String {
        let at = arguments.iter().position(|word| word == flag).unwrap();
        arguments[at + 1].clone()
    }

    #[test]
    fn ubuntu_gets_the_gl_screen_on_the_card_and_nothing_of_nvidias() {
        let scratch = Scratch::new();
        let arguments = ubuntu(&scratch, AmdMode::Native).qemu_arguments();
        assert_eq!(value_after(&arguments, "-vga"), "virtio");
        assert_eq!(value_after(&arguments, "-display"), "dbus,p2p=yes,gl=on,rendernode=/dev/dri/renderD128");
        for global in
            ["virtio-vga-gl.drm_native_context=on", "virtio-vga-gl.blob=on", "virtio-vga-gl.hostmem=8G"]
        {
            assert!(arguments.iter().any(|word| word == global), "{global}");
        }
        assert_eq!(value_after(&arguments, "-fw_cfg"), "name=opt/guesthouse/amd-mode,string=native");
        assert!(!arguments.iter().any(|word| word.contains("nvfwd") || word == "std"), "{arguments:?}");
        let venus = ubuntu(&scratch, AmdMode::Venus).qemu_arguments();
        assert!(venus.iter().any(|word| word == "virtio-vga-gl.venus=on"));
        assert_eq!(value_after(&venus, "-fw_cfg"), "name=opt/guesthouse/amd-mode,string=venus");
    }

    #[test]
    fn qemus_mesa_can_load_nothing_but_radv_and_mesas_egl() {
        let scratch = Scratch::new();
        let plan = ubuntu(&scratch, AmdMode::Native);
        let environment = plan.amd_environment();
        let value = |name: &str| environment.iter().find(|(key, _)| key == name).unwrap().1.clone();
        assert!(value("VK_DRIVER_FILES").contains("radeon_icd"), "{}", value("VK_DRIVER_FILES"));
        assert_eq!(value("__EGL_VENDOR_LIBRARY_FILENAMES"), MESA_EGL);
        assert!(value("RENDER_SERVER_EXEC_PATH").ends_with("/virgl_render_server"));
        assert!(value("XDG_CACHE_HOME").starts_with(&plan.run_folder.display().to_string()));
    }

    #[test]
    fn radvs_manifest_is_found_where_the_loader_looks_and_none_names_nothing() {
        let scratch = Scratch::new();
        scratch.write("icd/nvidia_icd.json", "{}");
        scratch.write("icd/radeon_icd.x86_64.json", "{}");
        let folder = scratch.path("icd");
        assert_eq!(radv_manifest(&[&scratch.path("none"), &folder]), folder.join("radeon_icd.x86_64.json"));
        let nothing = radv_manifest(&[&scratch.path("none")]);
        assert!(!nothing.exists() && nothing.to_string_lossy().contains("radeon_icd"));
    }

    #[test]
    fn the_jail_grants_the_cards_render_node_and_no_other() {
        let scratch = Scratch::new();
        let mut rules = Plan::default();
        ubuntu(&scratch, AmdMode::Native).grant_amd(&mut rules);
        let devices: Vec<&PathBuf> =
            rules.paths.iter().filter(|(grant, _)| *grant == Grant::Device).map(|(_, path)| path).collect();
        assert_eq!(devices, [&PathBuf::from("/dev/dri/renderD128")]);
        let anything_else: Vec<_> = rules.paths.iter().filter(|(grant, _)| *grant != Grant::Device).collect();
        assert_eq!(anything_else, [&(Grant::List, PathBuf::from("/dev/dri"))], "only listed");
    }

    #[test]
    fn only_venus_starts_the_render_server() {
        let scratch = Scratch::new();
        assert!(!ubuntu(&scratch, AmdMode::Native).starts_render_server());
        assert!(ubuntu(&scratch, AmdMode::Venus).starts_render_server());
    }
}
