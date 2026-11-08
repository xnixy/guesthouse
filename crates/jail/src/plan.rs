//! What a jailed process may reach, as data. The app writes a plan as words for `guesthouse-jail`, and
//! the daemon builds one for itself.

use std::path::PathBuf;

/// What a process may do with one path and everything under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Read files and list folders.
    Read,
    /// Read, and run programs and load libraries from it.
    Run,
    /// Read and write files that are already there, without making or removing any.
    Write,
    /// Anything inside: make, remove, write, and make and connect to sockets. For a folder of the
    /// process's own, like a VM's run folder.
    Own,
    /// Read, write and ioctl a device file.
    Device,
    /// Connect to this unix socket.
    Socket,
    /// List what's in a folder, and no more: a device's, so it can be found without the others in
    /// there being opened.
    List,
}

impl Grant {
    const ALL: [(Grant, &str); 7] = [
        (Grant::Read, "--read"),
        (Grant::Run, "--run"),
        (Grant::Write, "--write"),
        (Grant::Own, "--own"),
        (Grant::Device, "--device"),
        (Grant::Socket, "--socket"),
        (Grant::List, "--list"),
    ];

    fn word(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(grant, _)| *grant == self)
            .map(|(_, word)| *word)
            .expect("every grant has a word")
    }

    fn from_word(word: &str) -> Option<Self> {
        Self::ALL.iter().find(|(_, known)| *known == word).map(|(grant, _)| *grant)
    }
}

/// Which ports one kind of network use may have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ports {
    /// Not limited.
    Any,
    /// Only these. An empty list is none at all.
    Only(Vec<u16>),
}

impl Ports {
    pub fn none() -> Self {
        Self::Only(Vec::new())
    }

    fn word(&self) -> String {
        match self {
            Self::Any => ANY_PORT.to_owned(),
            Self::Only(ports) if ports.is_empty() => NO_PORT.to_owned(),
            Self::Only(ports) => ports.iter().map(u16::to_string).collect::<Vec<_>>().join(","),
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        match word {
            ANY_PORT => Some(Self::Any),
            NO_PORT => Some(Self::none()),
            list => list.split(',').map(|port| port.parse().ok()).collect::<Option<_>>().map(Self::Only),
        }
    }
}

const ANY_PORT: &str = "any";
const NO_PORT: &str = "none";

/// The network a jailed process may use. Ports are local for a bind and remote for the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub tcp_bind: Ports,
    pub tcp_connect: Ports,
    pub udp_bind: Ports,
    pub udp_send: Ports,
}

impl Network {
    pub fn none() -> Self {
        Self {
            tcp_bind: Ports::none(),
            tcp_connect: Ports::none(),
            udp_bind: Ports::none(),
            udp_send: Ports::none(),
        }
    }

    fn fields_mut(&mut self) -> [(&'static str, &mut Ports); 4] {
        [
            ("--tcp-bind", &mut self.tcp_bind),
            ("--tcp-connect", &mut self.tcp_connect),
            ("--udp-bind", &mut self.udp_bind),
            ("--udp-send", &mut self.udp_send),
        ]
    }
}

/// Every path a jailed process may reach, and its network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub paths: Vec<(Grant, PathBuf)>,
    pub network: Network,
}

impl Default for Plan {
    fn default() -> Self {
        Self { paths: Vec::new(), network: Network::none() }
    }
}

impl Plan {
    pub fn grant(&mut self, grant: Grant, path: impl Into<PathBuf>) -> &mut Self {
        self.paths.push((grant, path.into()));
        self
    }

    /// The plan as words, two to an item.
    pub fn words(&self) -> Vec<String> {
        let mut words = Vec::new();
        for (grant, path) in &self.paths {
            words.extend([grant.word().to_owned(), path.display().to_string()]);
        }
        let mut network = self.network.clone();
        for (name, ports) in network.fields_mut() {
            words.extend([name.to_owned(), ports.word()]);
        }
        words
    }

    /// Takes one item of [`Plan::words`]. Says `Ok(false)` if the name isn't a plan's.
    pub fn take(&mut self, name: &str, value: &str) -> Result<bool, String> {
        if let Some(grant) = Grant::from_word(name) {
            self.grant(grant, value);
            return Ok(true);
        }
        for (field, ports) in self.network.fields_mut() {
            if field == name {
                *ports =
                    Ports::from_word(value).ok_or_else(|| format!("{name} takes ports, not {value:?}"))?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_comes_back_from_its_words() {
        let mut plan = Plan::default();
        plan.grant(Grant::Run, "/usr").grant(Grant::Own, "/run/user/1000/guesthouse/run-2");
        plan.network.tcp_bind = Ports::Only(vec![18767, 8080]);
        plan.network.tcp_connect = Ports::Any;
        let words = plan.words();
        let mut read = Plan::default();
        for pair in words.chunks(2) {
            assert!(read.take(&pair[0], &pair[1]).unwrap(), "{pair:?}");
        }
        assert_eq!(read, plan);
    }

    #[test]
    fn a_new_plan_has_no_network() {
        let words = Plan::default().words();
        assert_eq!(
            words,
            ["--tcp-bind", "none", "--tcp-connect", "none", "--udp-bind", "none", "--udp-send", "none"]
        );
    }

    #[test]
    fn bad_ports_and_other_names_are_told_apart() {
        let mut plan = Plan::default();
        assert!(plan.take("--tcp-bind", "eighty").is_err());
        assert_eq!(plan.take("--bridge", "br0"), Ok(false));
    }
}
