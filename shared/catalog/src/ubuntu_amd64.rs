//! The Linux host's rule for Ubuntu's disc: the amd64 server disc of the newest point release of
//! 26.04, picked from the checksum list in releases.ubuntu.com's folder for it. The list itself is
//! fetched by the host, and the disc is checked against the sum picked here.

/// Ubuntu's release folder for the version the Ubuntu install is made for, and its checksum list.
pub const UBUNTU_RELEASE: &str = "https://releases.ubuntu.com/26.04/";
pub const UBUNTU_SUMS: &str = "SHA256SUMS";
/// The server disc's name begins and ends so, with the point release between.
pub const UBUNTU_PREFIX: &str = "ubuntu-26.04";
pub const UBUNTU_SUFFIX: &str = "-live-server-amd64.iso";

/// A server disc's point release, from its name: nothing between the prefix and the suffix is the
/// first, and `.` and digits are a later one. Any other name isn't an Ubuntu disc's.
fn point_release(name: &str) -> Option<u32> {
    let middle = name.strip_prefix(UBUNTU_PREFIX)?.strip_suffix(UBUNTU_SUFFIX)?;
    if middle.is_empty() {
        return Some(0);
    }
    let digits = middle.strip_prefix('.')?;
    (!digits.is_empty() && digits.len() <= 4 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// The newest point release's server disc in Ubuntu's checksum list, and its SHA-256.
pub fn newest_server_disc(sums: &str) -> Option<(String, String)> {
    sums.lines()
        .filter_map(|line| {
            let (sum, name) = line.split_once(' ')?;
            let name = name.trim().trim_start_matches('*');
            // 26.04 sorts before 26.04.1, which sorts before 26.04.2.
            Some((point_release(name)?, name.to_owned(), sum.to_owned()))
        })
        .max_by_key(|(point, _, _)| *point)
        .map(|(_, name, sum)| (name, sum))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_point_releases_server_disc_is_picked() {
        let pick = |sums: &str| newest_server_disc(sums).map(|(name, sum)| format!("{sum} {name}"));
        let sums = "aa *ubuntu-26.04-desktop-amd64.iso\nbb *ubuntu-26.04-live-server-amd64.iso\n\
                    cc *ubuntu-26.04.1-live-server-amd64.iso\ndd *ubuntu-26.04.1-live-server-arm64.iso\n";
        assert_eq!(pick(sums).unwrap(), "cc ubuntu-26.04.1-live-server-amd64.iso");
        let ten = "b *ubuntu-26.04.10-live-server-amd64.iso\na *ubuntu-26.04.9-live-server-amd64.iso\n";
        assert_eq!(pick(ten).unwrap(), "b ubuntu-26.04.10-live-server-amd64.iso");
        assert_eq!(pick("x *ubuntu-26.04-live-server-arm64.iso\n"), None);
    }

    #[test]
    fn a_name_with_anything_but_a_point_release_between_is_no_disc() {
        let pick = |name: &str| newest_server_disc(&format!("aa *{name}\n"));
        assert!(pick("ubuntu-26.04.2-live-server-amd64.iso").is_some());
        for name in [
            "ubuntu-26.04/../../x-live-server-amd64.iso",
            "ubuntu-26.04.1/../../x-live-server-amd64.iso",
            "ubuntu-26.04..1-live-server-amd64.iso",
            "ubuntu-26.04.-live-server-amd64.iso",
            "ubuntu-26.04.1a-live-server-amd64.iso",
            "ubuntu-26.04-beta-live-server-amd64.iso",
            "ubuntu-26.04.-1-live-server-amd64.iso",
            "ubuntu-26.04.+1-live-server-amd64.iso",
        ] {
            assert_eq!(pick(name), None, "{name}");
        }
    }
}
