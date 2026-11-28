//! An Android guest's arguments. Android for VMs (LineageOS's virtio target) boots from its own disk
//! through OVMF and GRUB, keeps its data on a second disk, and draws through QEMU's virtio GPU on the
//! host's card: OpenGL ES through virgl and Vulkan through Venus, whichever maker's card it is. Nothing
//! of the host's driver goes into the guest, and no daemon runs beside it.
//!
//! The screen is the GL device on QEMU's D-Bus display, which reads the frames back for the viewer as it
//! does for a Linux on an AMD card. The keyboard and pointer are QEMU's.

use super::amd::{RENDER_SERVER, SHADER_CACHE, VULKAN_DRIVER_FOLDERS, mesa_egl, radv_manifest};
use super::arguments::words;
use super::disk_node::{DiskOptions, disk_node};
use super::forwarding_jail::{DEVICES, gpu_nodes};
use super::macos::UDMABUF;
use super::{Boot, VmPlan};
use crate::disk_image::backing::{BackingFile, DISK_FORMAT};
use crate::firmware::format_of;
use crate::gpu_maker::GpuMaker;
use crate::linux::card::ForwardedCard;
use crate::render_cards::RenderCard;
use crate::text::qemu_value;
use guesthouse_jail::{Grant, Plan};
use std::path::{Path, PathBuf};

/// The host memory the GPU maps into the guest for Venus's buffers.
const HOST_MEMORY: &str = "4G";
/// Android's buffer allocator marks every shared buffer for use across devices, and the guest's kernel
/// refuses that unless the GPU hands out resource UUIDs. Our QEMU's patch adds them to the GL device.
const GPU_SETTINGS: [&str; 4] = ["venus=on", "blob=on", "resource_uuid=on", "hostmem"];
/// Android's logcat comes out on its second console, and the guest's own shell on its first.
pub const LOGCAT_FILE: &str = "logcat.log";
const KERNEL_LOG_FILE: &str = "serial.log";
/// NVIDIA's Vulkan and EGL manifests, where its driver puts them, and on NixOS.
const NVIDIA_ICD: &str = "nvidia_icd.json";
const NVIDIA_EGL: [&str; 2] = [
    "/usr/share/glvnd/egl_vendor.d/10_nvidia.json",
    "/run/opengl-driver/share/glvnd/egl_vendor.d/10_nvidia.json",
];
/// Intel's Vulkan driver, ANV.
const INTEL_ICD_PREFIX: &str = "intel_icd";
/// NVIDIA's Vulkan driver opens this beside the card's own nodes.
const NVIDIA_MODESET: &str = "nvidia-modeset";
const RENDER_NODES: &str = "dri";
/// The SKU's first word, so a guest can tell Guesthouse's choices from a SKU of its own.
const CHOICES_MARK: &str = "guesthouse";
const PLAY_CHOICE: &str = "microg";

/// The card an Android guest draws on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidCard {
    pub maker: GpuMaker,
    /// Its render node, which QEMU's display and virglrenderer open.
    pub render_node: PathBuf,
    /// Every device file of the card a jailed QEMU may open.
    pub nodes: Vec<PathBuf>,
}

impl AndroidCard {
    /// An AMD or Intel card, whose render node is all of it the drivers open.
    pub fn render(maker: GpuMaker, card: &RenderCard) -> Self {
        Self { maker, render_node: card.render_node.clone(), nodes: vec![card.render_node.clone()] }
    }

    /// An NVIDIA card, whose Vulkan driver opens its control nodes too.
    pub fn nvidia(card: &ForwardedCard) -> Option<Self> {
        let devices = Path::new(DEVICES);
        let render_node = devices.join(RENDER_NODES).join(card.render_node.as_ref()?);
        let mut nodes = gpu_nodes(devices, card);
        nodes.extend(Some(devices.join(NVIDIA_MODESET)).filter(|node| node.exists()));
        Some(Self { maker: GpuMaker::Nvidia, render_node, nodes })
    }

    /// The Vulkan driver QEMU's render server may load: this maker's and no other's.
    fn vulkan_driver(&self) -> PathBuf {
        let folders: Vec<&Path> = VULKAN_DRIVER_FOLDERS.iter().map(Path::new).collect();
        let named = |prefix: &str| {
            folders.iter().find_map(|folder| {
                [format!("{prefix}.{}.json", std::env::consts::ARCH), format!("{prefix}.json")]
                    .into_iter()
                    .map(|name| folder.join(name))
                    .find(|path| path.exists())
            })
        };
        match self.maker {
            GpuMaker::Amd => radv_manifest(&folders),
            GpuMaker::Nvidia => named(NVIDIA_ICD.trim_end_matches(".json"))
                .unwrap_or_else(|| Path::new(VULKAN_DRIVER_FOLDERS[0]).join(NVIDIA_ICD)),
            GpuMaker::Intel => named(INTEL_ICD_PREFIX)
                .unwrap_or_else(|| Path::new(VULKAN_DRIVER_FOLDERS[0]).join("intel_icd.none.json")),
        }
    }

    /// The EGL QEMU's display and virgl use: NVIDIA's for its card, Mesa's for the others.
    fn egl_vendor(&self) -> &'static str {
        match self.maker {
            GpuMaker::Nvidia => {
                NVIDIA_EGL.into_iter().find(|path| Path::new(path).exists()).unwrap_or(NVIDIA_EGL[0])
            }
            GpuMaker::Amd | GpuMaker::Intel => mesa_egl(),
        }
    }
}

/// What an Android guest boots with beside its system disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidBoot {
    /// Android's own disk for its data, which the system disk's fstab names as the second one.
    pub data: PathBuf,
    /// For a throwaway run, the machine's own data disk, which `data` is an overlay on.
    pub data_base: Option<PathBuf>,
    pub card: AndroidCard,
    /// A folder of the machine's for the drivers' compiled shaders, so a start doesn't build them again.
    pub cache: PathBuf,
    /// Play services through microG.
    pub play: bool,
}

impl AndroidBoot {
    /// The files QEMU reads the data disk through: a throwaway run's is an overlay on the machine's
    /// own, which stands alone.
    pub(super) fn data_chain(&self) -> Vec<BackingFile> {
        let base = self.data_base.iter();
        base.map(|base| BackingFile {
            path: base.to_string_lossy().into_owned(),
            format: DISK_FORMAT.to_owned(),
        })
        .collect()
    }
}

impl VmPlan {
    pub(super) fn android_boot(&self) -> Option<&AndroidBoot> {
        match &self.boot {
            Boot::Android(boot) => Some(boot),
            _ => None,
        }
    }

    fn android_machine(&self) -> Vec<String> {
        words(&[
            "-name",
            &self.name,
            "-machine",
            "q35,accel=kvm",
            "-cpu",
            "host",
            "-smp",
            &format!("{0},sockets=1,cores={0}", self.cores),
            "-m",
            &self.ram,
        ])
    }

    /// The machine's choices, as words in the SMBIOS SKU, which the image's GRUB reads and puts on the
    /// kernel's line for its setup app.
    fn android_choices(boot: &AndroidBoot) -> Vec<String> {
        let mut choices = vec![CHOICES_MARK];
        if boot.play {
            choices.push(PLAY_CHOICE);
        }
        words(&["-smbios", &format!("type=1,sku={}", choices.join(" "))])
    }

    fn android_disks(&self, boot: &AndroidBoot) -> Vec<String> {
        let format = format_of(&self.firmware.code);
        words(&[
            "-drive",
            &format!("if=pflash,format={format},unit=0,readonly=on,file={}", self.firmware.code.display()),
            "-drive",
            &format!("if=pflash,format={format},unit=1,file={}", qemu_value(&self.vars)),
            "-blockdev",
            &disk_node("vda", &self.disk, &self.disk_chain(), DiskOptions { cache: None, discard: true }),
            "-device",
            "virtio-blk-pci,drive=vda,bootindex=0",
            "-blockdev",
            &disk_node(
                "vdb",
                &boot.data.to_string_lossy(),
                &boot.data_chain(),
                DiskOptions { cache: None, discard: true },
            ),
            "-device",
            "virtio-blk-pci,drive=vdb",
        ])
    }

    fn android_screen(&self, boot: &AndroidBoot) -> Vec<String> {
        let mut arguments = words(&["-vga", "virtio"]);
        let (width, height) = self.screen_size();
        let mut settings: Vec<String> = GPU_SETTINGS
            .iter()
            .map(|setting| match *setting {
                "hostmem" => format!("hostmem={HOST_MEMORY}"),
                setting => setting.to_owned(),
            })
            .collect();
        settings.extend([format!("xres={width}"), format!("yres={height}")]);
        for setting in settings {
            arguments.extend(words(&["-global", &format!("virtio-vga-gl.{setting}")]));
        }
        let node = qemu_value(&boot.card.render_node.to_string_lossy());
        arguments.extend(words(&["-display", &format!("dbus,p2p=yes,gl=on,rendernode={node}")]));
        arguments
    }

    fn android_devices(&self) -> Vec<String> {
        let (sound, sound_id) = self.sound_backend();
        words(&[
            "-device",
            "qemu-xhci,id=xhci",
            "-device",
            "usb-tablet,bus=xhci.0",
            "-device",
            "usb-kbd,bus=xhci.0",
            "-device",
            "virtio-rng-pci",
            "-audiodev",
            &sound,
            "-device",
            &format!("virtio-sound-pci,audiodev={sound_id},streams=2"),
            "-device",
            "virtio-serial",
            "-chardev",
            "null,id=android-shell",
            "-device",
            "virtconsole,chardev=android-shell",
            "-chardev",
            &format!("file,id=logcat,path={}", self.in_run_folder(LOGCAT_FILE)),
            "-device",
            "virtconsole,chardev=logcat",
        ])
    }

    fn android_outputs(&self) -> Vec<String> {
        let mut arguments = self.monitor_arguments();
        if self.passes_unix_sockets() {
            arguments.extend(words(&["-pidfile", &self.in_run_folder(super::PID_FILE)]));
        }
        arguments.extend(words(&[
            "-serial",
            &format!("file:{}", self.in_run_folder(KERNEL_LOG_FILE)),
            "-D",
            &self.in_run_folder("qemu.log"),
        ]));
        arguments
    }

    /// The whole line of an Android guest, or nothing for any other.
    pub(super) fn android_arguments(&self) -> Option<Vec<String>> {
        let boot = self.android_boot()?;
        let mut arguments = self.qemu_files_arguments();
        arguments.extend(self.android_machine());
        arguments.extend(Self::android_choices(boot));
        arguments.extend(self.android_disks(boot));
        arguments.extend(self.android_nic());
        arguments.extend(self.android_screen(boot));
        arguments.extend(self.android_devices());
        arguments.extend(self.android_outputs());
        arguments.extend(self.second_nic());
        arguments.extend(self.qemu_sandbox_arguments());
        Some(arguments)
    }

    /// What an Android guest's QEMU may reach beyond any jailed QEMU: its data disk, the card's nodes
    /// and no other card's, udmabuf for buffers in the guest's memory, and its shader cache.
    pub(super) fn grant_android(&self, plan: &mut Plan) {
        let Some(boot) = self.android_boot() else { return };
        plan.grant(Grant::Write, &boot.data).grant(Grant::Own, &boot.cache);
        if Path::new(UDMABUF).exists() {
            plan.grant(Grant::Device, UDMABUF);
        }
        for node in &boot.card.nodes {
            plan.grant(Grant::Device, node);
            if let Some(folder) = node.parent().filter(|folder| folder.ends_with(RENDER_NODES)) {
                plan.grant(Grant::List, folder);
            }
        }
        plan.grant(Grant::Read, self.qemu.with_file_name(RENDER_SERVER));
    }

    /// QEMU and its render server load the card's own Vulkan and EGL drivers and nothing else, and keep
    /// compiled shaders in the machine's cache.
    pub(super) fn android_environment(&self) -> Vec<(String, String)> {
        let Some(boot) = self.android_boot() else { return Vec::new() };
        vec![
            ("VK_DRIVER_FILES".to_owned(), boot.card.vulkan_driver().display().to_string()),
            ("__EGL_VENDOR_LIBRARY_FILENAMES".to_owned(), boot.card.egl_vendor().to_owned()),
            (
                "RENDER_SERVER_EXEC_PATH".to_owned(),
                self.qemu.with_file_name(RENDER_SERVER).display().to_string(),
            ),
            ("XDG_CACHE_HOME".to_owned(), boot.cache.join(SHADER_CACHE).display().to_string()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{Kind, Machine};
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};

    fn boot(maker: GpuMaker) -> AndroidBoot {
        AndroidBoot {
            data: "/m/droid/data.qcow2".into(),
            data_base: None,
            card: AndroidCard {
                maker,
                render_node: "/dev/dri/renderD129".into(),
                nodes: vec!["/dev/dri/renderD129".into()],
            },
            cache: "/m/droid/gpu-cache".into(),
            play: maker == GpuMaker::Nvidia,
        }
    }

    fn plan(maker: GpuMaker) -> VmPlan {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = Machine {
            kind: Kind::Plain,
            android_data: "/m/droid/data.qcow2".into(),
            ..Machine::new("droid", "Droid", "/m/droid/disk.qcow2", "/m/droid/OVMF_VARS.fd")
        };
        VmPlan::new(&context, &machine, Boot::Android(boot(maker)), slots::first(), 0)
    }

    fn value_after(arguments: &[String], flag: &str, starting: &str) -> Option<String> {
        arguments
            .windows(2)
            .find(|pair| pair[0] == flag && pair[1].starts_with(starting))
            .map(|pair| pair[1].clone())
    }

    #[test]
    fn it_boots_its_two_disks_on_virtio_and_draws_on_the_card_through_venus() {
        let arguments = plan(GpuMaker::Amd).qemu_arguments();
        let disk = value_after(&arguments, "-blockdev", "{").unwrap();
        assert!(
            disk.contains(r#""filename":"/m/droid/disk.qcow2""#) && disk.contains(r#""node-name":"vda""#)
        );
        let nodes: Vec<&String> =
            arguments.windows(2).filter(|two| two[0] == "-blockdev").map(|two| &two[1]).collect();
        assert!(nodes[1].contains("data.qcow2") && nodes[1].contains(r#""node-name":"vdb""#), "{nodes:?}");
        for setting in ["venus=on", "blob=on", "resource_uuid=on", "hostmem=4G"] {
            assert!(arguments.contains(&format!("virtio-vga-gl.{setting}")), "{setting}");
        }
        assert_eq!(
            value_after(&arguments, "-display", "dbus").unwrap(),
            "dbus,p2p=yes,gl=on,rendernode=/dev/dri/renderD129"
        );
        assert!(!arguments.iter().any(|word| word.contains("ahci")), "no SATA, Android has no driver for it");
        assert_eq!(value_after(&arguments, "-smbios", "type=1").unwrap(), "type=1,sku=guesthouse");
        let play = plan(GpuMaker::Nvidia).qemu_arguments();
        assert_eq!(value_after(&play, "-smbios", "type=1").unwrap(), "type=1,sku=guesthouse microg");
    }

    #[test]
    fn each_maker_gets_its_own_drivers_and_no_other() {
        let environment = |maker| plan(maker).qemu_environment();
        let value = |environment: &[(String, String)], name: &str| {
            environment.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()).unwrap()
        };
        let nvidia = environment(GpuMaker::Nvidia);
        assert!(value(&nvidia, "VK_DRIVER_FILES").ends_with(NVIDIA_ICD));
        assert!(value(&nvidia, "__EGL_VENDOR_LIBRARY_FILENAMES").ends_with("10_nvidia.json"));
        let amd = environment(GpuMaker::Amd);
        assert!(value(&amd, "VK_DRIVER_FILES").contains("radeon_icd"));
        assert!(value(&amd, "__EGL_VENDOR_LIBRARY_FILENAMES").ends_with("50_mesa.json"));
        let intel = environment(GpuMaker::Intel);
        assert!(value(&intel, "VK_DRIVER_FILES").contains("intel_icd"));
    }

    /// The data disk is written by QEMU as the system disk is, so its header is checked the same way:
    /// it stands alone, and a throwaway run's is an overlay on the machine's own and nothing else.
    #[test]
    fn the_data_disk_is_built_on_nothing_its_settings_dont_name() {
        let scratch = Scratch::new();
        let qemu_img = |words: &[&str]| {
            assert!(std::process::Command::new("qemu-img").args(words).status().unwrap().success());
        };
        let path = |name: &str| scratch.path(name).to_str().unwrap().to_owned();
        let (disk, data, run_data) = (path("disk.qcow2"), path("data.qcow2"), path("run/data.qcow2"));
        std::fs::create_dir_all(scratch.path("run")).unwrap();
        qemu_img(&["create", "-q", "-f", "qcow2", &disk, "1G"]);
        qemu_img(&["create", "-q", "-f", "qcow2", &data, "1G"]);
        qemu_img(&["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", &data, &run_data]);
        let base = VmPlan { disk: disk.clone(), ..plan(GpuMaker::Amd) };
        let with = |data: &str, data_base: Option<&str>| VmPlan {
            boot: Boot::Android(AndroidBoot {
                data: data.into(),
                data_base: data_base.map(Into::into),
                ..boot(GpuMaker::Amd)
            }),
            ..base.clone()
        };
        assert_eq!(with(&data, None).checked_backing_files().unwrap(), Vec::<std::path::PathBuf>::new());
        assert_eq!(
            with(&run_data, Some(&data)).checked_backing_files().unwrap(),
            [scratch.path("data.qcow2")]
        );
        let other = scratch.write("elsewhere/notes.txt", "not the machine's");
        let other = other.to_str().unwrap();
        qemu_img(&["rebase", "-u", "-f", "qcow2", "-F", "raw", "-b", other, &data]);
        assert!(with(&data, None).checked_backing_files().unwrap_err().0.contains(other));
        assert!(with(&run_data, Some(&data)).checked_backing_files().unwrap_err().0.contains(other));
        qemu_img(&["rebase", "-u", "-f", "qcow2", "-F", "raw", "-b", other, &run_data]);
        assert!(with(&run_data, Some(&data)).checked_backing_files().unwrap_err().0.contains(other));
    }

    #[test]
    fn its_files_are_given_by_the_drive_rules() {
        crate::vm_args::drive_rules::check(&plan(GpuMaker::Amd).qemu_arguments());
    }
}
