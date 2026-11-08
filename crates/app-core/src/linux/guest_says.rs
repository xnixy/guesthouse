//! What a Linux guest says about its GPU, on its serial port. gpu-up.sh writes one line at each boot,
//! `nvfwd-gpu on VERSION` or `nvfwd-gpu off REASON`, and the app shows the newest while the VM runs. A
//! guest installed by hand then says where its desktop went, `nvfwd-desktop gpu` or
//! `nvfwd-desktop plain REASON`, and says it again if it later goes back to the plain screen.
//! The guest writes these, so only those prefixes count, and only plain words of a sane length are shown.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const SERIAL_LOG: &str = "serial.log";
const PREFIX: &str = "nvfwd-gpu ";
const DESKTOP_PREFIX: &str = "nvfwd-desktop ";
/// The line is written early in the boot, and a guest's console can say a lot after it.
pub(super) const TAIL_BYTES: u64 = 256 * 1024;
const LONGEST_REASON: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestGpu {
    On { version: String },
    Off { reason: String },
}

impl GuestGpu {
    /// The words for the machine's row.
    pub fn text(&self) -> String {
        match self {
            Self::On { version } => format!("The guest has its GPU, on driver {version}."),
            Self::Off { reason } => format!("The guest has no GPU on this boot: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestDesktop {
    Gpu,
    Plain { reason: String },
}

/// What the guest said on its newest boot: its GPU, and where its desktop is if it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSays {
    pub gpu: GuestGpu,
    pub desktop: Option<GuestDesktop>,
}

impl GuestSays {
    /// The words for the machine's row.
    pub fn text(&self) -> String {
        match (&self.gpu, &self.desktop) {
            (GuestGpu::On { version }, Some(GuestDesktop::Gpu)) => {
                format!("The guest has its GPU, on driver {version}, and its desktop is on it.")
            }
            (GuestGpu::On { .. }, Some(GuestDesktop::Plain { reason })) => {
                format!("{} Its desktop is on the plain screen: {reason}", self.gpu.text())
            }
            _ => self.gpu.text(),
        }
    }

    /// Whether this boot's desktop is on QEMU's display, so the viewer has no GPU frames to wait for.
    pub fn plain_desktop(&self) -> bool {
        matches!(self.gpu, GuestGpu::Off { .. }) || matches!(self.desktop, Some(GuestDesktop::Plain { .. }))
    }
}

/// Keeps what a person would type, so a guest can't send the row control characters or markup.
pub(super) fn plain(text: &str) -> String {
    let typed = |letter: &char| letter.is_ascii_graphic() || *letter == ' ';
    let kept = text.chars().filter(|letter| typed(letter) && !matches!(letter, '<' | '>' | '&'));
    kept.take(LONGEST_REASON).collect::<String>().trim().to_owned()
}

/// A line's word, and the plain words after it, if it has the prefix.
pub(super) fn said<'a>(line: &'a str, prefix: &str) -> Option<(&'a str, String)> {
    let said = line.trim_end_matches('\r').strip_prefix(prefix)?;
    let (word, rest) = said.split_once(' ').unwrap_or((said, ""));
    Some((word, plain(rest)))
}

fn read_gpu(line: &str) -> Option<GuestGpu> {
    match said(line, PREFIX)? {
        ("on", rest) if !rest.is_empty() && !rest.contains(' ') => Some(GuestGpu::On { version: rest }),
        ("off", rest) if !rest.is_empty() => Some(GuestGpu::Off { reason: rest }),
        _ => None,
    }
}

fn read_desktop(line: &str) -> Option<GuestDesktop> {
    match said(line, DESKTOP_PREFIX)? {
        ("gpu", rest) if rest.is_empty() => Some(GuestDesktop::Gpu),
        ("plain", rest) if !rest.is_empty() => Some(GuestDesktop::Plain { reason: rest }),
        _ => None,
    }
}

/// What the newest boot said in what the serial port has had, if it said anything. A GPU line starts a
/// boot, so a desktop line counts only after one.
pub fn newest(serial: &[u8]) -> Option<GuestSays> {
    let mut found: Option<GuestSays> = None;
    for line in String::from_utf8_lossy(serial).lines() {
        if let Some(gpu) = read_gpu(line) {
            found = Some(GuestSays { gpu, desktop: None });
        } else if let (Some(desktop), Some(says)) = (read_desktop(line), found.as_mut()) {
            says.desktop = Some(desktop);
        }
    }
    found
}

/// What the newest boot said in a run folder's serial log. A guest that never wrote a line says nothing.
pub fn read(run_folder: &Path) -> Option<GuestSays> {
    newest(&serial_tail(run_folder)?)
}

/// The end of a run folder's serial log, where the newest boot's lines are.
pub(super) fn serial_tail(run_folder: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(run_folder.join(SERIAL_LOG)).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES))).ok()?;
    let mut tail = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
    Some(tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;

    #[test]
    fn the_newest_line_wins() {
        let serial =
            b"[  1.0] booting\nnvfwd-gpu off no nvfwd device.\nrebooting\nnvfwd-gpu on 615.78.08\r\nlogin:";
        assert_eq!(newest(serial).unwrap().gpu, GuestGpu::On { version: "615.78.08".into() });
        assert_eq!(
            newest(b"nvfwd-gpu on 615.78.08\nnvfwd-gpu off the build failed\n").unwrap().text(),
            "The guest has no GPU on this boot: the build failed"
        );
    }

    #[test]
    fn only_a_line_of_its_own_counts() {
        for serial in [
            &b""[..],
            b"echo nvfwd-gpu on 615.78.08\n",
            b"nvfwd-gpu on\n",
            b"nvfwd-gpu on 615 and more\n",
            b"nvfwd-gpu sideways 615\n",
            b"nvfwd-gpuon 615\n",
        ] {
            assert_eq!(newest(serial), None, "{}", String::from_utf8_lossy(serial));
        }
    }

    #[test]
    fn a_guest_cannot_send_the_row_anything_but_plain_words() {
        let mut serial = b"nvfwd-gpu off <b>bold</b>\x1b[31m red\x07 ".to_vec();
        serial.extend(std::iter::repeat_n(b'x', 1000));
        let Some(GuestGpu::Off { reason }) = newest(&serial).map(|says| says.gpu) else {
            panic!("it should be read")
        };
        assert!(reason.starts_with("bbold/b[31m red x"), "{reason}");
        assert_eq!(reason.len(), LONGEST_REASON);
    }

    #[test]
    fn a_desktop_line_says_where_this_boots_desktop_is() {
        let on_gpu = newest(b"nvfwd-gpu on 615.78.08\nnvfwd-desktop gpu\n").unwrap();
        assert_eq!(on_gpu.desktop, Some(GuestDesktop::Gpu));
        assert!(!on_gpu.plain_desktop());
        assert!(on_gpu.text().ends_with("and its desktop is on it."), "{}", on_gpu.text());
        let back =
            newest(b"nvfwd-gpu on 615.78.08\nnvfwd-desktop gpu\nnvfwd-desktop plain it never lit\n").unwrap();
        assert!(back.plain_desktop());
        assert!(back.text().ends_with("Its desktop is on the plain screen: it never lit"), "{}", back.text());
    }

    #[test]
    fn a_new_boot_forgets_the_last_ones_desktop() {
        let serial =
            b"nvfwd-gpu on 615.78.08\nnvfwd-desktop plain it never lit\nreboot\nnvfwd-gpu on 615.78.08\n";
        let says = newest(serial).unwrap();
        assert_eq!(says.desktop, None);
        assert!(!says.plain_desktop(), "the new boot may still bring it up on the GPU");
        assert!(newest(b"nvfwd-desktop gpu\n").is_none(), "no boot has said anything");
    }

    #[test]
    fn with_no_gpu_the_desktop_is_plain_and_odd_desktop_lines_are_left_out() {
        assert!(newest(b"nvfwd-gpu off no nvfwd device.\n").unwrap().plain_desktop());
        for line in ["nvfwd-desktop plain", "nvfwd-desktop gpu now", "nvfwd-desktop sideways"] {
            let serial = format!("nvfwd-gpu on 615.78.08\n{line}\n");
            assert_eq!(newest(serial.as_bytes()).unwrap().desktop, None, "{line}");
        }
    }

    #[test]
    fn only_the_end_of_a_long_log_is_read() {
        let scratch = Scratch::new();
        let mut log = b"nvfwd-gpu on 615.78.08\n".to_vec();
        log.extend(std::iter::repeat_n(b'.', TAIL_BYTES as usize));
        std::fs::write(scratch.path("serial.log"), &log).unwrap();
        assert_eq!(read(&scratch.path("")), None);
        log.extend(b"\nnvfwd-gpu off the host gave no image\n");
        std::fs::write(scratch.path("serial.log"), &log).unwrap();
        let off = GuestGpu::Off { reason: "the host gave no image".into() };
        assert_eq!(read(&scratch.path("")).map(|says| says.gpu), Some(off));
        assert_eq!(read(&scratch.path("nowhere")), None);
    }
}
