//! What a VM starts with, as data: every QEMU argument, and nothing here runs anything.
//!
//! The machine's settings and the state of the PC go in, and [`VmPlan::qemu_command`] gives the words
//! to run. The order of devices is what the guest has already seen, so it must not change.

mod amd;
mod android;
mod arguments;
mod disk_node;
#[cfg(test)]
mod drive_rules;
mod drives;
mod forwarding_jail;
mod macos;
mod nic;
mod own_linux;
mod passt_age;
mod qemu_jail;
mod standalone;
mod throwaway;
mod ubuntu;

pub use amd::{AmdBoot, RENDER_SERVER};
pub use android::{AndroidBoot, AndroidCard, LOGCAT_FILE};
pub use disk_node::names_disk_file;
pub use macos::{MacosBoot, MacosCard, UDMABUF};
pub use nic::{PASST_PID_FILE, only_through_probe_words, passt_probe_words};
pub use passt_age::PasstAge;
pub use qemu_jail::{PASST_FD, PID_FILE, TAP_FD};
pub use standalone::memory_files_in;
pub use throwaway::{THROWAWAY_FDS, THROWAWAY_MARK, throwaway_disk};
pub use ubuntu::{FORWARDED_INPUT_SOCKET, FRAMES_PREFIX, INSTALL_LOG, ssh_port};

use crate::context::Context;
use crate::disk_image::backing::BackingFile;
use crate::firmware::Firmware;
use crate::gpu_identity::{PciIdentity, shown_identity};
use crate::jailing::LAUNCHER;
use crate::linux::card::ForwardedCard;
use crate::machine::{Drive, Kind, Machine};
use crate::network::{Forward, Mode};
use crate::only_through::{OnlyThrough, dns_now};
use guesthouse_daemon::memory_files::MemoryFiles;
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
    /// A plain machine holding macOS, on reims-vgpu's QEMU.
    Macos(MacosBoot),
    /// A plain machine holding Android for VMs, drawing through the virtio GPU on the host's card.
    Android(AndroidBoot),
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

/// The address the machine's first card is given: the machine's own, which an empty one leaves as QEMU's
/// default. An Ubuntu guest installed before its netplan found the card by name has that netplan pinned
/// to the default address, so it only gets the machine's own where its scripts will put that right: in a
/// boot with the driver image, which brings them. Otherwise it keeps the default, and its network.
fn card_address(context: &Context, machine: &Machine, boot: &Boot) -> String {
    let safe = machine.kind != Kind::Linux
        || matches!(boot, Boot::UbuntuInstall(_) | Boot::Ubuntu(UbuntuBoot::WithGpu { .. }))
        || machine.network_by_name_marker(context).is_file();
    if safe { machine.mac.clone() } else { String::new() }
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
    /// The DNS server passt passes the guest's names on to: the host's, or while the guest goes out
    /// through one interface, one that works there. For a passt too old for that, one off the PC that
    /// the guest asks itself.
    pub resolver: Option<String>,
    /// NAT only: the one interface all of the guest's traffic goes through. It takes passt, whatever
    /// `passt` says, and passt must be started by the jail's launcher, which guards its connections.
    pub only_through: Option<OnlyThrough>,
    /// Which of the app's words this PC's passt takes.
    pub passt_age: PasstAge,
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
    /// Where the app reaches the helper for dropped and dragged files, which only Windows has.
    pub files_port: Option<u16>,
    /// Where the app reaches the app windows helper. Only a Windows machine that has app windows
    /// switched on has one: with it off, QEMU gets the same words it always did.
    pub app_windows_port: Option<u16>,
    /// The loopback port QEMU forwards the guest's remoteman port to, for the remote view helper.
    /// Every Windows machine has it from its start, so its remote view can be switched on while it
    /// runs; nothing but this PC can reach it.
    pub remote_view_port: Option<u16>,
    pub cores: u32,
    pub ram: String,
    pub screen: String,
    pub disk: String,
    /// The disk is on an NVMe controller, not the SATA one.
    pub nvme: bool,
    pub vars: String,
    /// The folder of the machine's virtual TPM.
    pub tpm: Option<PathBuf>,
    /// CPUs to run on, as taskset takes them.
    pub pin: Option<String>,
    pub share: Option<String>,
    /// The host's drives the guest has. The app opens each and hands it to QEMU: see
    /// [`crate::host_drives::OpenDrives`].
    pub drives: Vec<Drive>,
    pub share_program: PathBuf,
    /// The jail's launcher, which also starts passt for a machine held to one interface that has no
    /// jail around QEMU.
    pub launcher: PathBuf,
    /// The cap on video memory, as `8G`. The daemon reads it, and QEMU has no use for it.
    pub vram: Option<String>,
    /// A relative USB mouse beside the tablet, for games.
    pub mouse: bool,
    /// What the virtual render device shows Windows it is.
    pub render_identity: PciIdentity,
    pub network: NetworkPlan,
    /// The tools disc from the first hand-built guest, if this store has one. Nothing makes it now.
    pub tools_disc: Option<PathBuf>,
    /// NVIDIA's driver on a disc, for a start that brings the guest's copy of it to this PC's version:
    /// see [`crate::guest_driver_store`]. It goes where the tools disc would.
    pub driver_disc: Option<PathBuf>,
    /// Sound goes to the host's default output, or the sink named, if PipeWire can be reached. QEMU
    /// won't start if it can't, and a VM matters more.
    pub sound: Option<Sound>,
    /// The jail QEMU starts in, if any. GPU machines have one, Windows and the forwarded GPU alike.
    pub jail: Option<QemuJail>,
    /// What the machine's row says about the jail while it runs. Empty for a whole jail, and for a
    /// machine that has none to have.
    pub jail_note: String,
    /// The two files a GPU machine's memory is in, for a QEMU that opens them by path. Without them
    /// it is the app's descriptors, which only a child of the app can have: see [`standalone`].
    pub memory_by_path: Option<MemoryFiles>,
    /// The folder of the drives' names, when QEMU opens each by its path and not from a descriptor
    /// the app opened: see [`standalone`].
    pub drives_by_path: Option<PathBuf>,
    /// For a throwaway machine's run, its own disk, which is only read. `disk` is then the overlay
    /// the app handed QEMU open: see [`crate::throwaway`].
    pub throwaway: Option<String>,
    /// The files the machine's disk is built on, as its settings name them. A start refuses a disk
    /// whose header names others.
    pub backing_files: Vec<BackingFile>,
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
        let drives = Self::drives_for(machine, &boot);
        let tools = context.paths.store.join("iso/tools.iso");
        let nat = machine.network_mode == Mode::Nat;
        let only_through = if nat { OnlyThrough::read(&machine.only_through) } else { None };
        // Held to one interface, a machine never falls back to QEMU's own NAT, which has no such
        // thing. If passt isn't there, the start is refused: see `network_start_problems`.
        let passt = nat && (only_through.is_some() || (machine.passt && passt_installed()));
        // Held to one interface, passt must be the newest, or the start is refused.
        let passt_age = match &only_through {
            None if passt => passt_age_now().unwrap_or_default(),
            _ => PasstAge::default(),
        };
        let resolver = match &only_through {
            Some(through) => dns_now(&machine.dns, &through.interface),
            None if passt && passt_age == PasstAge::NoDnsHost => passt_age::outside_resolver_now(),
            None if passt => host_resolver(),
            None => None,
        };
        let mac = card_address(context, machine, &boot);
        Self {
            boot,
            slot,
            name: machine.id.clone(),
            qemu: context.paths.qemu.clone(),
            qemu_files: Some(context.paths.helper("qemu/pc-bios")).filter(|folder| folder.is_dir()),
            firmware: context.paths.firmware.clone(),
            agent_port: context.paths.agent_port(slot),
            files_port: (machine.kind == crate::machine::Kind::Windows).then(|| slot.files_port()),
            app_windows_port: crate::app_windows::is_on(context, machine)
                .then(|| crate::app_windows::host_port(context, Some(slot))),
            remote_view_port: (machine.kind == crate::machine::Kind::Windows)
                .then(|| slot.remote_view_port()),
            cores: machine.cores,
            ram: format!("{}G", machine.ram_gib),
            // Its device takes any size, where the VGA only has the firmware's.
            screen: if machine.macos() || machine.android() {
                machine.screen.clone()
            } else {
                machine.basic_screen().to_owned()
            },
            disk: machine.disk.clone(),
            nvme: machine.nvme,
            vars: machine.vars.clone(),
            tpm: machine.tpm.then(|| machine.tpm_folder(context)),
            pin: None,
            share: Some(machine.share.clone()).filter(|share| !share.is_empty()),
            drives,
            share_program: context.paths.binary(SHARE_PROGRAM),
            launcher: context.paths.binary(LAUNCHER),
            vram: (vram_gib > 0).then(|| format!("{vram_gib}G")),
            mouse: machine.has_gpu(),
            render_identity: shown_identity(context, machine),
            network: NetworkPlan {
                mode: machine.network_mode,
                passt,
                bridge: machine.bridge.clone(),
                mac,
                forwards: machine.forwards.clone(),
                resolver,
                only_through,
                passt_age,
            },
            tools_disc: tools.is_file().then_some(tools),
            driver_disc: None,
            sound: None,
            jail: None,
            jail_note: String::new(),
            memory_by_path: None,
            drives_by_path: None,
            throwaway: None,
            backing_files: machine.backing_files.clone(),
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

/// A program's first word in a command run with a cleared environment: where the app's own `PATH`
/// finds it. The cleared environment's `PATH` is the one the program is looked for on, and it may not
/// have the folder the program is in. NixOS keeps programs in its store, and the app's package adds
/// their folders to the app's `PATH` only. One that isn't found keeps its name.
pub fn program_word(program: &str) -> String {
    program_on_path(program).map_or_else(|| program.to_owned(), |found| found.display().to_string())
}

fn is_program(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok() && path.is_file()
}

pub fn passt_installed() -> bool {
    on_path("passt")
}

/// Which of the app's words this PC's passt takes, or `None` if it takes none or isn't there.
pub fn passt_age_now() -> Option<PasstAge> {
    passt_age::age_now(passt_probe_words)
}

/// Why a machine on passt can't start on this PC's passt, if it can't. passt missing isn't one: the
/// machine then uses QEMU's own NAT.
pub fn passt_start_problem() -> Option<String> {
    if !passt_installed() {
        return None;
    }
    let age = passt_age_now();
    let outside = (age == Some(PasstAge::NoDnsHost)).then(passt_age::outside_resolver_now).flatten();
    passt_age::start_problem(age, outside.as_deref())
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
