//! QEMU's arguments, in the order the guest has already seen.

use super::{Boot, DISPLAY_SOCKET, Sound, VmPlan};
use crate::firmware::format_of;
use crate::guest_memory::WINDOW_GIB;
use crate::text::qemu_value;
use guesthouse_daemon::guest_memory::window_base;
use guesthouse_daemon::handover::Part;

const HYPERV_FEATURES: &str = "hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-vpindex,hv-runtime,hv-time,hv-synic,\
                               hv-stimer,hv-stimer-direct,hv-frequencies,hv-reset,hv-ipi,hv-tlbflush";
/// With Secure Boot and a TPM Windows would turn on memory integrity if it could, and that puts its
/// own hypervisor under the guest, between GPU-PV and QEMU. It needs these CPU features.
const NO_NESTED_VIRTUALIZATION: &str = ",-svm,-vmx";
/// The instance our virtual GPU has on the bus.
const VGPU_INSTANCE: &str = "7a1e2f30-4c55-4d6e-9f10-0123456789ab";
/// The guest's graphics kernel looks for this fixed instance on the global channel.
const GLOBAL_INSTANCE: &str = "711dad3a-73ce-468b-90a9-ede6906841b2";
const VMBUS_WINDOW_SIZE: &str = "0x1000000000";
/// Bit 0 makes the guest list the pages the GPU maps instead of using a GPADL.
const HOST_CAPABILITIES: u32 = 1;
const MIB: u64 = 1 << 20;
const BYTES_PER_PIXEL: u64 = 4;
/// The relative mouse beside the tablet, which capturing the mouse moves.
pub(super) const RELATIVE_MOUSE: &str = "usb-mouse,bus=xhci.0";
pub(super) const MONITOR_SOCKETS: [&str; 4] = ["qmp.sock", "watch.sock", "input.sock", DISPLAY_SOCKET];

impl VmPlan {
    /// The firmware only offers modes that fit the VGA memory, and with Windows' highestmode boot
    /// option the largest one wins, so this memory size picks the mode.
    pub(super) fn vga_memory_mib(&self) -> u64 {
        let (width, height) = self.screen_size();
        (width * height * BYTES_PER_PIXEL).div_ceil(MIB).next_power_of_two().max(1)
    }

    fn sound_backend(&self) -> (String, &'static str) {
        match &self.sound {
            None => ("none,id=silent".to_owned(), "silent"),
            Some(Sound { sink: None }) => ("pipewire,id=sound".to_owned(), "sound"),
            Some(Sound { sink: Some(sink) }) => (format!("pipewire,out.name={sink},id=sound"), "sound"),
        }
    }

    fn firmware_drives(&self) -> Vec<String> {
        let code = if self.tpm.is_some() { &self.firmware.code_secure } else { &self.firmware.code };
        let format = format_of(code);
        words(&[
            "-global",
            "driver=cfi.pflash01,property=secure,value=on",
            "-drive",
            &format!("if=pflash,format={format},unit=0,readonly=on,file={}", code.display()),
            "-drive",
            &format!("if=pflash,format={format},unit=1,file={}", qemu_value(&self.vars)),
        ])
    }

    fn common_arguments(&self) -> Vec<String> {
        let mut hyperv = HYPERV_FEATURES.replace(' ', "");
        if self.tpm.is_some() {
            hyperv += NO_NESTED_VIRTUALIZATION;
        }
        let (width, height) = self.screen_size();
        let (sound, sound_id) = self.sound_backend();
        let mut arguments = words(&[
            "-name",
            "gpupv",
            "-machine",
            "q35,accel=kvm,smm=on",
            "-cpu",
            &format!("host,{hyperv}"),
            "-smp",
            &format!("{0},sockets=1,cores={0}", self.cores),
            "-m",
            &self.ram,
        ]);
        arguments.extend(self.firmware_drives());
        arguments.extend(words(&[
            "-rtc",
            "base=utc,driftfix=slew",
            "-global",
            "kvm-pit.lost_tick_policy=discard",
            "-device",
            "ahci,id=ahci",
            "-nic",
            &self.first_nic(),
            "-device",
            "qemu-xhci,id=xhci",
            "-device",
            "usb-tablet",
            "-audiodev",
            &sound,
            "-device",
            "ich9-intel-hda",
            "-device",
            &format!("hda-output,audiodev={sound_id}"),
        ]));
        arguments.extend(self.screen_arguments(width, height));
        if self.passes_unix_sockets() {
            arguments.extend(self.monitors_on_descriptors());
        } else {
            for socket in MONITOR_SOCKETS {
                arguments.extend(words(&[
                    "-qmp",
                    &format!("unix:{},server=on,wait=off", self.in_run_folder(socket)),
                ]));
            }
        }
        arguments.extend(words(&[
            "-serial",
            &format!("file:{}", self.in_run_folder("serial.log")),
            "-D",
            &self.in_run_folder("qemu.log"),
            "-drive",
            &format!(
                "if=none,id=hd0,file={},format=qcow2,cache=unsafe,discard=unmap",
                qemu_value(&self.disk)
            ),
            "-device",
            "ide-hd,bus=ahci.0,drive=hd0,bootindex=1",
        ]));
        arguments
    }

    /// The plain screen, or for a Linux on an AMD card, the GL screen in its place.
    fn screen_arguments(&self, width: u64, height: u64) -> Vec<String> {
        if let Some(amd) = self.amd_boot() {
            return Self::amd_screen_arguments(amd, Some((width, height)));
        }
        words(&[
            "-vga",
            "std",
            "-global",
            &format!("VGA.xres={width}"),
            "-global",
            &format!("VGA.yres={height}"),
            "-global",
            &format!("VGA.vgamem_mb={}", self.vga_memory_mib()),
            "-display",
            "dbus,p2p=yes",
        ])
    }

    fn cdrom(id: &str, file: &str) -> String {
        match file {
            "" => format!("if=none,id={id},media=cdrom,readonly=on"),
            _ => format!("if=none,id={id},media=cdrom,readonly=on,file={}", qemu_value(file)),
        }
    }

    fn gpu_arguments(&self) -> Vec<String> {
        let tools = self.tools_disc.as_ref().map_or_else(String::new, |path| path.display().to_string());
        words(&[
            "-drive",
            &Self::cdrom("cd1", &tools),
            "-device",
            "ide-cd,bus=ahci.2,drive=cd1",
            "-device",
            &format!("vmbus-bridge,mmio-base={:#x},mmio-size={VMBUS_WINDOW_SIZE}", window_base()),
            // Windows keeps its clock on the host's through this. OpenGL needs the two close.
            "-device",
            "hv-timesync",
            "-chardev",
            &self.client_socket("gpg", "global.sock"),
            // The app's memfds, which the daemon has too: see `guesthouse_daemon::handover`.
            "-object",
            &format!(
                "memory-backend-file,id=gpio,size={WINDOW_GIB}G,mem-path={},share=on",
                Part::Window.path_for_qemu().display()
            ),
            "-device",
            &format!(
                "gpupv-global,chardev=gpg,instanceid={GLOBAL_INSTANCE},iomem=gpio,iomem-base={:#x}",
                window_base()
            ),
            "-chardev",
            &self.client_socket("gpv", "vgpu.sock"),
            "-device",
            &format!("gpupv-vgpu,chardev=gpv,instanceid={VGPU_INSTANCE}"),
            // Shared, because the GPU maps some of the guest's own pages.
            "-object",
            &format!(
                "memory-backend-file,id=ram0,size={},mem-path={},share=on",
                self.ram,
                Part::Ram.path_for_qemu().display()
            ),
            "-machine",
            "memory-backend=ram0",
            "-device",
            &format!(
                "gpupv-render,channel={VGPU_INSTANCE},hostcaps={HOST_CAPABILITIES}{}",
                self.render_identity.qemu_properties()
            ),
        ])
    }

    fn boot_arguments(&self) -> Vec<String> {
        match &self.boot {
            Boot::Plain { iso } | Boot::OwnLinux { iso, .. } => words(&[
                "-drive",
                &Self::cdrom("cd0", iso),
                "-device",
                "ide-cd,bus=ahci.1,drive=cd0,bootindex=0",
            ]),
            Boot::Install { iso, answer } => words(&[
                "-drive",
                &Self::cdrom("cd0", iso),
                "-device",
                "ide-cd,bus=ahci.1,drive=cd0,bootindex=0",
                "-drive",
                &Self::cdrom("cd1", &answer.to_string_lossy()),
                "-device",
                "ide-cd,bus=ahci.2,drive=cd1",
            ]),
            Boot::Gpu => self.gpu_arguments(),
            Boot::UbuntuInstall(_) | Boot::Ubuntu(_) => Vec::new(),
        }
    }

    /// The program's arguments.
    pub(super) fn qemu_files_arguments(&self) -> Vec<String> {
        self.qemu_files.iter().flat_map(|folder| words(&["-L", &folder.to_string_lossy()])).collect()
    }

    pub fn qemu_arguments(&self) -> Vec<String> {
        if let Some(arguments) = self.ubuntu_arguments() {
            return arguments;
        }
        let mut arguments = self.qemu_files_arguments();
        arguments.extend(self.common_arguments());
        arguments.extend(self.boot_arguments());
        if self.mouse {
            arguments.extend(words(&["-device", RELATIVE_MOUSE]));
        }
        if self.tpm.is_some() {
            arguments.extend(words(&[
                "-chardev",
                &self.client_socket("tpm", "tpm.sock"),
                "-tpmdev",
                "emulator,id=tpm0,chardev=tpm",
                "-device",
                "tpm-crb,tpmdev=tpm0",
            ]));
        }
        arguments.extend(self.second_nic());
        arguments.extend(self.qemu_sandbox_arguments());
        // Last, so the devices the guest was installed with keep their places.
        arguments.extend(self.own_linux_arguments());
        arguments
    }

    /// The words to run, with `taskset` first if the VM is pinned.
    pub fn qemu_command(&self) -> Vec<String> {
        let qemu = self.qemu.to_string_lossy().into_owned();
        let mut command = self.jail_words(&self.address_layout());
        if let Some(cpus) = &self.pin {
            command.extend(words(&["taskset", "-c", cpus]));
        }
        command.push(qemu);
        command.extend(self.qemu_arguments());
        command
    }

    /// What QEMU needs in its environment: the share's program finds its folder there. A jailed QEMU
    /// gets this and no more, and the rest of the app's is cleared: see [`Self::clears_environment`].
    pub fn qemu_environment(&self) -> Vec<(String, String)> {
        let mut environment =
            self.jail.as_ref().map(|jail| self.jailed_environment(jail)).unwrap_or_default();
        environment.extend(self.share.iter().map(|folder| ("SHARE".to_owned(), folder.clone())));
        environment.extend(self.amd_environment());
        environment
    }

    pub fn clears_environment(&self) -> bool {
        self.jail.is_some()
    }

    /// The virtual TPM's process. It ends when QEMU lets go of it, so nothing has to stop it.
    pub fn swtpm_arguments(&self) -> Option<Vec<String>> {
        let state = self.tpm.as_ref()?;
        Some(words(&[
            "socket",
            "--tpm2",
            "--tpmstate",
            &format!("dir={}", state.display()),
            "--ctrl",
            &format!("type=unixio,path={}", self.in_run_folder("tpm.sock")),
            "--terminate",
            "--daemon",
            "--log",
            &format!("file={}", self.in_run_folder("swtpm.log")),
        ]))
    }
}

pub(super) fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|word| (*word).to_owned()).collect()
}
