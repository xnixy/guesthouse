//! A submission's private data: the command buffer header, COMMAND_BUFFER_HEADER.
//!
//! The kernel driver takes it in two forms, and no other size: a compact one of [`COMPACT_SIZE`]
//! bytes, which is what the guest's Direct3D 12 driver sends, and a full one of [`FULL_SIZE`],
//! whose layout isn't known yet and is only logged. The batch's address and length are not in the
//! header: they come with the submission itself.
//!
//! The compact header carries what the kernel driver would set up for this one submission: what it
//! is for, its slice and EU requests, preemption, and an optional value to write when it is done.
//! On this host the kernel's own driver does that its own way, so it is read here for the log, and
//! a header the kernel driver would refuse stops the queue as it would.

use super::bytes::{bits, dword, qword, word};

pub const COMPACT_SIZE: usize = 0xB8;
pub const FULL_SIZE: usize = 0x898;

/// Where the compact header's fields are. Names are compute-runtime's, where it has one.
mod at {
    /// The low half is a revision stamp the guest's driver writes; the kernel driver ignores it.
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
/// Five flags not known yet; the kernel driver reads the first of them.
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

/// The ranges [`Header::parse`] reads in a compact header.
pub const DECODED: [(usize, usize); 3] = [
    (at::STAMP, at::FLAGS + 4),
    (at::TAG_ENABLE, at::PERF_TAG + 4),
    (at::MONITOR_FENCE_ADDRESS, at::MONITOR_FENCE_VALUE + 8),
];

/// The ranges that change from one submission to the next, which don't make it a new kind.
const CHANGING: [(usize, usize); 2] =
    [(at::TAG_ADDRESS, at::TAG_VALUE + 4), (at::MONITOR_FENCE_VALUE, at::MONITOR_FENCE_VALUE + 8)];

/// Why the kernel driver would refuse this header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// What a header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    Compact(Compact),
    /// The full form, which is only logged.
    Full,
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
}

impl Header {
    pub fn parse(data: &[u8]) -> Result<Self, Refused> {
        match data.len() {
            COMPACT_SIZE => Ok(Self::Compact(Compact::parse(data))),
            FULL_SIZE => Ok(Self::Full),
            _ => Err(Refused("its private data is neither header size the kernel driver takes")),
        }
    }
}

impl Compact {
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
/// submission, so a busy queue's identical headers count as one kind.
pub fn kind_of(data: &[u8]) -> impl std::hash::Hash + use<'_> {
    let changing = |at: usize| CHANGING.iter().any(|&(start, end)| (start..end).contains(&at));
    let kept = data.iter().enumerate().map(move |(at, &byte)| if changing(at) { 0 } else { byte });
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

    /// What the guest's Direct3D 12 driver puts in a compact header: its stamp, the Direct3D 12
    /// context type, mid-batch preemption and its performance tag. All else is zero.
    pub fn direct3d_12() -> Vec<u8> {
        let mut data = vec![0u8; COMPACT_SIZE];
        put_word(&mut data, at::STAMP, 0x23D7);
        put_dword(&mut data, at::FIRST, 7 << 16 | 0x23D7);
        put_dword(&mut data, at::FLAGS, flag::MID_BATCH_PREEMPTION);
        put_dword(&mut data, at::PERF_TAG, 0x2000);
        data
    }

    fn hashed(value: impl Hash) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn a_direct3d_12_header_reads_as_it_was_sent() {
        let Ok(Header::Compact(header)) = Header::parse(&direct3d_12()) else { panic!("a compact header") };
        assert_eq!((header.stamp, header.context_type, header.perf_tag), (0x23D7, 7, 0x2000));
        assert_eq!(header.flags, flag::MID_BATCH_PREEMPTION);
        assert_eq!(header.unnamed_flags(), 0);
        assert_eq!((header.tag, header.monitor_fence), (None, None));
        assert!(!header.patch_list);
    }

    #[test]
    fn the_requests_dword_splits_into_its_fields() {
        let mut data = direct3d_12();
        put_dword(&mut data, at::REQUESTS, 31 << 27 | 4 << 24 | 1 << 23 | 5 << 20 | 0x3FF << 10 | 0x155);
        let Ok(Header::Compact(header)) = Header::parse(&data) else { panic!() };
        assert_eq!((header.eu_count, header.subslice_count, header.slice_state), (31, 4, 5));
        assert!(header.request_bit_23);
        assert_eq!((header.request_high, header.request_low), (0x3FF, 0x155));
    }

    #[test]
    fn a_tag_and_a_monitored_fence_show_when_set() {
        let mut data = direct3d_12();
        put_dword(&mut data, at::TAG_ENABLE, 1);
        put_qword(&mut data, at::TAG_ADDRESS, 0x1000);
        put_dword(&mut data, at::TAG_VALUE, 9);
        put_qword(&mut data, at::MONITOR_FENCE_ADDRESS, 0x2000);
        put_qword(&mut data, at::MONITOR_FENCE_VALUE, 3);
        let Ok(Header::Compact(header)) = Header::parse(&data) else { panic!() };
        assert_eq!((header.tag, header.monitor_fence), (Some((0x1000, 9)), Some((0x2000, 3))));
    }

    #[test]
    fn only_the_two_sizes_are_taken() {
        assert_eq!(Header::parse(&vec![0; FULL_SIZE]), Ok(Header::Full));
        for size in [0, 0x18, COMPACT_SIZE - 1, COMPACT_SIZE + 1, FULL_SIZE + 8] {
            assert!(Header::parse(&vec![0; size]).is_err(), "{size:#x}");
        }
    }

    #[test]
    fn headers_that_differ_only_in_their_fence_value_are_one_kind() {
        let mut later = direct3d_12();
        put_qword(&mut later, at::MONITOR_FENCE_VALUE, 99);
        assert_eq!(hashed(kind_of(&direct3d_12())), hashed(kind_of(&later)));
        let mut other = direct3d_12();
        put_dword(&mut other, at::FLAGS, 1 << 13);
        assert_ne!(hashed(kind_of(&direct3d_12())), hashed(kind_of(&other)));
    }
}
