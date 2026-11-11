//! A submission's private data: the command buffer header, COMMAND_BUFFER_HEADER.
//!
//! This backend knows two sizes for it: a compact one of [`COMPACT_SIZE`] bytes, whose fields are
//! read here, and a full one of [`FULL_SIZE`], whose layout isn't known and is only logged. Any
//! other size is one the vendor's kernel driver wouldn't take, which the log says. The batch's
//! address and length are not in the header: they come with the submission itself.
//!
//! In this stage nothing runs, so a header never fails a submission: it is read from the front
//! when there are bytes enough, and the log says how long the data was and what the vendor's kernel
//! driver would have made of that length. The core passes a hardware queue's data to the end of
//! the message, so that length can be longer than what the guest meant.

use super::bytes::{bits, dword, qword, word};

pub const COMPACT_SIZE: usize = 0xB8;
pub const FULL_SIZE: usize = 0x898;

/// Where the compact header's fields are. Names are compute-runtime's, where it has one.
mod at {
    /// The low half of the first dword, a stamp nothing here checks.
    pub const STAMP: usize = 0x00;
    /// The rest of the first dword: the context type and some flags.
    pub const FIRST: usize = 0x00;
    /// Slice, subslice and EU requests, and two fields not known yet.
    pub const REQUESTS: usize = 0x04;
    pub const FLAGS: usize = 0x08;
    pub const TAG_ENABLE: usize = 0x0C;
    pub const TAG_ADDRESS: usize = 0x10;
    pub const TAG_VALUE: usize = 0x18;
    pub const PERF_TAG: usize = 0x1C;
    pub const MONITOR_FENCE_ADDRESS: usize = 0x30;
    pub const MONITOR_FENCE_VALUE: usize = 0x38;
}

/// The first dword's fields, as (lowest bit, width).
const CONTEXT_TYPE: (u32, u32) = (16, 6);
const PATCH_LIST: (u32, u32) = (22, 1);
/// Five flags not known yet.
const FIRST_FLAGS: (u32, u32) = (23, 5);
/// The requests dword's fields.
const REQUEST_LOW: (u32, u32) = (0, 10);
const REQUEST_HIGH: (u32, u32) = (10, 10);
const SLICE_STATE: (u32, u32) = (20, 3);
const REQUEST_BIT_23: (u32, u32) = (23, 1);
const SUBSLICE_COUNT: (u32, u32) = (24, 3);
const EU_COUNT: (u32, u32) = (27, 5);

/// The flags dword's bits that have names.
pub mod flag {
    pub const USES_RESOURCE_STREAMER: u32 = 1 << 1;
    pub const MID_BATCH_PREEMPTION: u32 = 1 << 2;
    pub const USES_GPGPU_PIPELINE: u32 = 1 << 5;
    pub const REQUIRES_COHERENCY: u32 = 1 << 6;
    pub const NAMED: u32 =
        USES_RESOURCE_STREAMER | MID_BATCH_PREEMPTION | USES_GPGPU_PIPELINE | REQUIRES_COHERENCY;
}

/// The ranges [`Compact`] reads.
pub const DECODED: [(usize, usize); 3] = [
    (at::STAMP, at::FLAGS + 4),
    (at::TAG_ENABLE, at::PERF_TAG + 4),
    (at::MONITOR_FENCE_ADDRESS, at::MONITOR_FENCE_VALUE + 8),
];

/// The ranges that change from one submission to the next, which don't make it a new kind.
const CHANGING: [(usize, usize); 2] =
    [(at::TAG_ADDRESS, at::TAG_VALUE + 4), (at::MONITOR_FENCE_VALUE, at::MONITOR_FENCE_VALUE + 8)];

/// What the vendor's kernel driver would make of the data's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Length {
    Compact,
    Full,
    /// Neither size: the vendor's kernel driver wouldn't take it.
    Neither,
}

impl Length {
    pub fn word(&self) -> &'static str {
        match self {
            Self::Compact => "take-compact",
            Self::Full => "take-full",
            Self::Neither => "refuse-size",
        }
    }
}

/// What a submission's data says, as far as it can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub length: usize,
    pub verdict: Length,
    /// The compact fields, read from the front whenever there are bytes enough.
    pub compact: Option<Compact>,
}

impl Header {
    pub fn read(data: &[u8]) -> Self {
        let verdict = match data.len() {
            COMPACT_SIZE => Length::Compact,
            FULL_SIZE => Length::Full,
            _ => Length::Neither,
        };
        let compact = data.get(..COMPACT_SIZE).map(Compact::parse);
        Self { length: data.len(), verdict, compact }
    }
}

/// The compact header's known fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Compact {
    pub stamp: u16,
    pub context_type: u32,
    pub patch_list: bool,
    pub first_flags: u32,
    pub request_low: u32,
    pub request_high: u32,
    pub slice_state: u32,
    pub request_bit_23: bool,
    pub subslice_count: u32,
    pub eu_count: u32,
    pub flags: u32,
    pub tag: Option<(u64, u32)>,
    pub perf_tag: u32,
    pub monitor_fence: Option<(u64, u64)>,
}

impl Compact {
    /// The flags no name is known for yet.
    pub fn unnamed_flags(&self) -> u32 {
        self.flags & !flag::NAMED
    }

    fn parse(data: &[u8]) -> Self {
        let first = dword(data, at::FIRST).unwrap_or(0);
        let requests = dword(data, at::REQUESTS).unwrap_or(0);
        let field = |value, (low, width)| bits(value, low, width);
        let tag_enabled = dword(data, at::TAG_ENABLE).unwrap_or(0) != 0;
        let fence_address = qword(data, at::MONITOR_FENCE_ADDRESS).unwrap_or(0);
        Self {
            stamp: word(data, at::STAMP).unwrap_or(0),
            context_type: field(first, CONTEXT_TYPE),
            patch_list: field(first, PATCH_LIST) != 0,
            first_flags: field(first, FIRST_FLAGS),
            request_low: field(requests, REQUEST_LOW),
            request_high: field(requests, REQUEST_HIGH),
            slice_state: field(requests, SLICE_STATE),
            request_bit_23: field(requests, REQUEST_BIT_23) != 0,
            subslice_count: field(requests, SUBSLICE_COUNT),
            eu_count: field(requests, EU_COUNT),
            flags: dword(data, at::FLAGS).unwrap_or(0),
            tag: tag_enabled.then(|| {
                (qword(data, at::TAG_ADDRESS).unwrap_or(0), dword(data, at::TAG_VALUE).unwrap_or(0))
            }),
            perf_tag: dword(data, at::PERF_TAG).unwrap_or(0),
            monitor_fence: (fence_address != 0)
                .then(|| (fence_address, qword(data, at::MONITOR_FENCE_VALUE).unwrap_or(0))),
        }
    }
}

/// A value that tells one kind of header from another, leaving out what changes with every
/// submission, so a busy queue's identical headers count as one kind. Only the header's bytes
/// count, so what follows it in the message doesn't make every submission new.
pub fn kind_of(data: &[u8]) -> impl std::hash::Hash + use<'_> {
    let header = &data[..data.len().min(FULL_SIZE)];
    let changing = |at: usize| CHANGING.iter().any(|&(start, end)| (start..end).contains(&at));
    let kept = header.iter().enumerate().map(move |(at, &byte)| if changing(at) { 0 } else { byte });
    HashedBytes(data.len(), kept)
}

/// Hashes the length, then the bytes an iterator gives, without collecting them.
struct HashedBytes<I>(usize, I);

impl<I: Iterator<Item = u8> + Clone> std::hash::Hash for HashedBytes<I> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
        self.1.clone().for_each(|byte| state.write_u8(byte));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::bytes::{put_dword, put_qword, put_word};
    use std::hash::{DefaultHasher, Hash, Hasher};

    /// A compact header with a few arbitrary fields set: a stamp, a context type, one named flag
    /// and a performance tag. All else is zero.
    pub fn compact() -> Vec<u8> {
        let mut data = vec![0u8; COMPACT_SIZE];
        put_word(&mut data, at::STAMP, 0x1234);
        put_dword(&mut data, at::FIRST, 3 << 16 | 0x1234);
        put_dword(&mut data, at::FLAGS, flag::MID_BATCH_PREEMPTION);
        put_dword(&mut data, at::PERF_TAG, 0x55);
        data
    }

    fn hashed(value: impl Hash) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    fn compact_of(data: &[u8]) -> Compact {
        Header::read(data).compact.expect("bytes enough for a compact header")
    }

    #[test]
    fn a_compact_header_reads_as_it_was_sent() {
        let header = Header::read(&compact());
        assert_eq!(header.verdict, Length::Compact);
        let fields = header.compact.unwrap();
        assert_eq!((fields.stamp, fields.context_type, fields.perf_tag), (0x1234, 3, 0x55));
        assert_eq!(fields.flags, flag::MID_BATCH_PREEMPTION);
        assert_eq!(fields.unnamed_flags(), 0);
        assert_eq!((fields.tag, fields.monitor_fence), (None, None));
        assert!(!fields.patch_list);
    }

    #[test]
    fn the_requests_dword_splits_into_its_fields() {
        let mut data = compact();
        put_dword(&mut data, at::REQUESTS, 31 << 27 | 4 << 24 | 1 << 23 | 5 << 20 | 0x3FF << 10 | 0x155);
        let fields = compact_of(&data);
        assert_eq!((fields.eu_count, fields.subslice_count, fields.slice_state), (31, 4, 5));
        assert!(fields.request_bit_23);
        assert_eq!((fields.request_high, fields.request_low), (0x3FF, 0x155));
    }

    #[test]
    fn a_tag_and_a_monitored_fence_show_when_set() {
        let mut data = compact();
        put_dword(&mut data, at::TAG_ENABLE, 1);
        put_qword(&mut data, at::TAG_ADDRESS, 0x1000);
        put_dword(&mut data, at::TAG_VALUE, 9);
        put_qword(&mut data, at::MONITOR_FENCE_ADDRESS, 0x2000);
        put_qword(&mut data, at::MONITOR_FENCE_VALUE, 3);
        let fields = compact_of(&data);
        assert_eq!((fields.tag, fields.monitor_fence), (Some((0x1000, 9)), Some((0x2000, 3))));
    }

    #[test]
    fn each_length_gets_its_verdict_and_a_long_one_is_still_read_from_the_front() {
        assert_eq!(Header::read(&vec![0; FULL_SIZE]).verdict, Length::Full);
        for size in [0, 0x18, COMPACT_SIZE - 1, COMPACT_SIZE + 1, FULL_SIZE + 8] {
            assert_eq!(Header::read(&vec![0; size]).verdict, Length::Neither, "{size:#x}");
        }
        let mut longer = compact();
        longer.extend_from_slice(&[0xEE; 0x40]);
        let header = Header::read(&longer);
        assert_eq!((header.verdict, header.length), (Length::Neither, COMPACT_SIZE + 0x40));
        assert_eq!(header.compact.unwrap().context_type, 3);
        assert_eq!(Header::read(&[0; 0x18]).compact, None, "too short to read");
    }

    #[test]
    fn headers_that_differ_only_in_their_fence_value_are_one_kind() {
        let mut later = compact();
        put_qword(&mut later, at::MONITOR_FENCE_VALUE, 99);
        assert_eq!(hashed(kind_of(&compact())), hashed(kind_of(&later)));
        let mut other = compact();
        put_dword(&mut other, at::FLAGS, 1 << 13);
        assert_ne!(hashed(kind_of(&compact())), hashed(kind_of(&other)));
    }
}
