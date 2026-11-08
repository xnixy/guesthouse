//! A plain machine holding a Linux the user installed, with the forwarded GPU. Its own line stays as
//! it is, firmware and disk and all, since that's what the guest was installed on. The GPU adds what
//! the Ubuntu route's device needs. Its desktop's frames come from the device when the guest puts it on
//! the GPU, and its keyboard and pointer are QEMU's either way.

use super::arguments::{RELATIVE_MOUSE, words};
use super::{Boot, UbuntuBoot, VmPlan};

/// The memory the device maps for the GPU has to be shareable, as in Ubuntu's line.
const MEMORY_BACKEND: &str = "ram0";
/// OVMF sizes its own 64-bit window and doesn't read QEMU's, so it's told the same 256 GiB.
const OVMF_WINDOW_MIB: u32 = 256 * 1024;

impl VmPlan {
    /// What a Linux the user installed has beyond the plain line: the GPU, or the reason it has none.
    pub(super) fn own_linux_arguments(&self) -> Vec<String> {
        let Boot::OwnLinux { gpu, .. } = &self.boot else { return Vec::new() };
        // For a captured mouse, on the GPU or not. A plain machine that isn't a Linux has none.
        let mut arguments = words(&["-device", RELATIVE_MOUSE]);
        arguments.extend(self.gpu_or_reason(gpu));
        arguments
    }

    fn gpu_or_reason(&self, gpu: &UbuntuBoot) -> Vec<String> {
        match gpu {
            UbuntuBoot::NoGpu { reason_file } => Self::off_reason_arguments(reason_file),
            // Its screen took the plain one's place, and the rest goes here.
            UbuntuBoot::Amd(amd) => Self::amd_guest_arguments(amd),
            UbuntuBoot::WithGpu { image, allow_list, sensor_daemon, card } => {
                let mut arguments = words(&[
                    "-object",
                    &format!("memory-backend-memfd,id={MEMORY_BACKEND},size={},share=on", self.ram),
                    "-machine",
                    &format!("memory-backend={MEMORY_BACKEND}"),
                    "-fw_cfg",
                    &format!("name=opt/ovmf/X-PciMmio64Mb,string={OVMF_WINDOW_MIB}"),
                ]);
                arguments.extend(self.forwarding_arguments(image, allow_list, sensor_daemon, card));
                arguments
            }
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

    fn plan(context: &Context, boot: Boot) -> VmPlan {
        let machine = Machine {
            kind: Kind::Plain,
            linux_gpu: true,
            vram_gib: 12,
            ..Machine::new("fedora", "Fedora", "/m/fedora/disk.qcow2", "/m/fedora/vars.fd")
        };
        VmPlan::new(context, &machine, boot, slots::first(), machine.vram_gib)
    }

    fn with_gpu() -> UbuntuBoot {
        UbuntuBoot::WithGpu {
            image: "/images/driver.iso".into(),
            allow_list: "/p/linux/allow.txt".into(),
            sensor_daemon: "/q/nvfwd-sensord".into(),
            card: ForwardedCard {
                pci: "0000:05:00.0".into(),
                minor: 1,
                render_node: Some("renderD130".into()),
            },
        }
    }

    fn own(gpu: UbuntuBoot) -> Boot {
        Boot::OwnLinux { iso: "/iso/Fedora.iso".into(), gpu }
    }

    #[test]
    fn the_plain_line_is_kept_and_the_gpu_comes_after_it() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let plain = plan(&context, Boot::Plain { iso: "/iso/Fedora.iso".into() }).qemu_arguments();
        let with_gpu = plan(&context, own(with_gpu())).qemu_arguments();
        assert!(with_gpu.len() > plain.len());
        assert_eq!(&with_gpu[..plain.len()], &plain[..], "the guest's own devices stay where it saw them");
        let added = &with_gpu[plain.len()..];
        assert!(added.iter().any(|word| word.starts_with("nvfwd,allow=/p/linux/allow.txt,")));
        assert!(added.iter().any(|word| word.ends_with(",max-vram=12G")));
        assert!(added.iter().any(|word| word == "memory-backend-memfd,id=ram0,size=16G,share=on"));
        assert!(added.iter().any(|word| word == "name=opt/ovmf/X-PciMmio64Mb,string=262144"));
        assert!(added.iter().any(|word| word.contains("file=/images/driver.iso,format=raw,readonly=on")));
        assert!(!added.iter().any(|word| word.contains("forwarded-input")), "its input is QEMU's");
        assert!(added.iter().any(|word| word == RELATIVE_MOUSE), "for a captured mouse");
    }

    #[test]
    fn the_device_is_told_the_chosen_card_as_on_ubuntu() {
        let scratch = Scratch::new();
        let arguments = plan(&scratch_context(&scratch), own(with_gpu())).qemu_arguments();
        let device = arguments.iter().find(|word| word.starts_with("nvfwd,")).unwrap();
        assert!(device.contains(",card=0000:05:00.0,card-minor=1"), "{device}");
    }

    #[test]
    fn with_no_gpu_it_is_the_plain_line_and_the_reason() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let plain = plan(&context, Boot::Plain { iso: "/iso/Fedora.iso".into() }).qemu_arguments();
        let off = plan(&context, own(UbuntuBoot::NoGpu { reason_file: "/r/gpu-off-reason".into() }));
        let arguments = off.qemu_arguments();
        assert_eq!(
            &arguments[plain.len()..],
            ["-device", RELATIVE_MOUSE, "-fw_cfg", "name=opt/nvfwd/gpu-off-reason,file=/r/gpu-off-reason"]
        );
        assert_eq!(
            off.qemu_command()[0],
            context.paths.qemu.display().to_string(),
            "nothing goes before QEMU"
        );
    }

    #[test]
    fn with_the_gpu_it_starts_jailed_in_the_old_address_layout() {
        let scratch = Scratch::new();
        let plan = plan(&scratch_context(&scratch), own(with_gpu()));
        let jail = crate::vm_args::QemuJail {
            launcher: "/p/guesthouse-jail".into(),
            namespaces: true,
            qemu_sandbox: false,
            sound_socket: None,
            backing_files: Vec::new(),
        };
        let plan = VmPlan { jail: Some(jail), ..plan };
        let command = plan.qemu_command();
        assert_eq!(command[0], "bwrap");
        let launcher = command.iter().rposition(|word| word == "/p/guesthouse-jail").unwrap();
        assert_eq!(&command[launcher - 3..launcher], ["setarch", "x86_64", "-L"]);
        let rules = plan.qemu_jail_plan(plan.jail.as_ref().unwrap());
        let read: Vec<_> =
            rules.paths.iter().filter(|(grant, _)| *grant == guesthouse_jail::Grant::Read).collect();
        assert!(read.iter().any(|(_, path)| path == std::path::Path::new("/iso/Fedora.iso")), "its disc");
        assert!(rules.paths.iter().any(|(_, path)| path == std::path::Path::new("/m/fedora/vars.fd")));
    }
}
