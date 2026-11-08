//! The jail around a GPU machine's QEMU, for Windows and for the forwarded GPU alike: what
//! `guesthouse-jail` lets it reach, and bubblewrap's namespaces around that where the PC allows them.
//!
//! The plan names this VM's files one by one, so the machine's folder, with its token and settings,
//! stays out, and so does every other VM. What the network may do follows the machine's mode: NAT
//! is the internet for the guest, so only binds are held to the forwarded ports.

use super::arguments::{MONITOR_SOCKETS, words};
use super::forwarding_jail::DEVICES as CARD_NODES;
use super::{Boot, QemuJail, VmPlan};
use crate::disk_image::backing::backing_files;
use crate::failure::{Failure, Result};
use crate::network::{Mode, Protocol, bridge_helper};
use guesthouse_jail::launch::{Launch, Passt, Tap, UnixSocket};
use guesthouse_jail::{Grant, Network, Plan, Ports};
use std::path::{Path, PathBuf};

/// Where the launcher leaves QEMU's unix sockets, when it opens them: monitors first, then clients.
const FIRST_UNIX_FD: i32 = 20;
const CLIENT_SOCKETS: [(&str, &str); 3] = [("gpg", "global.sock"), ("gpv", "vgpu.sock"), ("tpm", "tpm.sock")];
const RECONNECT_MS: u32 = 500;
/// QEMU's pid file, which names the run folder in its words when no monitor socket does.
pub const PID_FILE: &str = "qemu.pid";

/// Where a bridged network's device is open for QEMU. Out of the way of what QEMU opens itself.
pub const TAP_FD: i32 = 10;
/// Where QEMU's end of its connection to passt is open.
pub const PASST_FD: i32 = 11;
const PASST: &str = "passt";
const DNS_PORT: u16 = 53;
/// Any free port, which sending a datagram binds first.
const EPHEMERAL_PORT: u16 = 0;
/// QEMU's own filter, on top. Starting programs stays allowed, for the shared folder's.
/// Its privileges group stays out: it ends QEMU's user network's child at `setsid`, before the
/// shared folder's program runs, and no new privileges, which the launcher sets, already covers it.
const QEMU_SANDBOX: &str = "on,obsolete=deny,spawn=allow,resourcecontrol=deny";
/// The same with no programs started, for a QEMU that has none to start: an AMD card's on native
/// context, with no shared folder.
const QEMU_SANDBOX_NO_SPAWN: &str = "on,obsolete=deny,spawn=deny,resourcecontrol=deny";

/// What any QEMU reads. `/proc` and `/sys` give nothing away, as Landlock doesn't let a jailed
/// process look into one outside its jail.
const SYSTEM: [(Grant, &str); 11] = [
    (Grant::Run, "/usr"),
    (Grant::Read, "/etc/ld.so.cache"),
    (Grant::Read, "/etc/nsswitch.conf"),
    (Grant::Read, "/etc/passwd"),
    (Grant::Read, "/etc/group"),
    (Grant::Read, "/etc/hosts"),
    (Grant::Read, "/etc/resolv.conf"),
    (Grant::Read, "/etc/qemu"),
    (Grant::Read, "/run/systemd/resolve"),
    (Grant::Read, "/sys"),
    (Grant::Read, "/proc"),
];
const DEVICES: [&str; 5] = ["/dev/kvm", "/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"];
/// What bubblewrap's namespaces start from, before this VM's own files go in. The network stays, for
/// QEMU's user network. `--new-session` is left out so the app can still stop QEMU with its process
/// group, and the filter refuses typing into a terminal instead. The pid namespace is left out too:
/// in one, the viewer's pid reads as 0, and QEMU's D-Bus display then refuses to let it in. Landlock
/// still keeps QEMU from tracing what's outside its jail, and from signalling it where it has scopes.
const NAMESPACES: &str = "bwrap --unshare-user --unshare-ipc --unshare-uts --unshare-cgroup-try \
    --tmpfs /tmp --ro-bind /usr /usr --symlink usr/lib /lib --symlink usr/lib64 /lib64 --symlink usr/bin /bin \
    --symlink usr/sbin /sbin --proc /proc --dev /dev --ro-bind /sys /sys";
/// Already in the namespaces' base.
const IN_THE_BASE: [&str; 3] = ["/usr", "/proc", "/sys"];
const PATH: &str = "/usr/bin:/usr/sbin";

/// The folder above the one QEMU is in: a QEMU in its build folder finds its firmware and libraries
/// through links to there, and for the system's it's `/usr`.
fn qemu_tree(qemu: &Path) -> PathBuf {
    qemu.parent().and_then(Path::parent).unwrap_or(qemu).to_owned()
}

impl VmPlan {
    fn forwarded_ports(&self, protocol: Protocol) -> Vec<u16> {
        let forwards = self.network.forwards.iter().filter(|forward| forward.protocol == protocol);
        forwards.filter_map(|forward| u16::try_from(forward.host).ok()).collect()
    }

    /// Ubuntu's line is its own: SeaBIOS from QEMU's files, NAT with its SSH forward, and none of the
    /// machine's network or shared folder settings. Every other line has those, and boots OVMF, which
    /// keeps its settings in a file of the machine's.
    fn has_machine_line(&self) -> bool {
        !matches!(self.boot, Boot::Ubuntu(_) | Boot::UbuntuInstall(_))
    }

    /// What QEMU's user network listens on: the agent and the forwards, or for Ubuntu its SSH.
    fn listened_ports(&self) -> Vec<u16> {
        if !self.has_machine_line() {
            return vec![self.ssh_port()];
        }
        let mut ports = vec![self.agent_port];
        ports.extend(self.forwarded_ports(Protocol::Tcp));
        ports
    }

    /// passt does the NAT for a machine on it, outside the jail, so QEMU's own network is the
    /// restricted private link, as for a bridged or restricted machine. Ubuntu's line is NAT.
    fn jail_network(&self) -> Network {
        let nat = !self.has_machine_line() || (self.network.mode == Mode::Nat && !self.network.passt);
        let mut tcp_bind = self.listened_ports();
        // The user network reaches the shared folder's program through a pair of loopback TCP
        // sockets it makes for each connection, on a port it can't know beforehand.
        if self.shared_folder().is_some() {
            tcp_bind.push(EPHEMERAL_PORT);
        }
        let tcp_bind = Ports::Only(tcp_bind);
        if nat {
            return Network { tcp_bind, tcp_connect: Ports::Any, udp_bind: Ports::Any, udp_send: Ports::Any };
        }
        // A restricted user network only answers forwards, and asks the host's resolver for names.
        let mut udp_bind = vec![EPHEMERAL_PORT];
        udp_bind.extend(self.forwarded_ports(Protocol::Udp));
        let tcp_connect = if self.shared_folder().is_some() { Ports::Any } else { Ports::none() };
        Network {
            tcp_bind,
            tcp_connect,
            udp_bind: Ports::Only(udp_bind),
            udp_send: Ports::Only(vec![DNS_PORT]),
        }
    }

    fn firmware_code(&self) -> &Path {
        if self.tpm.is_some() { &self.firmware.code_secure } else { &self.firmware.code }
    }

    fn shared_folder(&self) -> Option<&String> {
        self.share.as_ref().filter(|_| self.has_machine_line())
    }

    /// Every path QEMU may reach, and its network.
    pub fn qemu_jail_plan(&self, jail: &QemuJail) -> Plan {
        self.qemu_jail_plan_under(jail, Path::new(CARD_NODES))
    }

    /// The same, with the card's nodes looked for under `devices`.
    pub(super) fn qemu_jail_plan_under(&self, jail: &QemuJail, devices: &Path) -> Plan {
        let mut plan = Plan { network: self.jail_network(), ..Plan::default() };
        for (grant, path) in SYSTEM {
            plan.grant(grant, path);
        }
        plan.grant(Grant::Run, qemu_tree(&self.qemu));
        if let Some(folder) = &self.qemu_files {
            plan.grant(Grant::Read, folder);
        }
        if self.has_machine_line() {
            plan.grant(Grant::Read, self.firmware_code()).grant(Grant::Write, &self.vars);
        }
        // A disc chosen while it runs is outside the jail, and the guest sees an empty drive.
        if let Boot::Plain { iso } | Boot::OwnLinux { iso, .. } = &self.boot
            && !iso.is_empty()
        {
            plan.grant(Grant::Read, iso);
        }
        for device in DEVICES {
            plan.grant(Grant::Device, device);
        }
        plan.grant(Grant::Write, &self.disk);
        if self.is_gpu() {
            if let Some(disc) = &self.tools_disc {
                plan.grant(Grant::Read, disc);
            }
            plan.grant(Grant::Write, &self.ram_file).grant(Grant::Write, &self.io_file);
        }
        self.grant_forwarding(&mut plan, devices);
        self.grant_amd(&mut plan);
        if let Some(folder) = self.shared_folder() {
            plan.grant(Grant::Run, &self.share_program).grant(Grant::Own, folder);
        }
        if let Some(socket) = &jail.sound_socket {
            plan.grant(Grant::Socket, socket);
        }
        plan.grant(Grant::Own, &self.run_folder);
        // Last, so in namespaces each is bound read-only over anything granted above.
        for file in &jail.backing_files {
            plan.grant(Grant::Read, file);
        }
        plan
    }

    /// The jail with the files the disk is built on. Refuses a chain QEMU couldn't open, and a
    /// backing file in the shared folder, where QEMU may write anything.
    pub fn with_backing_files(&self, jail: QemuJail) -> Result<QemuJail> {
        let backing_files = backing_files(Path::new(&self.disk))?;
        let shared = |file: &&PathBuf| self.shared_folder().is_some_and(|folder| file.starts_with(folder));
        if let Some(file) = backing_files.iter().find(shared) {
            return Err(Failure(format!(
                "The VM did not start, as its disk is built on {}, which is in the shared folder. \
                 Move it out, as the guest can write anything in there.",
                file.display()
            )));
        }
        Ok(QemuJail { backing_files, ..jail })
    }

    fn passt(&self) -> Option<Passt> {
        let wanted = self.has_machine_line() && self.network.mode == Mode::Nat && self.network.passt;
        wanted.then(|| Passt {
            program: super::program_on_path(PASST).unwrap_or_else(|| PASST.into()),
            fd: PASST_FD,
            pid_file: self.run_folder.join(super::PASST_PID_FILE),
            words: self.passt_words(),
        })
    }

    fn tap(&self) -> Option<Tap> {
        (self.has_machine_line() && self.network.mode == Mode::Bridged).then(|| Tap {
            helper: bridge_helper().into(),
            bridge: self.network.bridge.clone(),
            fd: TAP_FD,
        })
    }

    /// Whether this QEMU runs in bubblewrap's namespaces as well.
    pub fn in_namespaces(&self, jail: &QemuJail) -> bool {
        jail.namespaces
    }

    /// bubblewrap's words, with each path of the plan bound at its own place, so the plan means the
    /// same inside.
    fn namespace_words(plan: &Plan, launcher: &Path) -> Vec<String> {
        let mut words: Vec<String> = NAMESPACES.split_whitespace().map(str::to_owned).collect();
        let bind = |kind: &str, path: &Path| {
            [kind.to_owned(), path.display().to_string(), path.display().to_string()]
        };
        words.extend(bind("--ro-bind", launcher));
        for (grant, path) in &plan.paths {
            if IN_THE_BASE.iter().any(|base| path.starts_with(base)) {
                continue;
            }
            let kind = match grant {
                Grant::Read | Grant::Run => "--ro-bind-try",
                Grant::Write | Grant::Own | Grant::Socket => "--bind-try",
                Grant::Device => "--dev-bind-try",
            };
            words.extend(bind(kind, path));
        }
        words.push("--".to_owned());
        words
    }

    /// What goes in front of QEMU for a jailed GPU machine, up to the program's own words, with
    /// `before_jail` right before the launcher that jails it. In namespaces, passt and a bridged
    /// network's helper go before bubblewrap: passt has a sandbox of its own, and the helper has to
    /// gain its privileges, which it can't inside. So the launcher runs twice, once to start those
    /// and once inside to jail QEMU.
    pub(super) fn jail_words(&self, before_jail: &[String]) -> Vec<String> {
        let Some(jail) = &self.jail else { return before_jail.to_vec() };
        let plan = self.qemu_jail_plan(jail);
        let mut jailing = Launch {
            plan: Some(plan.clone()),
            passt: self.passt(),
            tap: self.tap(),
            unix_sockets: self.unix_sockets(),
            no_new_unix_sockets: self.passes_unix_sockets(),
            program: Vec::new(),
        };
        let launcher = jail.launcher.display().to_string();
        let mut words = Vec::new();
        if self.in_namespaces(jail) {
            if jailing.passt.is_some() || jailing.tap.is_some() {
                let opening = Launch {
                    plan: None,
                    passt: jailing.passt.take(),
                    tap: jailing.tap.take(),
                    unix_sockets: Vec::new(),
                    no_new_unix_sockets: false,
                    program: Vec::new(),
                };
                words.push(launcher.clone());
                words.extend(opening.words());
            }
            words.extend(Self::namespace_words(&plan, &jail.launcher));
        }
        words.extend(before_jail.iter().cloned());
        words.push(launcher);
        words.extend(jailing.words());
        words
    }

    /// The launcher opens every unix socket QEMU needs, and QEMU may make none, so a QEMU that's been
    /// taken over can't reach the session bus or any other socket of the desktop's, whatever the
    /// kernel's Landlock and with or without namespaces. Only with no sound, as PipeWire's client
    /// makes its own socket, and only for Windows: the forwarded GPU's device makes the socket its
    /// frames are offered on.
    pub(super) fn passes_unix_sockets(&self) -> bool {
        self.jail.as_ref().is_some() && self.can_pass_unix_sockets()
    }

    /// Whether a jail would hand this QEMU its unix sockets.
    pub fn can_pass_unix_sockets(&self) -> bool {
        self.sound.is_none() && self.is_gpu()
    }

    fn client_names(&self) -> impl Iterator<Item = (usize, &'static str, &'static str)> + '_ {
        let tpm = self.tpm.is_some();
        CLIENT_SOCKETS
            .into_iter()
            .enumerate()
            .filter(move |(_, (id, _))| *id != "tpm" || tpm)
            .map(|(at, (id, file))| (at, id, file))
    }

    fn unix_sockets(&self) -> Vec<UnixSocket> {
        if !self.passes_unix_sockets() {
            return Vec::new();
        }
        let monitors = MONITOR_SOCKETS.iter().enumerate().map(|(at, name)| UnixSocket {
            path: self.run_folder.join(name),
            fd: FIRST_UNIX_FD + at as i32,
            listen: true,
        });
        let mut sockets: Vec<UnixSocket> = monitors.collect();
        let first_client = FIRST_UNIX_FD + MONITOR_SOCKETS.len() as i32;
        sockets.extend(self.client_names().map(|(at, _, file)| UnixSocket {
            path: self.run_folder.join(file),
            fd: first_client + at as i32,
            listen: false,
        }));
        sockets
    }

    /// The monitors on the descriptors the launcher opened, and a pid file that names the run folder.
    pub(super) fn monitors_on_descriptors(&self) -> Vec<String> {
        let mut arguments = Vec::new();
        for at in 0..MONITOR_SOCKETS.len() {
            let fd = FIRST_UNIX_FD + at as i32;
            arguments.extend(words(&[
                "-chardev",
                &format!("socket,id=monitor{at},fd={fd},server=on,wait=off"),
                "-mon",
                &format!("chardev=monitor{at},mode=control"),
            ]));
        }
        arguments.extend(words(&["-pidfile", &self.run_folder.join(PID_FILE).display().to_string()]));
        arguments
    }

    /// A socket QEMU connects to: on the launcher's descriptor, or by its path.
    pub(super) fn client_socket(&self, id: &str, file: &str) -> String {
        let first_client = FIRST_UNIX_FD + MONITOR_SOCKETS.len() as i32;
        let on_fd = self.client_names().find(|(_, name, _)| *name == id);
        match on_fd {
            Some((at, _, _)) if self.passes_unix_sockets() => {
                format!("socket,id={id},fd={}", first_client + at as i32)
            }
            _ if id == "tpm" => format!("socket,id={id},path={}", self.run_folder.join(file).display()),
            _ => format!(
                "socket,id={id},path={},reconnect-ms={RECONNECT_MS}",
                self.run_folder.join(file).display()
            ),
        }
    }

    /// QEMU's own filter, if this QEMU has one.
    pub(super) fn qemu_sandbox_arguments(&self) -> Vec<String> {
        match &self.jail {
            Some(jail) if jail.qemu_sandbox => {
                let spawns =
                    !self.has_amd_gpu() || self.starts_render_server() || self.shared_folder().is_some();
                let sandbox = if spawns { QEMU_SANDBOX } else { QEMU_SANDBOX_NO_SPAWN };
                vec!["-sandbox".to_owned(), sandbox.to_owned()]
            }
            _ => Vec::new(),
        }
    }

    /// A jailed QEMU starts with only the environment it needs, as the user's may hold secrets. Its
    /// temporary files go in its run folder, as the PC's `/tmp` is out of its reach.
    pub(super) fn jailed_environment(&self, jail: &QemuJail) -> Vec<(String, String)> {
        let mut environment = vec![
            ("PATH".to_owned(), PATH.to_owned()),
            ("TMPDIR".to_owned(), self.run_folder.display().to_string()),
        ];
        if let Some(folder) = jail.sound_socket.as_deref().and_then(Path::parent) {
            environment.push(("XDG_RUNTIME_DIR".to_owned(), folder.display().to_string()));
        }
        environment
    }
}

#[cfg(test)]
mod tests;
