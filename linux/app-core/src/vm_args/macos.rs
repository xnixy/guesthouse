//! A macOS guest's arguments, which have little in common with the others. Its GPU is reims-vgpu's
//! device, which only that project's QEMU has, and the machine line is the one its own tool starts a
//! Mac with. A guest was installed on these devices at these places, so they stay as they are.
//!
//! The device shows its screen on QEMU's console, so the viewer gets it through the D-Bus display as
//! it gets a plain machine's, and the keyboard and pointer are QEMU's.

use super::arguments::{MONITOR_SOCKETS, words};
use super::disk_node::{DiskOptions, disk_node};
use super::forwarding_jail::{DEVICES, gpu_nodes};
use super::{Boot, VmPlan};
use crate::linux::amd::AmdCard;
use crate::linux::card::ForwardedCard;
use crate::text::qemu_value;
use guesthouse_jail::{Grant, Plan};
use std::path::{Path, PathBuf};

/// macOS puts every 64-bit BAR below 4 GiB, so the guest's RAM leaves room for the device's there.
const MACHINE: &str = "q35,memory-backend=ram,dump-guest-core=off,max-ram-below-4g=1G";
/// A CPU macOS knows, whatever the host's is, with what its kernel asks for at boot. KVM keeps a TSC
/// adjust register for every vCPU on any host, and CpuTscSync needs it to put the cores' clocks in step.
const CPU: &str = "Skylake-Client-noTSX-IBRS,kvm=on,vendor=GenuineIntel,+invtsc,+tsc-adjust,vmware-cpuid-freq=on,\
                   +ssse3,+sse4.2,+popcnt,+avx,+aes,+xsave,+xsaveopt,check";
/// The key macOS reads from the system management controller of every Mac.
const SMC: &str = "isa-applesmc,osk=ourhardworkbythesewordsguardedpleasedontsteal(c)AppleComputerInc";
const LOG_VARIABLE: &str = "REIMS_VGPU_LOG";
const LOG_LEVEL: &str = "warn";
const HOME_VARIABLE: &str = "HOME";
/// The guest's memory reaches the GPU through this, whatever the card.
pub const UDMABUF: &str = "/dev/udmabuf";
/// Mesa's and the Vulkan loader's settings for which card to use, which the device's library obeys.
const MESA_CARD_VARIABLE: &str = "MESA_VK_DEVICE_SELECT";
const LOADER_CARD_VARIABLE: &str = "VK_LOADER_DEVICE_SELECT";
/// libdrm finds a card's nodes by listing this folder.
const RENDER_NODES: &str = "dri";
/// NVIDIA's Vulkan driver opens this beside the card's own nodes.
const NVIDIA_MODESET: &str = "nvidia-modeset";
const NVIDIA_VENDOR: &str = "10de";
const AMD_VENDOR: &str = "1002";

/// The card the device draws on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MacosCard {
    /// Its device files, which are all of a GPU's a jailed QEMU may open.
    pub nodes: Vec<PathBuf>,
    /// Its PCI vendor and device numbers, as `10de:2b85`, if sysfs says.
    pub numbers: Option<String>,
}

impl MacosCard {
    /// An NVIDIA card, with its PCI device number if sysfs gave one.
    pub fn nvidia(card: &ForwardedCard, device: Option<u32>) -> Self {
        let devices = Path::new(DEVICES);
        let mut nodes = gpu_nodes(devices, card);
        nodes.extend(Some(devices.join(NVIDIA_MODESET)).filter(|node| node.exists()));
        Self { nodes, numbers: device.map(|device| format!("{NVIDIA_VENDOR}:{device:04x}")) }
    }

    pub fn amd(card: &AmdCard) -> Self {
        Self {
            nodes: vec![card.render_node.clone()],
            numbers: Some(format!("{AMD_VENDOR}:{:04x}", card.device)),
        }
    }
}

/// What a macOS guest boots from beside its disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosBoot {
    /// The OpenCore image, which starts macOS and adds the device's drivers to it.
    pub opencore: PathBuf,
    /// The firmware OpenCore was made for, which isn't the one the other machines use.
    pub firmware_code: PathBuf,
    /// Apple's installer as a disk, or empty once macOS is installed.
    pub installer: String,
    pub rom: PathBuf,
    pub card: MacosCard,
    /// A folder of the machine's that a jailed QEMU has for a home. The device's library and the
    /// card's driver keep compiled shaders under it, and with nowhere to keep them every start
    /// compiles them again.
    pub home: PathBuf,
}

impl VmPlan {
    pub(super) fn macos_boot(&self) -> Option<&MacosBoot> {
        match &self.boot {
            Boot::Macos(boot) => Some(boot),
            _ => None,
        }
    }

    /// The device reaches the guest's memory through the GPU, so the memory has to be shareable.
    fn macos_machine(&self) -> Vec<String> {
        let memory = format!(
            "memory-backend-memfd,id=ram,size={},share=on,prealloc=on,prealloc-threads={}",
            self.ram, self.cores
        );
        words(&[
            "-name",
            &self.name,
            "-machine",
            MACHINE,
            "-object",
            &memory,
            "-accel",
            "kvm",
            "-m",
            &self.ram,
            "-cpu",
            CPU,
            "-smp",
            &format!("{0},sockets=1,cores={0},threads=1", self.cores),
            "-global",
            "ICH9-LPC.acpi-pci-hotplug-with-bridge-support=off",
            "-global",
            "ICH9-LPC.disable_s3=1",
            "-device",
            "pcie-root-port,bus=pcie.0,addr=0x1,slot=1,speed=16,width=32",
            "-device",
            SMC,
        ])
    }

    fn macos_disks(&self, boot: &MacosBoot) -> Vec<String> {
        let mut arguments = words(&[
            "-drive",
            &format!("if=pflash,format=raw,readonly=on,file={}", boot.firmware_code.display()),
            "-drive",
            &format!("if=pflash,format=raw,file={}", qemu_value(&self.vars)),
            "-smbios",
            "type=2",
            "-device",
            "ich9-ahci,id=sata,addr=0x2",
            "-blockdev",
            &disk_node("opencore", &boot.opencore.to_string_lossy(), &[], DiskOptions::default()),
            "-device",
            "ide-hd,bus=sata.2,drive=opencore,bootindex=1",
            "-blockdev",
            &disk_node("disk", &self.disk, &self.disk_chain(), DiskOptions::default()),
            "-device",
            "ide-hd,bus=sata.3,drive=disk,rotation_rate=1",
        ]);
        if !boot.installer.is_empty() {
            // Nothing the installer writes to its own disk is kept.
            arguments.extend(words(&[
                "-drive",
                &format!("id=installer,if=none,format=raw,snapshot=on,file={}", qemu_value(&boot.installer)),
                "-device",
                "ide-hd,bus=sata.4,drive=installer",
            ]));
        }
        arguments
    }

    /// The machine's network, as any machine line has it, on a card macOS has a driver for.
    fn macos_network(&self) -> Vec<String> {
        self.macos_first_nic()
    }

    fn macos_devices(&self, boot: &MacosBoot) -> Vec<String> {
        let (sound, sound_id) = self.sound_backend();
        let screen = format!(
            "reims-graphics-pci,id=gfx,bus=pcie.0,addr=0x4,resolution={},romfile={}",
            self.screen,
            boot.rom.display()
        );
        words(&[
            "-vga",
            "none",
            "-device",
            &screen,
            "-device",
            "qemu-xhci,id=xhci,addr=0x5,conditional-intr-mapping=on,p2=7,p3=7",
            "-device",
            "usb-kbd,id=kbd,bus=xhci.0",
            "-device",
            "usb-tablet,id=tablet,bus=xhci.0",
            "-audiodev",
            &sound,
            "-device",
            &format!("virtio-sound-pci,id=audio,audiodev={sound_id},addr=0x7,streams=1"),
            "-device",
            &format!("virtio-sound-pci,id=audioin,audiodev={sound_id},addr=0x8,streams=1,input-streams=1"),
            "-device",
            "ich9-intel-hda,id=hda,addr=0x1b",
            "-device",
            &format!("hda-output,bus=hda.0,audiodev={sound_id}"),
        ])
    }

    /// With a display of QEMU's the device opens no window of its own, and shows on the console.
    fn macos_outputs(&self) -> Vec<String> {
        let mut arguments = words(&["-display", "dbus,p2p=yes"]);
        for socket in MONITOR_SOCKETS {
            arguments
                .extend(words(&["-qmp", &format!("unix:{},server=on,wait=off", self.in_run_folder(socket))]));
        }
        arguments.extend(words(&[
            "-serial",
            "none",
            "-monitor",
            "none",
            "-D",
            &self.in_run_folder("qemu.log"),
        ]));
        arguments
    }

    /// The whole line of a macOS guest, or nothing for any other.
    pub(super) fn macos_arguments(&self) -> Option<Vec<String>> {
        let boot = self.macos_boot()?;
        let mut arguments = self.macos_machine();
        arguments.extend(self.macos_disks(boot));
        arguments.extend(self.macos_network());
        arguments.extend(self.macos_devices(boot));
        arguments.extend(self.macos_outputs());
        arguments.extend(self.second_nic());
        arguments.extend(self.qemu_sandbox_arguments());
        arguments.extend(self.drive_arguments());
        Some(arguments)
    }

    /// What a macOS guest's QEMU may reach beyond what any jailed QEMU does. The device's library
    /// opens the card itself, so it gets that card's nodes and no other card's.
    pub(super) fn grant_macos(&self, plan: &mut Plan) {
        let Some(boot) = self.macos_boot() else { return };
        // OpenCore keeps its settings in its own image.
        plan.grant(Grant::Write, &boot.opencore).grant(Grant::Read, &boot.rom);
        if !boot.installer.is_empty() {
            plan.grant(Grant::Read, &boot.installer);
        }
        plan.grant(Grant::Own, &boot.home);
        plan.grant(Grant::Device, UDMABUF);
        for node in &boot.card.nodes {
            plan.grant(Grant::Device, node);
            if let Some(folder) = node.parent().filter(|folder| folder.ends_with(RENDER_NODES)) {
                plan.grant(Grant::List, folder);
            }
        }
    }

    /// The device's library says what goes wrong on QEMU's output, where a failed start is read from.
    pub(super) fn macos_environment(&self) -> Vec<(String, String)> {
        let Some(boot) = self.macos_boot() else { return Vec::new() };
        let mut environment = vec![(LOG_VARIABLE.to_owned(), LOG_LEVEL.to_owned())];
        // Outside a jail it has the user's own home, as any program does.
        if self.jail.is_some() {
            environment.push((HOME_VARIABLE.to_owned(), boot.home.display().to_string()));
        }
        if let Some(numbers) = &boot.card.numbers {
            let (vendor, device) = numbers.split_once(':').unwrap_or((numbers, ""));
            environment.push((MESA_CARD_VARIABLE.to_owned(), numbers.clone()));
            environment.push((LOADER_CARD_VARIABLE.to_owned(), format!("0x{vendor}:0x{device}")));
        }
        environment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{Kind, Machine};
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};
    use crate::vm_args::QemuJail;

    fn boot(installer: &str) -> Boot {
        Boot::Macos(MacosBoot {
            opencore: "/m/mac/opencore.qcow2".into(),
            firmware_code: "/m/mac/OVMF_CODE.fd".into(),
            installer: installer.to_owned(),
            rom: "/p/macos/ReimsGraphics.rom".into(),
            card: MacosCard {
                nodes: vec!["/dev/nvidiactl".into(), "/dev/nvidia0".into(), "/dev/dri/renderD129".into()],
                numbers: Some("10de:2b85".into()),
            },
            home: "/m/mac/gpu-cache".into(),
        })
    }

    fn plan(boot: Boot) -> VmPlan {
        let scratch = Scratch::new();
        let machine = Machine {
            kind: Kind::Plain,
            opencore: "/m/mac/opencore.qcow2".into(),
            cores: 6,
            ram_gib: 12,
            screen: "2560x1440".into(),
            mac: "52:54:00:9b:79:fe".into(),
            ..Machine::new("mac", "Mac", "/m/mac/disk.qcow2", "/m/mac/OVMF_VARS.fd")
        };
        VmPlan::new(&scratch_context(&scratch), &machine, boot, slots::first(), 0)
    }

    fn value_after<'a>(arguments: &'a [String], name: &str) -> &'a str {
        let at = arguments.iter().position(|word| word == name).unwrap();
        &arguments[at + 1]
    }

    #[test]
    fn its_memory_is_shareable_and_its_screen_comes_through_the_dbus_display() {
        let arguments = plan(boot("")).qemu_arguments();
        assert_eq!(value_after(&arguments, "-machine"), MACHINE);
        assert_eq!(
            value_after(&arguments, "-object"),
            "memory-backend-memfd,id=ram,size=12G,share=on,prealloc=on,prealloc-threads=6"
        );
        assert_eq!(value_after(&arguments, "-smp"), "6,sockets=1,cores=6,threads=1");
        assert_eq!(value_after(&arguments, "-display"), "dbus,p2p=yes", "or the device opens a window");
        assert_eq!(value_after(&arguments, "-vga"), "none");
        let cpu = value_after(&arguments, "-cpu");
        assert!(cpu.contains(",+tsc-adjust,"), "CpuTscSync needs it, or the cores' clocks drift: {cpu}");
        let device = arguments.iter().find(|word| word.starts_with("reims-graphics-pci,")).unwrap();
        assert!(device.contains(",resolution=2560x1440,romfile=/p/macos/ReimsGraphics.rom"), "{device}");
        assert!(arguments.iter().any(|word| word.ends_with("display.sock,server=on,wait=off")));
    }

    #[test]
    fn it_boots_opencore_on_its_own_firmware_with_the_disk_beside_it() {
        let arguments = plan(boot("")).qemu_arguments();
        let drives: Vec<&String> = arguments.iter().filter(|word| word.contains(",file=")).collect();
        assert_eq!(
            drives,
            [
                "if=pflash,format=raw,readonly=on,file=/m/mac/OVMF_CODE.fd",
                "if=pflash,format=raw,file=/m/mac/OVMF_VARS.fd",
            ]
        );
        let nodes: Vec<&String> =
            arguments.windows(2).filter(|two| two[0] == "-blockdev").map(|two| &two[1]).collect();
        assert!(
            nodes[0].contains(r#""filename":"/m/mac/opencore.qcow2""#)
                && nodes[0].contains(r#""backing":null"#)
        );
        assert!(
            nodes[1].contains(r#""filename":"/m/mac/disk.qcow2""#)
                && nodes[1].contains(r#""node-name":"disk""#)
        );
        assert!(arguments.iter().any(|word| word == "ide-hd,bus=sata.2,drive=opencore,bootindex=1"));
        assert!(!arguments.iter().any(|word| word.contains("installer")), "macOS is installed");
        assert!(value_after(&arguments, "-netdev").starts_with("user,id=net0,ipv6=off,hostfwd="));
        let card = "virtio-net-pci,netdev=net0,id=net0,mac=52:54:00:9b:79:fe,addr=0x3";
        assert!(arguments.iter().any(|word| word == card), "where macOS was installed with it");
    }

    #[test]
    fn an_installer_is_a_disk_that_keeps_nothing_written_to_it() {
        let arguments = plan(boot("/m/mac/installer.img")).qemu_arguments();
        let installer = "id=installer,if=none,format=raw,snapshot=on,file=/m/mac/installer.img";
        assert!(arguments.iter().any(|word| word == installer));
        assert!(arguments.iter().any(|word| word == "ide-hd,bus=sata.4,drive=installer"));
    }

    #[test]
    fn only_a_macos_guest_gets_the_line_and_the_librarys_log() {
        let mac = plan(boot(""));
        let set = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        assert_eq!(
            mac.qemu_environment(),
            [
                set(LOG_VARIABLE, LOG_LEVEL),
                set(MESA_CARD_VARIABLE, "10de:2b85"),
                set(LOADER_CARD_VARIABLE, "0x10de:0x2b85")
            ]
        );
        let plain = plan(Boot::Plain { iso: String::new() });
        assert!(plain.macos_arguments().is_none());
        assert!(plain.qemu_environment().is_empty());
        assert!(!plain.qemu_arguments().iter().any(|word| word.contains("reims")));
    }

    fn jail() -> QemuJail {
        QemuJail {
            launcher: "/p/guesthouse-jail".into(),
            namespaces: true,
            qemu_sandbox: true,
            sound_socket: None,
            backing_files: Vec::new(),
        }
    }

    #[test]
    fn its_jail_has_its_own_files_the_cards_nodes_and_udmabuf() {
        let mac = VmPlan {
            qemu: "/p/macos/qemu/bin/qemu-system-x86_64".into(),
            ..plan(boot("/m/mac/installer.img"))
        };
        let rules = mac.qemu_jail_plan(&jail());
        let has = |grant: Grant, path: &str| rules.paths.contains(&(grant, PathBuf::from(path)));
        assert!(has(Grant::Write, "/m/mac/opencore.qcow2") && has(Grant::Write, "/m/mac/disk.qcow2"));
        assert!(has(Grant::Write, "/m/mac/OVMF_VARS.fd") && has(Grant::Read, "/m/mac/OVMF_CODE.fd"));
        assert!(has(Grant::Read, "/m/mac/installer.img") && has(Grant::Read, "/p/macos/ReimsGraphics.rom"));
        assert!(has(Grant::Run, "/p/macos/qemu"), "its own QEMU's tree");
        assert!(has(Grant::Device, UDMABUF));
        assert!(has(Grant::Own, "/m/mac/gpu-cache"), "somewhere to keep compiled shaders");
        for node in ["/dev/nvidiactl", "/dev/nvidia0", "/dev/dri/renderD129"] {
            assert!(has(Grant::Device, node), "{node}");
        }
        assert!(has(Grant::List, "/dev/dri"));
        let devices: Vec<&PathBuf> =
            rules.paths.iter().filter(|(grant, _)| *grant == Grant::Device).map(|(_, path)| path).collect();
        assert!(!devices.iter().any(|path| path.ends_with("nvidia1")), "no other card's");
    }

    #[test]
    fn a_jailed_one_has_qemus_own_filter_and_starts_behind_the_launcher() {
        let mac = VmPlan { jail: Some(jail()), ..plan(boot("")) };
        let arguments = mac.qemu_arguments();
        assert_eq!(value_after(&arguments, "-sandbox"), "on,obsolete=deny,spawn=allow,resourcecontrol=deny");
        assert!(mac.qemu_command().iter().any(|word| word == "/p/guesthouse-jail"));
        let home = (HOME_VARIABLE.to_owned(), "/m/mac/gpu-cache".to_owned());
        assert!(mac.qemu_environment().contains(&home));
        assert!(!plan(boot("")).qemu_environment().contains(&home), "its user's own, with no jail");
        assert!(!plan(boot("")).qemu_arguments().iter().any(|word| word == "-sandbox"));
    }

    #[test]
    fn held_to_an_interface_its_passt_is_the_launchers_with_no_jail_too() {
        let mut mac = plan(boot(""));
        mac.network.passt = true;
        mac.network.only_through =
            Some(crate::only_through::OnlyThrough { interface: "wg0".into(), ipv6: false });
        let command = mac.qemu_command();
        assert_eq!(command[..2], [mac.launcher.display().to_string(), "--open-only".to_owned()]);
        let guard = command.iter().position(|word| word == "--passt-only-through").unwrap();
        assert_eq!(command[guard + 1], "wg0");
        assert!(value_after(&mac.qemu_arguments(), "-netdev").starts_with("socket,id=net0,fd="));
    }

    #[test]
    fn it_has_the_machines_network_share_sound_and_drives() {
        let mut mac = plan(boot(""));
        mac.network.passt = true;
        mac.share = Some("/home/me/Shared".into());
        mac.sound = Some(crate::vm_args::Sound { sink: None });
        mac.drives = vec![crate::machine::Drive { id: "ata-SSD".into() }];
        let arguments = mac.qemu_arguments();
        assert!(value_after(&arguments, "-netdev").starts_with("passt,id=net0,mtu=1500,"));
        let link = arguments.iter().find(|word| word.starts_with("user,id=link,restrict=on,")).unwrap();
        assert!(link.contains(",guestfwd=tcp:10.0.2.4:80-cmd:"), "the share: {link}");
        assert!(arguments.iter().any(|word| word.starts_with("virtio-net-pci,netdev=link,")));
        assert_eq!(value_after(&arguments, "-audiodev"), "pipewire,id=sound");
        assert!(arguments.iter().any(|word| word == "hda-output,bus=hda.0,audiodev=sound"));
        assert!(
            arguments.iter().any(|word| word == "ide-hd,bus=sata.5,drive=drive1"),
            "after the installer's port"
        );
    }

    #[test]
    fn opencores_disk_is_built_on_nothing() {
        let scratch = Scratch::new();
        let opencore = scratch.path("opencore.qcow2");
        let other = scratch.write("elsewhere/notes.txt", "not the machine's");
        let qemu_img = |words: &[&str]| {
            assert!(std::process::Command::new("qemu-img").args(words).status().unwrap().success());
        };
        qemu_img(&["create", "-q", "-f", "qcow2", opencore.to_str().unwrap(), "64M"]);
        let Boot::Macos(base) = boot("") else { unreachable!() };
        let with =
            VmPlan { boot: Boot::Macos(MacosBoot { opencore: opencore.clone(), ..base }), ..plan(boot("")) };
        assert_eq!(with.checked_backing_files().unwrap(), Vec::<std::path::PathBuf>::new());
        let (other, opencore) = (other.to_str().unwrap(), opencore.to_str().unwrap());
        qemu_img(&["rebase", "-u", "-f", "qcow2", "-F", "raw", "-b", other, opencore]);
        assert!(with.checked_backing_files().unwrap_err().0.contains(other));
    }

    #[test]
    fn its_files_are_given_by_the_drive_rules() {
        crate::vm_args::drive_rules::check(&plan(boot("")).qemu_arguments());
    }
}
