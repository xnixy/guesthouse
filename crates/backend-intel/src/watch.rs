//! What the log says about the guest's traffic, and how it stays cheap.
//!
//! This stage runs nothing on the card. Its use is the log a tester sends back, so every kind of
//! vendor exchange gets a line at info level, each starting with [`PREFIX`] and a word for the kind,
//! so the log can be searched. A busy guest repeats the same kinds of exchange thousands of times,
//! so [`Seen`] lets the first few of each kind through at info, and the rest go to debug, where
//! `GUESTHOUSE_VERBOSE` shows them. With it, every buffer is dumped too, up to [`DUMP_BYTES`].
//!
//! The message path must not allocate, so the views here write straight into the log line, and
//! [`Seen`] takes all the room it will ever have when it is made.

use std::collections::HashMap;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};

/// The start of every line this backend logs about the guest.
pub const PREFIX: &str = "intel";

/// How many times one kind of exchange is logged at info before it goes to debug.
const INFO_REPEATS: u32 = 3;
/// How many kinds are told apart. Past this every new kind counts as seen, which only moves its
/// lines to debug.
const KINDS_KEPT: usize = 1024;
/// The most a dump shows of one buffer.
pub const DUMP_BYTES: usize = 4096;
/// The most non-zero words a line lists from what isn't decoded.
pub const UNDECODED_WORDS: usize = 12;

/// Kinds of exchange already logged, by a hash of what tells them apart, and how often each was.
#[derive(Debug)]
pub struct Seen {
    counts: HashMap<u64, u32>,
}

impl Default for Seen {
    fn default() -> Self {
        Self { counts: HashMap::with_capacity(KINDS_KEPT) }
    }
}

impl Seen {
    /// Whether this exchange still gets an info line. `kind` is anything that tells it apart.
    pub fn worth_telling(&mut self, kind: impl Hash) -> bool {
        let mut hasher = DefaultHasher::new();
        kind.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(count) = self.counts.get_mut(&key) {
            *count = count.saturating_add(1);
            return *count <= INFO_REPEATS;
        }
        if self.counts.len() >= KINDS_KEPT {
            return false;
        }
        self.counts.insert(key, 1);
        true
    }
}

/// Logs at info if `telling`, and at debug otherwise.
macro_rules! tell {
    ($telling:expr, $($line:tt)+) => {
        if $telling {
            log::info!($($line)+);
        } else {
            log::debug!($($line)+);
        }
    };
}
pub(crate) use tell;

/// The first bytes of a buffer in hex, and how many more there were.
pub struct Hex<'a> {
    pub bytes: &'a [u8],
    pub most: usize,
}

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = &self.bytes[..self.bytes.len().min(self.most)];
        for byte in shown {
            write!(f, "{byte:02x}")?;
        }
        let left = self.bytes.len() - shown.len();
        if left > 0 {
            write!(f, "..(+{left})")?;
        }
        Ok(())
    }
}

/// The non-zero dwords of a buffer, with the bytes already decoded counted as zero, as
/// `offset:value`, up to [`UNDECODED_WORDS`] of them. A line can then show what a block holds that
/// nothing names yet. Decoded ranges are byte ranges, end excluded.
pub struct Undecoded<'a> {
    pub bytes: &'a [u8],
    pub decoded: &'a [(usize, usize)],
}

impl Undecoded<'_> {
    fn is_decoded(&self, at: usize) -> bool {
        self.decoded.iter().any(|&(start, end)| (start..end).contains(&at))
    }
}

impl fmt::Display for Undecoded<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut shown = 0;
        let mut more = 0;
        for (index, chunk) in self.bytes.chunks(4).enumerate() {
            let at = index * 4;
            let mut word = [0; 4];
            for (offset, byte) in chunk.iter().enumerate() {
                if !self.is_decoded(at + offset) {
                    word[offset] = *byte;
                }
            }
            let word = u32::from_le_bytes(word);
            if word == 0 {
                continue;
            }
            if shown == UNDECODED_WORDS {
                more += 1;
                continue;
            }
            let space = if shown == 0 { "" } else { " " };
            write!(f, "{space}{at:#x}:{word:#x}")?;
            shown += 1;
        }
        match (shown, more) {
            (0, _) => f.write_str("none"),
            (_, 0) => Ok(()),
            _ => write!(f, " (+{more} more)"),
        }
    }
}

/// Logs a buffer at debug, whole up to [`DUMP_BYTES`], on one line under a label, with its length.
/// Nothing is done unless debug lines are on, as with `GUESTHOUSE_VERBOSE`.
pub fn dump(label: fmt::Arguments<'_>, bytes: &[u8]) {
    log::debug!("{PREFIX} dump {label} length={} bytes={}", bytes.len(), Hex { bytes, most: DUMP_BYTES });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_is_told_a_few_times_and_then_not() {
        let mut seen = Seen::default();
        let told: Vec<bool> = (0..5).map(|_| seen.worth_telling(("query", 7u32))).collect();
        assert_eq!(told, [true, true, true, false, false]);
        assert!(seen.worth_telling(("query", 8u32)), "another kind starts again");
    }

    #[test]
    fn past_its_room_a_new_kind_goes_to_debug() {
        let mut seen = Seen::default();
        for kind in 0..KINDS_KEPT {
            assert!(seen.worth_telling(kind));
        }
        assert!(!seen.worth_telling(KINDS_KEPT));
        assert!(seen.worth_telling(0usize), "a kind already kept still counts its repeats");
    }

    #[test]
    fn a_dump_is_cut_short_and_says_by_how_much() {
        assert_eq!(Hex { bytes: &[1, 0xAB, 3], most: 2 }.to_string(), "01ab..(+1)");
        assert_eq!(Hex { bytes: &[], most: 2 }.to_string(), "");
    }

    #[test]
    fn undecoded_words_skip_zeros_and_what_is_decoded() {
        let mut bytes = vec![0u8; 0x20];
        bytes[0] = 1;
        bytes[0x8] = 2;
        bytes[0x1C] = 3;
        let view = Undecoded { bytes: &bytes, decoded: &[(0, 4)] };
        assert_eq!(view.to_string(), "0x8:0x2 0x1c:0x3");
        assert_eq!(Undecoded { bytes: &[0; 8], decoded: &[] }.to_string(), "none");
    }

    #[test]
    fn a_byte_beside_a_decoded_one_still_shows() {
        let bytes = [0xAA, 0, 0xBB, 0];
        assert_eq!(Undecoded { bytes: &bytes, decoded: &[(0, 1)] }.to_string(), "0x0:0xbb0000");
    }

    #[test]
    fn a_long_list_of_undecoded_words_is_cut_short() {
        let bytes = vec![1u8; 4 * (UNDECODED_WORDS + 2)];
        let text = Undecoded { bytes: &bytes, decoded: &[] }.to_string();
        assert!(text.ends_with(" (+2 more)"), "{text}");
    }
}
