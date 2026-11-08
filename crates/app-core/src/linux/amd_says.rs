//! What a Linux guest on an AMD card says on its serial port, from amd-up.sh and the Mesa build:
//!
//! - `guesthouse-amd building DONE TOTAL` while it builds Mesa with native context
//! - `guesthouse-amd built VERSION` once that Mesa is installed
//! - `guesthouse-amd native RENDERER` or `guesthouse-amd venus RENDERER` when its 3D works
//! - `guesthouse-amd failed REASON` when native context doesn't, so the next start is on Venus
//!
//! The newest line counts. As with the forwarded GPU, the guest writes these, so only plain words of a
//! sane length are kept.

use super::amd::{AmdMode, fall_back, try_native_again};
use super::guest_says::{said, serial_tail};
use crate::context::Context;
use crate::machine::Machine;
use std::path::Path;

const PREFIX: &str = "guesthouse-amd ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmdSays {
    Building { done: u32, total: u32 },
    Built { version: String },
    Native { renderer: String },
    Venus { renderer: String },
    Failed { reason: String },
}

impl AmdSays {
    /// The words for the machine's row.
    pub fn text(&self, mode: AmdMode) -> String {
        match (self, mode) {
            (Self::Building { done, total }, _) => {
                format!("The guest is building Mesa for the AMD GPU: {done} of {total} steps.")
            }
            (Self::Built { version }, AmdMode::Venus) => format!(
                "The guest built Mesa {version} for native context, which the next start of the VM uses."
            ),
            (Self::Built { version }, AmdMode::Native) => {
                format!("The guest built Mesa {version} for the AMD GPU, and is checking it.")
            }
            (Self::Native { renderer }, _) => {
                format!("The guest has its AMD GPU, on native context: {renderer}.")
            }
            (Self::Venus { renderer }, _) => format!("The guest has its AMD GPU, on Venus: {renderer}."),
            (Self::Failed { reason }, AmdMode::Native) => {
                format!("Native context failed in the guest: {reason}. The next start of the VM uses Venus.")
            }
            (Self::Failed { reason }, AmdMode::Venus) => {
                format!("The guest's GPU failed on Venus: {reason}.")
            }
        }
    }

    /// Whether the start can stop waiting on the guest: it has its GPU or has said why not.
    pub fn settled(&self) -> bool {
        matches!(self, Self::Native { .. } | Self::Venus { .. } | Self::Failed { .. })
    }
}

fn read_line(line: &str) -> Option<AmdSays> {
    let (word, rest) = said(line, PREFIX)?;
    let one_word = !rest.is_empty() && !rest.contains(' ');
    match word {
        "building" => {
            let (done, total) = rest.split_once(' ')?;
            let (done, total) = (done.parse().ok()?, total.parse().ok()?);
            (done <= total).then_some(AmdSays::Building { done, total })
        }
        "built" if one_word => Some(AmdSays::Built { version: rest }),
        "native" if !rest.is_empty() => Some(AmdSays::Native { renderer: rest }),
        "venus" if !rest.is_empty() => Some(AmdSays::Venus { renderer: rest }),
        "failed" if !rest.is_empty() => Some(AmdSays::Failed { reason: rest }),
        _ => None,
    }
}

/// The newest thing the guest said, if it said anything.
pub fn newest(serial: &[u8]) -> Option<AmdSays> {
    String::from_utf8_lossy(serial).lines().rev().find_map(read_line)
}

/// What the guest in this run folder said last.
pub fn read(run_folder: &Path) -> Option<AmdSays> {
    newest(&serial_tail(run_folder)?)
}

/// Keeps what the guest said for the next start: a native context that failed goes to Venus, and a
/// guest on Venus that has since built Mesa for native context goes back to it.
pub fn learn(context: &Context, machine: &Machine, mode: AmdMode, says: &AmdSays) {
    match (says, mode) {
        (AmdSays::Failed { reason }, AmdMode::Native) => {
            // Lost, it costs the user the fallback once, and the guest says it again on the next boot.
            let _ = fall_back(context, machine, reason);
        }
        (AmdSays::Built { .. }, AmdMode::Venus) => try_native_again(context, machine),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::amd::amd_mode;
    use super::*;
    use crate::testing::{Scratch, scratch_context};

    #[test]
    fn the_newest_line_counts() {
        let serial = b"guesthouse-amd building 10 2400\n[ 3.1] noise\nguesthouse-amd building 2400 2400\n\
                       guesthouse-amd built 25.2.8-0ubuntu0.24.04.4+guesthouse1\n\
                       guesthouse-amd native AMD Radeon 610M (radeonsi, gfx1036, LLVM 20.1.2, DRM 3.64)\r\n";
        let Some(AmdSays::Native { renderer }) = newest(serial) else { panic!("it should be native") };
        assert!(renderer.starts_with("AMD Radeon 610M (radeonsi"), "{renderer}");
        assert_eq!(
            newest(b"guesthouse-amd building 812 2400\n"),
            Some(AmdSays::Building { done: 812, total: 2400 })
        );
    }

    #[test]
    fn only_a_line_of_its_own_counts() {
        for serial in [
            &b""[..],
            b"echo guesthouse-amd native radeonsi\n",
            b"guesthouse-amd native\n",
            b"guesthouse-amd building 3\n",
            b"guesthouse-amd building 9 3\n",
            b"guesthouse-amd built 26.0 and more\n",
            b"guesthouse-amd sideways radeonsi\n",
        ] {
            assert_eq!(newest(serial), None, "{}", String::from_utf8_lossy(serial));
        }
    }

    #[test]
    fn a_failure_on_native_context_moves_the_next_start_to_venus_and_a_new_build_moves_it_back() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = Machine::new("ubuntu", "Ubuntu", "/m/disk.qcow2", "");
        std::fs::create_dir_all(machine.folder(&context)).unwrap();
        let failed =
            newest(b"guesthouse-amd failed the Mesa build failed, see /var/log/guesthouse\n").unwrap();
        assert!(failed.settled());
        assert!(failed.text(AmdMode::Native).ends_with("The next start of the VM uses Venus."));
        learn(&context, &machine, AmdMode::Native, &failed);
        assert_eq!(amd_mode(&context, &machine), AmdMode::Venus);
        learn(&context, &machine, AmdMode::Venus, &failed);
        assert_eq!(amd_mode(&context, &machine), AmdMode::Venus, "Venus has nothing to fall back to");
        let built = AmdSays::Built { version: "26.2.3-1.fc45.guesthouse1".into() };
        assert!(!built.settled());
        learn(&context, &machine, AmdMode::Venus, &built);
        assert_eq!(amd_mode(&context, &machine), AmdMode::Native);
    }
}
