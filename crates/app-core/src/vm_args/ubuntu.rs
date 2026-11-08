//! An Ubuntu guest's arguments, which have little in common with the others. The firmware is SeaBIOS,
//! and the guest's disk and NIC are virtio. Its plain screen always comes through QEMU's D-Bus display,
//! whether or not it gets the GPU.

use super::arguments::{RELATIVE_MOUSE, words};
use super::{Boot, DISPLAY_SOCKET, UbuntuBoot, UbuntuInstall, VmPlan};
use crate::linux::card::ForwardedCard;
use crate::text::qemu_value;
use guesthouse_daemon::slot::Slot;
use std::path::Path;

/// The installer asks for a confirmation unless this is on its command line, and the ISO's own menu
/// doesn't put it there, which is why the kernel is booted directly.
const INSTALL_COMMAND_LINE: &str = "autoinstall ds=nocloud console=tty0 console=ttyS0,115200";
const INSTALL_RAM: &str = "8G";
/// The guest's own SSH port, forwarded to the host for each slot.
const FIRST_SSH_PORT: u16 = 2228;
const GUEST_SSH_PORT: u16 = 22;
const SSH_PORT_VARIABLE: &str = "SSH_PORT";
/// The window for mapping the host's GPU memory into the guest.
const GPU_WINDOW: &str = "256G";
const INPUT_PORT: &str = "nvfwd.input";
/// The guest's reason for having no GPU, where its first boot looks for it.
const OFF_REASON_NAME: &str = "opt/nvfwd/gpu-off-reason";
/// Where the guest's frames are offered, as `show.sock` and `show.display`.
pub const FRAMES_PREFIX: &str = "show";
/// The guest's keyboard and pointer once it's on the GPU. QEMU's input has no way to its session.
pub const FORWARDED_INPUT_SOCKET: &str = "forwarded-input.sock";
pub const INSTALL_LOG: &str = "install-serial.log";
const SERIAL_LOG: &str = "serial.log";

/// The host's port for a guest's SSH: `SSH_PORT`, or the first one for this slot.
pub fn ssh_port(slot: Slot) -> u16 {
    let chosen = std::env::var(SSH_PORT_VARIABLE).ok().and_then(|text| text.parse().ok());
    chosen.unwrap_or(FIRST_SSH_PORT + u16::from(slot.number()))
}

impl VmPlan {
    pub(super) fn ssh_port(&self) -> u16 {
        ssh_port(self.slot)
    }

    fn monitors(&self) -> Vec<String> {
        let sockets = ["qmp.sock", "watch.sock", "input.sock", DISPLAY_SOCKET];
        let each = sockets.iter().flat_map(|socket| {
            words(&["-qmp", &format!("unix:{},server=on,wait=off", self.in_run_folder(socket))])
        });
        each.collect()
    }

    fn install_arguments(&self, install: &UbuntuInstall) -> Vec<String> {
        let mut arguments = self.qemu_files_arguments();
        arguments.extend(words(&[
            "-name",
            &format!("{}-install", self.name),
            "-pidfile",
            &self.in_run_folder("install.pid"),
            "-machine",
            "q35,accel=kvm",
            "-cpu",
            "host",
            "-smp",
            &self.cores.to_string(),
            "-m",
            INSTALL_RAM,
            "-kernel",
            &install.kernel.to_string_lossy(),
            "-initrd",
            &install.initrd.to_string_lossy(),
            "-append",
            INSTALL_COMMAND_LINE,
            "-drive",
            &format!("file={},if=virtio,format=qcow2", qemu_value(&self.disk)),
            "-drive",
            &format!("file={},media=cdrom,readonly=on", qemu_value(&install.iso)),
            "-drive",
            &format!("file={},media=cdrom,readonly=on", qemu_value(&install.seed.to_string_lossy())),
            "-nic",
            "user,model=virtio-net-pci",
            "-display",
            "dbus,p2p=yes",
            "-vga",
            "std",
            "-serial",
            &format!("file:{}", self.in_run_folder(INSTALL_LOG)),
        ]));
        arguments.extend(self.monitors());
        arguments
    }

    fn with_gpu_arguments(&self, boot: &UbuntuBoot) -> Vec<String> {
        match boot {
            UbuntuBoot::NoGpu { reason_file } => Self::off_reason_arguments(reason_file),
            UbuntuBoot::Amd(amd) => Self::amd_guest_arguments(amd),
            UbuntuBoot::WithGpu { image, allow_list, sensor_daemon, card } => {
                let mut arguments = self.forwarding_arguments(image, allow_list, sensor_daemon, card);
                arguments.extend(words(&[
                    "-device",
                    "virtio-serial-pci",
                    "-chardev",
                    &format!(
                        "socket,id=fwdin,path={},server=on,wait=off",
                        self.in_run_folder(FORWARDED_INPUT_SOCKET)
                    ),
                    "-device",
                    &format!("virtserialport,chardev=fwdin,name={INPUT_PORT}"),
                ]));
                arguments
            }
        }
    }

    /// The window, the driver image and the device, which a Linux the user installed has too.
    pub(super) fn forwarding_arguments(
        &self,
        image: &Path,
        allow_list: &Path,
        sensor_daemon: &Path,
        card: &ForwardedCard,
    ) -> Vec<String> {
        words(&[
            "-global",
            &format!("q35-pcihost.pci-hole64-size={GPU_WINDOW}"),
            "-drive",
            &format!(
                "if=none,id=nvdriver,file={},format=raw,readonly=on",
                qemu_value(&image.to_string_lossy())
            ),
            "-device",
            "virtio-blk-pci,drive=nvdriver",
            "-device",
            &format!(
                "nvfwd,allow={},sensord={},log={},trace=off,nogpu=off,show={},card={},card-minor={}{}",
                allow_list.display(),
                sensor_daemon.display(),
                self.in_run_folder("calls.log"),
                self.in_run_folder(FRAMES_PREFIX),
                card.pci,
                card.minor,
                self.vram.as_ref().map_or_else(String::new, |cap| format!(",max-vram={cap}"))
            ),
        ])
    }

    /// The guest's reason for having no GPU, where gpu-up.sh looks for it.
    pub(super) fn off_reason_arguments(reason_file: &Path) -> Vec<String> {
        let file = qemu_value(&reason_file.to_string_lossy());
        words(&["-fw_cfg", &format!("name={OFF_REASON_NAME},file={file}")])
    }

    fn boot_ubuntu_arguments(&self, boot: &UbuntuBoot) -> Vec<String> {
        let ssh_port = self.ssh_port();
        let mut arguments = self.qemu_files_arguments();
        arguments.extend(words(&[
            "-name",
            &self.name,
            "-pidfile",
            &self.in_run_folder("qemu.pid"),
            "-machine",
            "q35,accel=kvm,memory-backend=ram0",
            "-cpu",
            "host",
            "-smp",
            &self.cores.to_string(),
            "-m",
            &self.ram,
            "-object",
            &format!("memory-backend-memfd,id=ram0,size={},share=on", self.ram),
            "-drive",
            &format!("if=none,id=system,file={},format=qcow2", qemu_value(&self.disk)),
            "-device",
            "virtio-blk-pci,drive=system,bootindex=1",
            "-nic",
            &format!("user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{ssh_port}-:{GUEST_SSH_PORT}"),
            "-device",
            "qemu-xhci,id=xhci",
            "-device",
            "usb-tablet",
            // For a captured mouse, which moves it with no position to keep.
            "-device",
            RELATIVE_MOUSE,
        ]));
        arguments.extend(match boot {
            UbuntuBoot::Amd(amd) => Self::amd_screen_arguments(amd, None),
            _ => words(&["-vga", "std", "-display", "dbus,p2p=yes"]),
        });
        arguments.extend(words(&["-serial", &format!("file:{}", self.in_run_folder(SERIAL_LOG))]));
        arguments.extend(self.monitors());
        arguments.extend(self.with_gpu_arguments(boot));
        arguments.extend(self.qemu_sandbox_arguments());
        arguments
    }

    pub(super) fn ubuntu_arguments(&self) -> Option<Vec<String>> {
        match &self.boot {
            Boot::UbuntuInstall(install) => Some(self.install_arguments(install)),
            Boot::Ubuntu(boot) => Some(self.boot_ubuntu_arguments(boot)),
            _ => None,
        }
    }

    /// What goes before QEMU, and before its jail, whose filter keeps QEMU from changing it. The
    /// guest's address space must stay out of the way of the addresses the forwarding maps at the
    /// guest's own values.
    pub(super) fn address_layout(&self) -> Vec<String> {
        match self.boot {
            // Mesa's radeonsi can fall over in QEMU's last moments, and a core of it would hold the
            // guest's memory, so a QEMU on an AMD card leaves none.
            Boot::Ubuntu(UbuntuBoot::Amd(_)) | Boot::OwnLinux { gpu: UbuntuBoot::Amd(_), .. } => {
                words(&["prlimit", "--core=0"])
            }
            Boot::Ubuntu(_) | Boot::OwnLinux { gpu: UbuntuBoot::WithGpu { .. }, .. } => {
                words(&["setarch", "x86_64", "-L"])
            }
            // With no GPU it's a plain machine, which starts as one always has.
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::linux::card::ForwardedCard;
    use crate::machine::{Kind, Machine};
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};
    use std::path::PathBuf;

    fn plan(context: &Context, boot: Boot) -> VmPlan {
        let machine = Machine { kind: Kind::Linux, ..Machine::new("ubuntu", "Ubuntu", "/m/disk.qcow2", "") };
        VmPlan::new(context, &machine, boot, slots::first(), 0)
    }

    fn no_gpu() -> Boot {
        Boot::Ubuntu(UbuntuBoot::NoGpu { reason_file: "/r/gpu-off-reason".into() })
    }

    fn with_gpu() -> Boot {
        on_card("0000:02:00.0", 0)
    }

    fn on_card(pci: &str, minor: u32) -> Boot {
        Boot::Ubuntu(UbuntuBoot::WithGpu {
            image: "/images/driver.iso".into(),
            allow_list: "/p/allow.txt".into(),
            sensor_daemon: "/q/nvfwd-sensord".into(),
            card: ForwardedCard { pci: pci.into(), minor, render_node: None },
        })
    }

    fn value_after(arguments: &[String], flag: &str) -> String {
        let at = arguments.iter().position(|word| word == flag).unwrap();
        arguments[at + 1].clone()
    }

    #[test]
    fn with_no_gpu_there_is_no_device_and_the_reason_reaches_the_guest() {
        let scratch = Scratch::new();
        let arguments = plan(&scratch_context(&scratch), no_gpu()).qemu_arguments();
        assert!(!arguments.iter().any(|word| word.contains("nvfwd,")));
        assert_eq!(
            value_after(&arguments, "-fw_cfg"),
            "name=opt/nvfwd/gpu-off-reason,file=/r/gpu-off-reason"
        );
        assert_eq!(value_after(&arguments, "-display"), "dbus,p2p=yes");
    }

    #[test]
    fn with_the_gpu_it_has_the_device_the_image_and_its_input() {
        let scratch = Scratch::new();
        let arguments = plan(&scratch_context(&scratch), with_gpu()).qemu_arguments();
        assert!(
            arguments
                .iter()
                .any(|word| word.starts_with("nvfwd,allow=/p/allow.txt,sensord=/q/nvfwd-sensord,"))
        );
        assert!(arguments.iter().any(|word| word.contains("file=/images/driver.iso,format=raw,readonly=on")));
        assert!(arguments.iter().any(|word| word.contains("forwarded-input.sock")));
        assert!(!arguments.iter().any(|word| word == "-fw_cfg"));
    }

    #[test]
    fn a_boot_has_the_mouse_capturing_needs_and_the_install_does_not() {
        let scratch = Scratch::new();
        for boot in [no_gpu(), with_gpu()] {
            let arguments = plan(&scratch_context(&scratch), boot).qemu_arguments();
            assert!(arguments.iter().any(|word| word == RELATIVE_MOUSE));
        }
    }

    #[test]
    fn the_device_is_told_the_chosen_card() {
        let scratch = Scratch::new();
        let device = |boot| {
            let arguments = plan(&scratch_context(&scratch), boot).qemu_arguments();
            arguments.into_iter().find(|word| word.starts_with("nvfwd,")).unwrap()
        };
        assert!(device(with_gpu()).contains(",card=0000:02:00.0,card-minor=0"));
        // The tester's PC: the 1660 SUPER is the second device file.
        assert!(device(on_card("0000:05:00.0", 1)).contains(",card=0000:05:00.0,card-minor=1"));
    }

    #[test]
    fn the_system_disk_boots_first_and_the_app_can_find_it() {
        let scratch = Scratch::new();
        let arguments = plan(&scratch_context(&scratch), with_gpu()).qemu_arguments();
        assert!(arguments.iter().any(|word| word == "virtio-blk-pci,drive=system,bootindex=1"));
        assert!(arguments.iter().any(|word| word.contains("file=/m/disk.qcow2,")));
        assert!(arguments.iter().any(|word| word.contains("/run/qmp.sock,")));
    }

    #[test]
    fn it_runs_with_the_old_address_layout() {
        let scratch = Scratch::new();
        let command = plan(&scratch_context(&scratch), no_gpu()).qemu_command();
        assert_eq!(&command[..3], ["setarch", "x86_64", "-L"]);
    }

    #[test]
    fn the_install_boots_the_isos_own_kernel_with_no_prompt() {
        let scratch = Scratch::new();
        let install = UbuntuInstall {
            iso: "/iso/a,b.iso".into(),
            seed: "/m/seed/cidata.iso".into(),
            kernel: PathBuf::from("/m/boot/vmlinuz"),
            initrd: PathBuf::from("/m/boot/initrd"),
        };
        let arguments = plan(&scratch_context(&scratch), Boot::UbuntuInstall(install)).qemu_arguments();
        assert!(value_after(&arguments, "-append").starts_with("autoinstall ds=nocloud"));
        assert_eq!(value_after(&arguments, "-kernel"), "/m/boot/vmlinuz");
        assert_eq!(value_after(&arguments, "-m"), "8G");
        assert!(arguments.iter().any(|word| word == "file=/iso/a,,b.iso,media=cdrom,readonly=on"));
        assert!(arguments.iter().any(|word| word.ends_with("/install-serial.log")));
    }
}
