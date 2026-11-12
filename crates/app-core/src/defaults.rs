//! What a new machine starts as, for the app's dialog and the command line alike.

use crate::account::default_user;
use crate::machine::Kind;
use std::path::Path;

/// What a machine that is only a plain VM gets no part in.
pub const NO_GPU: &str = "This machine gets no GPU acceleration. It runs on the CPU, with a basic display.";

/// Sizes to start from. A plain machine is usually something small being tried out.
pub struct Sizes {
    pub cores: u32,
    pub ram: u64,
    pub disk: u32,
}

const WINDOWS_SIZES: Sizes = Sizes { cores: 8, ram: 16, disk: 128 };
const PLAIN_SIZES: Sizes = Sizes { cores: 4, ram: 4, disk: 32 };
const UBUNTU_SIZES: Sizes = Sizes { cores: 8, ram: 8, disk: 80 };

pub fn sizes_for(kind: Kind) -> Sizes {
    match kind {
        Kind::Windows => WINDOWS_SIZES,
        Kind::Linux => UBUNTU_SIZES,
        Kind::Plain => PLAIN_SIZES,
    }
}

pub fn suggested_name(kind: Kind, path: &str) -> String {
    match kind {
        Kind::Windows => "Windows 11".to_owned(),
        Kind::Linux => "Ubuntu".to_owned(),
        Kind::Plain => {
            Path::new(path).file_stem().map_or_else(String::new, |name| name.to_string_lossy().into_owned())
        }
    }
}

/// The name as the machine keeps it: empty for the default, so a machine made with the default reads
/// the same as one from before the choice.
pub fn account_name(kind: Kind, typed: &str) -> String {
    if typed == default_user(kind) { String::new() } else { typed.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_account_is_kept_as_no_choice() {
        assert_eq!(account_name(Kind::Windows, "gpupv"), "");
        assert_eq!(account_name(Kind::Linux, "desk"), "");
        assert_eq!(account_name(Kind::Windows, "ana"), "ana");
        assert_eq!(account_name(Kind::Linux, "gpupv"), "gpupv");
    }

    #[test]
    fn each_kind_of_disc_has_its_name() {
        assert_eq!(suggested_name(Kind::Windows, "/discs/win.iso"), "Windows 11");
        assert_eq!(suggested_name(Kind::Linux, "/discs/server.iso"), "Ubuntu");
        assert_eq!(suggested_name(Kind::Plain, "/discs/tiny-linux.iso"), "tiny-linux");
    }
}
