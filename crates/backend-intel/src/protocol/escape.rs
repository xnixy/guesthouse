//! A driver escape: the guest's driver talking to its kernel driver directly.
//!
//! Every escape starts with a 16-byte header, GFX_ESCAPE_HEADER: the payload's size, a checksum,
//! the escape code, and a minor code or a reserved word. The kernel driver checks the size against
//! the header, except for a few codes, and the code against its table. It never checks the
//! checksum. The memory manager's escapes all use [`GMM_CONTROL`], with what they ask for in the
//! payload's first dword.
//!
//! No escape is answered in this stage: the guest gets its buffer back as it sent it. The core
//! gives an escape no status of its own, so even one the kernel driver would refuse comes back
//! looking answered. What each one was, and whether the kernel driver would have taken it, goes to
//! the log.

use super::bytes::dword;

pub const HEADER_SIZE: usize = 16;
const PAYLOAD_SIZE: usize = 0x0;
const CHECKSUM: usize = 0x4;
const CODE: usize = 0x8;
const MINOR_CODE: usize = 0xC;
/// Codes the kernel driver has a table entry for are below this.
const CODES: u32 = 0x70;
/// Codes the kernel driver lets through with any size.
const ANY_SIZE: [u32; 4] = [0x01, 0x14, 0x15, 0x6A];
/// GFX_ESCAPE_GMM_CONTROL, from compute-runtime.
pub const GMM_CONTROL: u32 = 2;
/// A payload starting with these four letters is answered as done at once.
const DONE_AT_ONCE: [u8; 4] = *b"GOOG";
/// An escape this long that fails the checks goes to an older handler instead.
const OLDER_FORM_SIZE: usize = 0x30;

/// What the kernel driver would make of an escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    Taken,
    /// Answered as done without looking further.
    DoneAtOnce,
    /// Not taken as an escape of this kind, but by the older handler, whose rules aren't known.
    OlderForm,
    Refused(&'static str),
}

impl Verdict {
    /// One word for the log.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Taken => "take",
            Self::DoneAtOnce => "answer-at-once",
            Self::OlderForm => "pass-to-older-handler",
            Self::Refused(_) => "refuse",
        }
    }

    /// Why it would be refused, or `-`.
    pub fn why(&self) -> &'static str {
        if let Self::Refused(why) = self { why } else { "-" }
    }
}

/// What an escape says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Escape {
    pub length: usize,
    pub payload_size: u32,
    pub checksum: u32,
    pub code: u32,
    pub minor_code: u32,
    /// What a memory manager escape asks for.
    pub gmm_action: Option<u32>,
    pub verdict: Verdict,
}

impl Escape {
    pub fn parse(data: &[u8]) -> Self {
        let field = |at| dword(data, at).unwrap_or(0);
        let mut escape = Self {
            length: data.len(),
            payload_size: field(PAYLOAD_SIZE),
            checksum: field(CHECKSUM),
            code: field(CODE),
            minor_code: field(MINOR_CODE),
            gmm_action: None,
            verdict: Verdict::Taken,
        };
        let payload = data.get(HEADER_SIZE..).unwrap_or_default();
        if escape.code == GMM_CONTROL {
            escape.gmm_action = dword(payload, 0);
        }
        escape.verdict = escape.judge(payload);
        escape
    }

    fn judge(&self, payload: &[u8]) -> Verdict {
        let refused = if self.length < HEADER_SIZE {
            Some("shorter than its header")
        } else if self.code >= CODES {
            Some("a code past the kernel driver's table")
        } else if !ANY_SIZE.contains(&self.code)
            && self.length as u64 != u64::from(self.payload_size) + HEADER_SIZE as u64
        {
            Some("its size isn't the header's and the header")
        } else {
            None
        };
        match refused {
            Some(_) if self.length == OLDER_FORM_SIZE => Verdict::OlderForm,
            Some(why) => Verdict::Refused(why),
            None if payload.starts_with(&DONE_AT_ONCE) => Verdict::DoneAtOnce,
            None => Verdict::Taken,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::bytes::put_dword;

    /// A memory manager escape asking for `action`, with `payload` bytes after the header.
    pub fn gmm(action: u32, payload: usize) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_SIZE + payload];
        put_dword(&mut data, PAYLOAD_SIZE, payload as u32);
        put_dword(&mut data, CODE, GMM_CONTROL);
        put_dword(&mut data, HEADER_SIZE, action);
        data
    }

    #[test]
    fn a_memory_manager_escape_is_taken_and_says_what_it_asks() {
        let escape = Escape::parse(&gmm(0x1D, 0xC0));
        assert_eq!(
            (escape.code, escape.gmm_action, escape.verdict),
            (GMM_CONTROL, Some(0x1D), Verdict::Taken)
        );
        assert_eq!(escape.length, HEADER_SIZE + 0xC0);
    }

    #[test]
    fn the_checksum_is_read_but_never_judged() {
        let mut data = gmm(0x0F, 0x10);
        put_dword(&mut data, CHECKSUM, 0xDEAD_BEEF);
        let escape = Escape::parse(&data);
        assert_eq!((escape.checksum, escape.verdict), (0xDEAD_BEEF, Verdict::Taken));
    }

    #[test]
    fn a_size_that_disagrees_with_the_header_is_refused_except_for_a_few_codes() {
        let mut data = gmm(0x0F, 0x10);
        put_dword(&mut data, PAYLOAD_SIZE, 0x20);
        assert!(matches!(Escape::parse(&data).verdict, Verdict::Refused(_)));
        for code in ANY_SIZE {
            put_dword(&mut data, CODE, code);
            assert_eq!(Escape::parse(&data).verdict, Verdict::Taken, "{code:#x}");
        }
    }

    #[test]
    fn short_escapes_and_codes_past_the_table_are_refused() {
        assert!(matches!(Escape::parse(&[0; 8]).verdict, Verdict::Refused(_)));
        let mut data = gmm(0, 0x10);
        put_dword(&mut data, CODE, CODES);
        assert!(matches!(Escape::parse(&data).verdict, Verdict::Refused(_)));
        assert_eq!(Escape::parse(&data).gmm_action, None);
    }

    #[test]
    fn a_refused_escape_of_the_older_size_goes_to_the_older_handler() {
        let mut data = vec![0u8; OLDER_FORM_SIZE];
        put_dword(&mut data, CODE, CODES + 1);
        assert_eq!(Escape::parse(&data).verdict, Verdict::OlderForm);
    }

    #[test]
    fn the_marker_payload_is_done_at_once() {
        let mut data = vec![0u8; HEADER_SIZE + 8];
        put_dword(&mut data, PAYLOAD_SIZE, 8);
        put_dword(&mut data, CODE, 0x10);
        data[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&DONE_AT_ONCE);
        assert_eq!(Escape::parse(&data).verdict, Verdict::DoneAtOnce);
    }
}
