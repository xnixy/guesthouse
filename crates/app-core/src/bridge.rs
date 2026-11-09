//! What the host needs for a bridged machine, and what to tell the user to do about it.

use crate::network::{bridge_helper, bridge_name_problem};
use crate::text::shell_quote;
use rustix::fs::{Mode, stat};
use std::fs;
use std::path::{Path, PathBuf};

const FILE_CAPABILITIES: &str = "security.capability";
/// Debian, Ubuntu and Mint ship the helper without setuid, on purpose. This gives it the one right it
/// needs to make a tap, rather than root.
const HELPER_RIGHT: &str = "cap_net_admin+ep";
/// How deep QEMU's helper follows includes before this stops looking.
const MOST_INCLUDES: u32 = 8;

/// The files the bridge checks read, which a test can point elsewhere.
#[derive(Debug, Clone)]
pub struct BridgeHost {
    pub net: PathBuf,
    pub helper: PathBuf,
    pub config: PathBuf,
    pub routes: PathBuf,
    pub firewall: PathBuf,
}

impl Default for BridgeHost {
    fn default() -> Self {
        Self {
            net: "/sys/class/net".into(),
            helper: bridge_helper().into(),
            config: "/etc/qemu/bridge.conf".into(),
            routes: "/proc/net/route".into(),
            firewall: "/proc/sys/net/bridge/bridge-nf-call-iptables".into(),
        }
    }
}

impl BridgeHost {
    /// The helper makes the tap as root, for a QEMU that runs as this user.
    fn helper_can_run(&self) -> bool {
        let setuid =
            stat(&self.helper).is_ok_and(|found| Mode::from_raw_mode(found.st_mode).contains(Mode::SUID));
        let mut buffer = [0u8; 64];
        setuid
            || rustix::fs::getxattr(&self.helper, FILE_CAPABILITIES, &mut buffer[..])
                .is_ok_and(|size| size > 0)
    }

    /// Installed, but not able to make a tap for this user yet.
    fn helper_needs_right(&self) -> bool {
        self.helper.exists() && !self.helper_can_run()
    }

    /// As the helper decides: something must allow the bridge, and a deny always wins. `None` if some
    /// of it can't be read: a distribution may keep it from users, and only the helper can then say.
    fn bridge_allowed(&self, bridge: &str) -> Option<bool> {
        let lines = config_lines(&self.config, 0)?;
        let named =
            |verb: &str| lines.iter().any(|(word, name)| word == verb && (name == bridge || name == "all"));
        Some(named("allow") && !named("deny"))
    }

    pub fn exists(&self, bridge: &str) -> bool {
        self.net.join(bridge).join("bridge").is_dir()
    }

    /// Everything still in the way of running on this bridge. Nothing left means ready.
    pub fn problems(&self, bridge: &str) -> Vec<String> {
        if let Some(problem) = bridge_name_problem(bridge) {
            return vec![problem];
        }
        let mut found = Vec::new();
        let ports = fs::read_dir(self.net.join(bridge).join("brif"));
        if !self.exists(bridge) {
            found.push(format!("This PC has no bridge called {bridge}."));
        } else if ports.is_ok_and(|mut ports| ports.next().is_none()) {
            found.push(format!("Nothing is connected to {bridge}, so it does not reach the network."));
        }
        if !self.helper.exists() {
            found.push(format!(
                "{} is missing. It comes in the package qemu-common, or qemu-system-common on Debian, \
                 Ubuntu and Mint.",
                self.helper.display()
            ));
        } else if self.helper_needs_right() {
            found.push(format!(
                "{} cannot run as root. Debian, Ubuntu and Mint ship it that way.",
                self.helper.display()
            ));
        }
        if self.bridge_allowed(bridge) == Some(false) {
            found.push(format!("{} does not allow {bridge}.", self.config.display()));
        }
        found
    }

    /// The interface this PC reaches the internet through.
    pub fn default_interface(&self) -> Option<String> {
        let text = fs::read_to_string(&self.routes).ok()?;
        text.lines().skip(1).find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields.len() > 1 && fields[1] == "00000000").then(|| fields[0].to_owned())
        })
    }

    pub fn is_wireless(&self, interface: &str) -> bool {
        self.net.join(interface).join("wireless").is_dir()
    }

    /// Whether the host's firewall sees frames crossing a bridge. Docker turns this on, with rules
    /// that drop forwarded traffic, and a bridged guest then gets no network and no error.
    pub fn filtered(&self) -> bool {
        fs::read_to_string(&self.firewall).is_ok_and(|text| text.trim() == "1")
    }

    /// The one step that needs root, for NetworkManager, less what is already done. The bridge takes
    /// over the interface's address, so the port needs a higher priority than the wired profile or
    /// that would come back at boot.
    pub fn commands(&self, bridge: &str, interface: Option<&str>, exists: bool) -> String {
        let config = self.config.to_string_lossy();
        let folder = self.config.parent().unwrap_or(Path::new("/")).to_string_lossy();
        let mut allow = format!(
            "sudo mkdir -p {}\necho 'allow {bridge}' | sudo tee -a {}",
            shell_quote(&folder),
            shell_quote(&config)
        );
        if self.helper_needs_right() {
            allow += &format!("\nsudo setcap {HELPER_RIGHT} {}", shell_quote(&self.helper.to_string_lossy()));
        }
        let Some(interface) = interface.filter(|_| !exists) else { return allow };
        let port = shell_quote(&format!("{bridge}-{interface}"));
        let (name, device) = (shell_quote(bridge), shell_quote(interface));
        format!(
            "sudo nmcli connection add type bridge ifname {name} con-name {name} stp no\n\
             sudo nmcli connection add type ethernet ifname {device} con-name {port} \
             master {name} slave-type bridge connection.autoconnect-priority 10\n\
             sudo nmcli connection up {port}\n\
             sudo nmcli connection up {name}\n{allow}"
        )
    }
}

/// The verb and name of each line of QEMU's bridge config, with its includes.
fn config_lines(path: &Path, depth: u32) -> Option<Vec<(String, String)>> {
    if depth > MOST_INCLUDES {
        return Some(Vec::new());
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut lines = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split('#').next().unwrap_or("").split_whitespace().collect();
        match words[..] {
            ["include", file] => lines.extend(config_lines(Path::new(file), depth + 1)?),
            [verb, name] => lines.push((verb.to_owned(), name.to_owned())),
            _ => {}
        }
    }
    Some(lines)
}

/// What to say above the commands. None of this can be checked from here.
pub fn advice(bridge: &str, interface: Option<&str>, wireless: bool, filtered: bool) -> String {
    let Some(interface) = interface else {
        return "This PC has no network connection to bridge right now. Connect it by cable first.".into();
    };
    if wireless {
        return format!(
            "{interface} is a wireless connection, and most Wi-Fi cards cannot join a bridge. Connect this \
             PC by cable and look here again, or use NAT."
        );
    }
    let mut text = format!(
        "Run these once in a terminal. They move this PC's address from {interface} to {bridge}, so \
         the network drops for a few seconds."
    );
    if filtered {
        text += &format!(
            " This PC's firewall also sees traffic on bridges, which Docker turns on, and may drop it. If \
             the guest gets no address, run: sudo iptables -I FORWARD -i {bridge} -o {bridge} -j ACCEPT"
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;
    use std::os::unix::fs::PermissionsExt;

    fn host(scratch: &Scratch) -> BridgeHost {
        fs::create_dir_all(scratch.path("net")).unwrap();
        BridgeHost {
            net: scratch.path("net"),
            helper: scratch.write("helper", ""),
            config: scratch.path("bridge.conf"),
            routes: scratch.path("route"),
            firewall: scratch.path("firewall"),
        }
    }

    fn problems(host: &BridgeHost) -> String {
        host.problems("br0").join(" ")
    }

    fn make_bridge(scratch: &Scratch) {
        fs::create_dir_all(scratch.path("net/br0/bridge")).unwrap();
        fs::create_dir_all(scratch.path("net/br0/brif")).unwrap();
    }

    #[test]
    fn each_thing_in_the_way_is_named_until_it_is_ready() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let said = problems(&host);
        assert!(
            said.contains("no bridge")
                && said.contains("cannot run as root")
                && said.contains("does not allow")
        );
        fs::set_permissions(&host.helper, fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(!problems(&host).contains("cannot run as root"));
        make_bridge(&scratch);
        assert!(problems(&host).contains("Nothing is connected"));
        fs::create_dir_all(scratch.path("net/br0/brif/eno2")).unwrap();
        assert!(problems(&host).contains("does not allow") && !problems(&host).contains("no bridge"));
        let included = scratch.write("more.conf", "# from the admin\nallow br0\n");
        fs::write(&host.config, format!("allow virbr0\ninclude {}\n", included.display())).unwrap();
        assert_eq!(problems(&host), "", "ready, through an include");
        fs::write(&host.config, format!("include {}\ndeny br0\n", included.display())).unwrap();
        assert!(problems(&host).contains("does not allow"), "the helper lets a deny win");
        fs::write(&host.config, "allow all\n").unwrap();
        assert_eq!(problems(&host), "");
    }

    #[test]
    fn no_config_allows_nothing() {
        let scratch = Scratch::new();
        assert_eq!(host(&scratch).bridge_allowed("br0"), Some(false));
    }

    #[test]
    fn a_bad_name_is_all_that_is_said() {
        let scratch = Scratch::new();
        let found = host(&scratch).problems("br0,helper=/tmp/x");
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("up to 15"));
    }

    #[test]
    fn the_way_out_is_found_in_the_route_table() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        assert_eq!(host.default_interface(), None);
        fs::write(
            &host.routes,
            "Iface\tDestination\tGateway\neno2\t00000000\t0100000A\neno2\t0000000A\t00000000\n",
        )
        .unwrap();
        assert_eq!(host.default_interface().as_deref(), Some("eno2"));
        fs::create_dir_all(scratch.path("net/wlan0/wireless")).unwrap();
        assert!(host.is_wireless("wlan0") && !host.is_wireless("eno2"));
    }

    #[test]
    fn advice_fits_the_connection() {
        assert!(advice("br0", Some("wlan0"), true, false).contains("Wi-Fi"));
        assert!(advice("br0", None, false, false).contains("cable"));
        assert!(advice("br0", Some("eno2"), false, true).contains("Docker"));
        assert!(!advice("br0", Some("eno2"), false, false).contains("Docker"));
    }

    #[test]
    fn commands_leave_out_what_is_done() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        let commands = host.commands("br0", Some("eno2"), false);
        assert!(commands.contains("nmcli") && commands.contains("allow br0"));
        assert!(!host.commands("br0", Some("eno2"), true).contains("nmcli"), "only what is left to do");
    }

    #[test]
    fn a_helper_shipped_without_setuid_gets_its_right() {
        let scratch = Scratch::new();
        let host = host(&scratch);
        assert!(problems(&host).contains("Debian, Ubuntu and Mint ship it that way"));
        assert!(!problems(&host).contains("missing"));
        let setcap = format!("sudo setcap cap_net_admin+ep {}", host.helper.display());
        assert!(host.commands("br0", Some("eno2"), true).contains(&setcap));
        assert!(host.commands("br0", Some("eno2"), false).contains(&setcap));
        fs::set_permissions(&host.helper, fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(
            !host.commands("br0", Some("eno2"), true).contains("setcap"),
            "Fedora and Arch ship it setuid"
        );
    }

    #[test]
    fn a_missing_helper_names_the_package_for_each_family() {
        let scratch = Scratch::new();
        let host = BridgeHost { helper: scratch.path("nowhere"), ..host(&scratch) };
        let said = problems(&host);
        assert!(said.contains("is missing") && said.contains("qemu-system-common"));
        assert!(!said.contains("cannot run as root"));
        assert!(!host.commands("br0", Some("eno2"), true).contains("setcap"), "nothing to give a right to");
    }
}
