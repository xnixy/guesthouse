//! A machine's network: its mode, port forwards, MAC address and the host's bridge. Nothing here
//! needs a display or a VM. What it asks of the host is in [`crate::bridge`].

use crate::random::random_bytes;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::net::{TcpListener, UdpSocket};
use std::path::Path;

pub const MOST_PORT: u32 = 65535;
/// The guest's agent listens here.
pub const GUEST_AGENT_PORT: u16 = 8765;
/// QEMU's own range, which is locally administered.
const MAC_PREFIX: &str = "52:54:00";
/// What QEMU gives a NIC when it is told nothing.
pub const DEFAULT_MAC: &str = "52:54:00:12:34:56";
/// The second NIC's, which only ever sits on the VM's private link.
pub const CONTROL_MAC: &str = "52:54:00:67:68:01";
pub const DEFAULT_BRIDGE: &str = "br0";
/// Where install.sh puts Guesthouse's own bridge helper, setuid root. It says the same.
pub const OWN_BRIDGE_HELPER: &str = "/usr/local/libexec/guesthouse/qemu-bridge-helper";
/// Makes a tap as root, for a QEMU that runs as this user. Guesthouse's own comes first. A build of the
/// repo has none, and then it's the distribution's: Fedora keeps it in the second of these, and Debian,
/// Ubuntu and Arch in the third.
const BRIDGE_HELPERS: [&str; 3] =
    [OWN_BRIDGE_HELPER, "/usr/libexec/qemu-bridge-helper", "/usr/lib/qemu/qemu-bridge-helper"];
/// The kernel's limit on an interface's name.
const MOST_BRIDGE_NAME: usize = 15;
const FIRST_FREE_PORT_FILE: &str = "/proc/sys/net/ipv4/ip_unprivileged_port_start";
/// If this kernel won't say.
const USUAL_FIRST_PORT: u16 = 1024;

fn first_that_exists<'a>(places: &[&'a str], exists: impl Fn(&str) -> bool) -> &'a str {
    places.iter().copied().find(|place| exists(place)).unwrap_or(places[0])
}

/// Where this PC keeps the helper. If it has none, the bridge check says so about Guesthouse's own.
pub fn bridge_helper() -> &'static str {
    first_that_exists(&BRIDGE_HELPERS, |place| Path::new(place).exists())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Nat,
    None,
    Bridged,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Nat, Self::None, Self::Bridged];

    pub fn name(self) -> &'static str {
        match self {
            Self::Nat => "NAT",
            Self::None => "None",
            Self::Bridged => "Bridged",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub const ALL: [Self; 2] = [Self::Tcp, Self::Udp];

    /// As QEMU's `hostfwd` writes it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    pub protocol: Protocol,
    pub host: u32,
    pub guest: u32,
}

impl Forward {
    pub fn new(protocol: Protocol, host: u16, guest: u16) -> Self {
        Self { protocol, host: host.into(), guest: guest.into() }
    }

    /// As the settings list shows it.
    pub fn text(&self) -> String {
        format!("{} {} to {}", self.protocol.name(), self.host, self.guest)
    }

    /// How the user is told which port is meant.
    pub fn port_text(&self) -> String {
        format!("{} port {}", self.protocol.name(), self.host)
    }

    fn same_host_port(&self, other: &Self) -> bool {
        (self.protocol, self.host) == (other.protocol, other.host)
    }
}

/// A MAC of QEMU's range that nothing here uses.
pub fn new_mac<'a>(taken: impl IntoIterator<Item = &'a str> + Clone) -> String {
    loop {
        let bytes = random_bytes::<3>();
        let mac = format!("{MAC_PREFIX}:{:02x}:{:02x}:{:02x}", bytes[0], bytes[1], bytes[2]);
        if mac != DEFAULT_MAC && mac != CONTROL_MAC && !taken.clone().into_iter().any(|old| old == mac) {
            return mac;
        }
    }
}

/// The MAC the user typed, in the form QEMU takes, or `None` if it isn't one that can be used.
pub fn read_mac(text: &str) -> Option<String> {
    let text = text.trim().to_lowercase().replace('-', ":");
    let pairs: Vec<&str> = text.split(':').collect();
    let hex = |pair: &&str| pair.len() == 2 && pair.bytes().all(|byte| byte.is_ascii_hexdigit());
    (pairs.len() == 6 && pairs.iter().all(hex)).then_some(text)
}

/// Why a NIC can't have this MAC. A low bit set in its first byte would make it a multicast address,
/// which guests and switches drop.
pub fn mac_problem(mac: &str) -> Option<String> {
    if read_mac(mac).as_deref() != Some(mac) {
        return Some("A MAC address is six pairs of hex digits, like 52:54:00:ab:cd:ef.".into());
    }
    if u8::from_str_radix(&mac[..2], 16).is_ok_and(|first| first & 1 == 1) {
        return Some(
            "This is a multicast address, so a network card cannot have it. The first pair must be even."
                .into(),
        );
    }
    if mac == "00:00:00:00:00:00" {
        return Some("A network card cannot have an address of all zeros.".into());
    }
    (mac == CONTROL_MAC).then(|| "Guesthouse's own link to the guest uses this address.".into())
}

pub fn read_port(text: &str) -> Option<u16> {
    let text = text.trim();
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    text.parse::<u16>().ok().filter(|port| digits && *port != 0)
}

fn valid_port(port: u32) -> bool {
    (1..=MOST_PORT).contains(&port)
}

/// What is wrong with a saved list, whatever the host. Clashes with what runs now aren't here, as
/// they must not stop a machine loading.
pub fn forward_list_problems(forwards: &[Forward]) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for item in forwards {
        if !valid_port(item.host) || !valid_port(item.guest) {
            found.push(format!("Ports must be whole numbers from 1 to {MOST_PORT}."));
        } else if !seen.insert((item.protocol, item.host)) {
            found.push(format!("{} on the host is forwarded twice.", item.port_text()));
        }
    }
    found
}

/// The lowest port this user can listen on.
pub fn lowest_free_port() -> u16 {
    let text = fs::read_to_string(FIRST_FREE_PORT_FILE).unwrap_or_default();
    text.trim().parse().unwrap_or(USUAL_FIRST_PORT)
}

/// Why this forward can't work on this host. `reserved` holds TCP ports and what they're for. Only a
/// Windows machine has the agent, and any other guest may well use that port for itself.
pub fn forward_problem(
    item: &Forward,
    reserved: &BTreeMap<u32, String>,
    windows: bool,
    lowest: u16,
) -> Option<String> {
    if item.host < u32::from(lowest) {
        return Some(format!(
            "Ports below {lowest} need root on this PC. Choose host port {lowest} or above."
        ));
    }
    if item.protocol == Protocol::Tcp {
        if let Some(reason) = reserved.get(&item.host) {
            return Some(format!("Host port {} is {reason}.", item.host));
        }
        if windows && item.guest == u32::from(GUEST_AGENT_PORT) {
            return Some(format!(
                "Guest port {GUEST_AGENT_PORT} is the agent's, which stays private to Guesthouse."
            ));
        }
    }
    None
}

/// Which of `others`, as names and their forwards, also forwards this host port.
pub fn forward_clash<'a>(item: &Forward, others: &[(&'a str, &[Forward])]) -> Option<&'a str> {
    others
        .iter()
        .find(|(_, forwards)| forwards.iter().any(|old| old.same_host_port(item)))
        .map(|other| other.0)
}

/// Whether something on this PC already has this port on loopback, where a forward would listen.
pub fn port_taken(protocol: Protocol, port: u16) -> bool {
    let address = ("127.0.0.1", port);
    match protocol {
        Protocol::Tcp => TcpListener::bind(address).is_err(),
        Protocol::Udp => UdpSocket::bind(address).is_err(),
    }
}

/// What a running QEMU's network is, from its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InUse {
    Passt,
    Bridge,
    Restricted,
    Slirp,
}

const SLIRP_NAME: &str = "QEMU's built-in NAT (slirp)";
const SECOND_ADAPTER: &str = "The guest sees a second network adapter, which only Guesthouse uses.";

pub fn in_use_of(words: &[Vec<u8>]) -> Option<InUse> {
    let nic = words.windows(2).find(|pair| pair[0] == b"-nic")?;
    let word = &nic[1];
    let has = |needle: &[u8]| word.windows(needle.len()).any(|part| part == needle);
    Some(if word.starts_with(b"passt,") {
        InUse::Passt
    } else if word.starts_with(b"bridge,") {
        InUse::Bridge
    } else if has(b"restrict=on") {
        InUse::Restricted
    } else {
        InUse::Slirp
    })
}

/// What these settings give the guest.
pub fn describe(mode: Mode, passt: bool, installed: bool) -> String {
    match mode {
        Mode::None => {
            "No internet. Guesthouse can still reach the guest, and a shared folder still works.".into()
        }
        Mode::Bridged => format!("On this PC's network, with an address from its router. {SECOND_ADAPTER}"),
        Mode::Nat if passt && installed => format!("Uses passt. {SECOND_ADAPTER}"),
        Mode::Nat if passt => format!("passt is not installed, so this uses {SLIRP_NAME}."),
        Mode::Nat => format!("Uses {SLIRP_NAME}."),
    }
}

/// What a running machine really has.
pub fn in_use_text(found: Option<InUse>) -> String {
    let name = match found {
        Some(InUse::Passt) => "passt",
        Some(InUse::Bridge) => "the bridge",
        Some(InUse::Restricted) => "no internet",
        Some(InUse::Slirp) => SLIRP_NAME,
        None => return String::new(),
    };
    format!("Running now with {name}.")
}

/// The name goes into QEMU's options, where a comma would start another option.
pub fn bridge_name_problem(bridge: &str) -> Option<String> {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte);
    let good = (1..=MOST_BRIDGE_NAME).contains(&bridge.len()) && bridge.bytes().all(allowed);
    (!good).then(|| "A bridge's name is up to 15 letters, digits, dots, dashes or underscores.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_is_found_where_each_distribution_keeps_it() {
        let debian = |place: &str| place.starts_with("/usr/lib/qemu/");
        assert_eq!(first_that_exists(&BRIDGE_HELPERS, debian), "/usr/lib/qemu/qemu-bridge-helper");
        let fedora = |place: &str| place.starts_with("/usr/libexec/");
        assert_eq!(first_that_exists(&BRIDGE_HELPERS, fedora), "/usr/libexec/qemu-bridge-helper");
        assert_eq!(first_that_exists(&BRIDGE_HELPERS, |_| false), OWN_BRIDGE_HELPER);
    }

    #[test]
    fn guesthouse_s_own_helper_comes_before_the_distribution_s() {
        let installed = |place: &str| place == OWN_BRIDGE_HELPER || place.starts_with("/usr/libexec/");
        assert_eq!(first_that_exists(&BRIDGE_HELPERS, installed), OWN_BRIDGE_HELPER);
    }

    fn tcp(host: u16, guest: u16) -> Forward {
        Forward::new(Protocol::Tcp, host, guest)
    }

    fn reserved() -> BTreeMap<u32, String> {
        BTreeMap::from([(18765, "the agent's".to_owned())])
    }

    #[test]
    fn a_new_mac_is_usable_and_ours() {
        let made = new_mac(std::iter::empty());
        assert!(made.starts_with(MAC_PREFIX));
        assert_eq!(mac_problem(&made), None);
    }

    #[test]
    fn a_mac_is_read_as_a_person_may_type_it() {
        assert_eq!(read_mac(" 52-54-00-AB-CD-EF ").as_deref(), Some("52:54:00:ab:cd:ef"));
        assert_eq!(read_mac("52:54:00:ab:cd"), None);
    }

    #[test]
    fn unusable_macs_say_why() {
        assert!(mac_problem("01:00:5e:00:00:01").unwrap().contains("multicast"));
        assert!(mac_problem("00:00:00:00:00:00").unwrap().contains("zeros"));
        assert!(mac_problem(CONTROL_MAC).unwrap().contains("own link"));
        assert!(mac_problem("nothing").unwrap().contains("six pairs"));
    }

    #[test]
    fn two_protocols_make_two_ports() {
        let forwards = [tcp(2222, 22), Forward::new(Protocol::Udp, 2222, 22)];
        assert!(forward_list_problems(&forwards).is_empty());
    }

    #[test]
    fn a_saved_list_is_checked() {
        assert!(forward_list_problems(&[tcp(2222, 22), tcp(2222, 23)])[0].contains("twice"));
        assert!(!forward_list_problems(&[tcp(0, 22)]).is_empty());
        let too_high = Forward { protocol: Protocol::Tcp, host: 2222, guest: 65536 };
        assert!(!forward_list_problems(&[too_high]).is_empty());
    }

    #[test]
    fn ports_are_whole_numbers_in_range() {
        assert_eq!(read_port("8080"), Some(8080));
        for text in ["0", "65536", "port", &"9".repeat(400), "", "+80"] {
            assert_eq!(read_port(text), None, "{text}");
        }
    }

    #[test]
    fn forwards_are_judged_against_the_host() {
        let forward = |item, windows| forward_problem(&item, &reserved(), windows, 1024);
        assert!(forward(tcp(80, 80), false).unwrap().contains("need root"));
        assert!(forward(tcp(18765, 22), false).unwrap().contains("agent"));
        assert_eq!(forward(Forward::new(Protocol::Udp, 18765, 22), false), None, "the agent's is TCP");
        assert!(forward(tcp(2222, 8765), true).unwrap().contains("agent"));
        assert_eq!(forward(tcp(2222, 8765), false), None, "only Windows guests have the agent");
    }

    #[test]
    fn a_clash_names_the_other_machine() {
        let others: &[(&str, &[Forward])] = &[("Other", &[tcp(2222, 80)])];
        assert_eq!(forward_clash(&tcp(2222, 22), others), Some("Other"));
        assert_eq!(forward_clash(&Forward::new(Protocol::Udp, 2222, 22), others), None);
    }

    #[test]
    fn a_port_in_use_is_seen() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(port_taken(Protocol::Tcp, listener.local_addr().unwrap().port()));
    }

    #[test]
    fn a_running_network_is_read_from_its_arguments() {
        let words = |nic: &str| vec![b"qemu".to_vec(), b"-nic".to_vec(), nic.as_bytes().to_vec()];
        assert_eq!(in_use_of(&words("passt,model=e1000e")), Some(InUse::Passt));
        assert_eq!(in_use_of(&words("bridge,br=br0")), Some(InUse::Bridge));
        assert_eq!(in_use_of(&words("user,restrict=on")), Some(InUse::Restricted));
        assert_eq!(in_use_of(&words("user,model=e1000e")), Some(InUse::Slirp));
        assert_eq!(in_use_text(None), "");
    }

    #[test]
    fn passt_settings_are_described_as_they_will_work() {
        assert!(describe(Mode::Nat, true, true).contains("passt"));
        assert!(describe(Mode::Nat, true, false).contains("not installed"));
    }

    #[test]
    fn a_comma_cannot_reach_qemus_options() {
        assert!(bridge_name_problem("br0,helper=/tmp/x").unwrap().contains("up to 15"));
        assert_eq!(bridge_name_problem("br0"), None);
    }
}
