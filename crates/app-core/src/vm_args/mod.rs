//! What a VM starts with, as data: every QEMU argument, and nothing here runs anything.
//!
//! The machine's settings and the state of the PC go in, and [`VmPlan::qemu_command`] gives the words
//! to run. The order of devices is what the guest has already seen, so it must not change.

mod amd;
mod arguments;
mod forwarding_jail;
mod nic;
mod own_linux;
mod qemu_jail;
mod ubuntu;

pub use amd::{AmdBoot, RENDER_SERVER};
pub use nic::PASST_PID_FILE;
pub use qemu_jail::{PASST_FD, PID_FILE, TAP_FD};
pub use ubuntu::{FORWARDED_INPUT_SOCKET, FRAMES_PREFIX, INSTALL_LOG, ssh_port};

use crate::context::Context;
use crate::firmware::Firmware;
use crate::gpu_identity::{PciIdentity, shown_identity};
use crate::linux::card::ForwardedCard;
use crate::machine::Machine;
use crate::network::{Forward, Mode};
use guesthouse_daemon::slot::Slot;
use std::fs;
use std::path::{Path, PathBuf};

/// QEMU's monitor for the viewer, which takes the socket its D-Bus display listens on.
pub const DISPLAY_SOCKET: &str = "display.sock";
pub const SHARE_PROGRAM: &str = "guesthouse-share";
/// The file QEMU's output goes to, which holds why it failed to start.
pub const OUTPUT_FILE: &str = "vm-gpupv.out";
/// Where a guest reaches a shared folder.
pub const SHARE_ADDRESS: &str = "10.0.2.4";

const RESOLVER_FILE: &str = "/etc/resolv.conf";

/// How a VM is to boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boot {
    /// A machine with no GPU: its disk, and its ISO first if it has one. The drive is there either
    /// way, as a tray with nothing in it, so the ISO can be ejected while the VM runs.
    Plain { iso: String },
    /// Windows from its disc, with the answer disc beside it.
    Install { iso: String, answer: PathBuf },
    /// With the GPU, which needs the daemon up first.
    Gpu,
    /// Ubuntu's installer, from the ISO's own kernel.
    UbuntuInstall(UbuntuInstall),
    /// An installed Ubuntu guest.
    Ubuntu(UbuntuBoot),
    /// A plain machine holding a Linux the user installed, with the forwarded GPU if it can have it.
    OwnLinux { iso: String, gpu: UbuntuBoot },
}

/// What Ubuntu's unattended install starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UbuntuInstall {
    pub iso: String,
    pub seed: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
}

/// An Ubuntu guest, or a Linux the user installed, gets the forwarded GPU, an AMD card, or the reason it
/// has no GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UbuntuBoot {
    WithGpu {
        image: PathBuf,
        allow_list: PathBuf,
        sensor_daemon: PathBuf,
        card: ForwardedCard,
    },
    /// A file holding the reason, which reaches the guest so that it can say why.
    NoGpu {
        reason_file: PathBuf,
    },
    Amd(AmdBoot),
}

/// What the first NIC is, and what the private link carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPlan {
    pub mode: Mode,
    /// NAT through passt, rather than QEMU's own.
    pub passt: bool,
    pub bridge: String,
    pub mac: String,
    pub forwards: Vec<Forward>,
    /// The host's DNS server, which passt passes on.
    pub resolver: Option<String>,
}

/// Everything QEMU's arguments are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPlan {
    pub boot: Boot,
    pub slot: Slot,
    /// The machine's id, which an Ubuntu guest's QEMU goes by.
    pub name: String,
    pub qemu: PathBuf,
    /// QEMU's own files, if it isn't the system's, which knows where its files are.
    pub qemu_files: Option<PathBuf>,
    pub firmware: Firmware,
    pub run_folder: PathBuf,
    pub agent_port: u16,
    pub cores: u32,
    pub ram: String,
    pub screen: String,
    pub disk: String,
    pub vars: String,
    /// The folder of the machine's virtual TPM.
    pub tpm: Option<PathBuf>,
    /// CPUs to run on, as taskset takes them.
    pub pin: Option<String>,
    pub share: Option<String>,
    pub share_program: PathBuf,
    /// The cap on video memory, as `8G`. The daemon reads it, and QEMU has no use for it.
    pub vram: Option<String>,
    /// A relative USB mouse beside the tablet, for games.
    pub mouse: bool,
    /// What the virtual render device shows Windows it is.
    pub render_identity: PciIdentity,
    pub network: NetworkPlan,
    pub ram_file: PathBuf,
    pub io_file: PathBuf,
    /// The tools disc from the first hand-built guest, if this store has one. Nothing makes it now.
    pub tools_disc: Option<PathBuf>,
    /// Sound goes to the host's default output, or the sink named, if PipeWire can be reached. QEMU
    /// won't start if it can't, and a VM matters more.
    pub sound: Option<Sound>,
    /// The jail QEMU starts in, if any. GPU machines have one, Windows and the forwarded GPU alike.
    pub jail: Option<QemuJail>,
    /// What the machine's row says about the jail while it runs. Empty for a whole jail, and for a
    /// machine that has none to have.
    pub jail_note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sound {
    pub sink: Option<String>,
}

/// How a GPU machine's QEMU is jailed. The host decides this at each start, from what the PC allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QemuJail {
    /// `guesthouse-jail`.
    pub launcher: PathBuf,
    /// In bubblewrap's namespaces as well.
    pub namespaces: bool,
    /// QEMU's own seccomp filter on top, which a QEMU only has if it was built with it.
    pub qemu_sandbox: bool,
    /// PipeWire's socket, for a machine with sound.
    pub sound_socket: Option<PathBuf>,
    /// The files the disk is built on, which QEMU may only read.
    pub backing_files: Vec<PathBuf>,
}

impl VmPlan {
    /// The plan for this machine in this slot. `vram_gib` is the cap to give the daemon, if any.
    pub fn new(context: &Context, machine: &Machine, boot: Boot, slot: Slot, vram_gib: u32) -> Self {
        let run_folder = context.paths.run_folder(slot);
        let tools = context.paths.store.join("iso/tools.iso");
        let passt = machine.network_mode == Mode::Nat && machine.passt && passt_installed();
        Self {
            boot,
            slot,
            name: machine.id.clone(),
            qemu: context.paths.qemu.clone(),
            qemu_files: Some(context.paths.helper("qemu/pc-bios")).filter(|folder| folder.is_dir()),
            firmware: context.paths.firmware.clone(),
            agent_port: context.paths.agent_port(slot),
            cores: machine.cores,
            ram: format!("{}G", machine.ram_gib),
            screen: machine.basic_screen().to_owned(),
            disk: machine.disk.clone(),
            vars: machine.vars.clone(),
            tpm: machine.tpm.then(|| machine.tpm_folder(context)),
            pin: None,
            share: Some(machine.share.clone()).filter(|share| !share.is_empty()),
            share_program: context.paths.binary(SHARE_PROGRAM),
            vram: (vram_gib > 0).then(|| format!("{vram_gib}G")),
            mouse: machine.has_gpu(),
            render_identity: shown_identity(context, machine),
            network: NetworkPlan {
                mode: machine.network_mode,
                passt,
                bridge: machine.bridge.clone(),
                mac: machine.mac.clone(),
                forwards: machine.forwards.clone(),
                resolver: if passt { host_resolver() } else { None },
            },
            ram_file: context.paths.shared_file("ram", slot),
            io_file: context.paths.shared_file("iomem", slot),
            tools_disc: tools.is_file().then_some(tools),
            sound: None,
            jail: None,
            jail_note: String::new(),
            run_folder,
        }
    }

    pub fn is_gpu(&self) -> bool {
        self.boot == Boot::Gpu
    }

    fn in_run_folder(&self, name: &str) -> String {
        self.run_folder.join(name).display().to_string()
    }

    fn screen_size(&self) -> (u64, u64) {
        let (width, height) = self.screen.split_once('x').unwrap_or_default();
        (width.parse().unwrap_or(0), height.parse().unwrap_or(0))
    }
}

/// Whether a program is on `PATH`.
pub fn on_path(program: &str) -> bool {
    program_on_path(program).is_some()
}

/// Where `PATH` finds a program.
pub fn program_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|folder| folder.join(program)).find(|found| is_program(found))
}

fn is_program(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok() && path.is_file()
}

pub fn passt_installed() -> bool {
    on_path("passt")
}

fn host_resolver() -> Option<String> {
    let text = fs::read_to_string(RESOLVER_FILE).ok()?;
    text.lines().find_map(|line| {
        let address = line.strip_prefix("nameserver ")?.trim();
        address.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.').then(|| address.to_owned())
    })
}

#[cfg(test)]
mod tests;
