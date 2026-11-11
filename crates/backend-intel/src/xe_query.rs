//! xe's read-only device queries, and what their answers say.
//!
//! A query is asked twice: once with no buffer, which gives the size, and again with a buffer of
//! that size. The answers are plain arrays of fixed-size entries, laid out as `xe_drm.h` gives
//! them, and are read here by offset. Every parser works on bytes, so it is tested without a card.

use crate::ioctl::{DEVICE_QUERY_SIZE, device_query, drm_ioctl};
use crate::protocol::bytes::{dword, qword, word};
use std::io;
use std::os::fd::AsRawFd;

/// The kinds of query, DRM_XE_DEVICE_QUERY_*.
pub mod kind {
    pub const ENGINES: u32 = 0;
    pub const MEMORY_REGIONS: u32 = 1;
    pub const CONFIG: u32 = 2;
    pub const GT_LIST: u32 = 3;
    pub const HWCONFIG: u32 = 4;
    pub const GT_TOPOLOGY: u32 = 5;
    pub const FIRMWARE_VERSION: u32 = 7;
}

/// Where the request's fields are.
const QUERY_KIND: usize = 8;
const QUERY_SIZE: usize = 12;
const QUERY_DATA: usize = 16;

/// Every answer that is a list starts with its count, then a dword of padding.
const LIST_HEADER: usize = 8;

/// Asks one query and returns its answer.
pub fn ask(file: &impl AsRawFd, kind: u32) -> io::Result<Vec<u8>> {
    let mut request = [0u8; DEVICE_QUERY_SIZE];
    request[QUERY_KIND..QUERY_KIND + 4].copy_from_slice(&kind.to_le_bytes());
    // SAFETY: with no buffer given, the request holds no pointer, and xe only writes the size.
    unsafe { drm_ioctl(file, device_query(), &mut request)? };
    let size = dword(&request, QUERY_SIZE).unwrap_or(0) as usize;
    let mut answer = vec![0u8; size];
    ask_into(file, kind, &mut answer)?;
    Ok(answer)
}

/// Asks a query whose buffer also carries what is asked, with the buffer's size given.
pub fn ask_into(file: &impl AsRawFd, kind: u32, buffer: &mut [u8]) -> io::Result<()> {
    let mut request = [0u8; DEVICE_QUERY_SIZE];
    request[QUERY_KIND..QUERY_KIND + 4].copy_from_slice(&kind.to_le_bytes());
    request[QUERY_SIZE..QUERY_SIZE + 4].copy_from_slice(&(buffer.len() as u32).to_le_bytes());
    let data = buffer.as_mut_ptr() as u64;
    request[QUERY_DATA..QUERY_DATA + 8].copy_from_slice(&data.to_le_bytes());
    // SAFETY: the pointer is `buffer`'s, which is borrowed mutably for the call and as long as the
    // size the request gives.
    unsafe { drm_ioctl(file, device_query(), &mut request) }
}

/// The entries of a list answer, each `entry` bytes, as many as its count says and the answer holds.
fn entries(answer: &[u8], entry: usize) -> impl Iterator<Item = &[u8]> {
    let count = dword(answer, 0).unwrap_or(0) as usize;
    answer.get(LIST_HEADER..).unwrap_or_default().chunks_exact(entry).take(count)
}

/// The device's configuration, DRM_XE_DEVICE_QUERY_CONFIG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Config {
    pub device_id: u16,
    pub revision: u8,
    pub flags: u64,
    pub min_alignment: u64,
    pub va_bits: u64,
    pub max_queue_priority: u64,
}

impl Config {
    /// The card has memory of its own, DRM_XE_QUERY_CONFIG_FLAG_HAS_VRAM.
    pub const HAS_VRAM: u64 = 1 << 0;
    const REVISION_AND_DEVICE: usize = 0;
    const FLAGS: usize = 1;
    const MIN_ALIGNMENT: usize = 2;
    const VA_BITS: usize = 3;
    const MAX_QUEUE_PRIORITY: usize = 4;
    const REVISION_SHIFT: u32 = 16;

    pub fn parse(answer: &[u8]) -> Self {
        let count = dword(answer, 0).unwrap_or(0) as usize;
        let info = |index: usize| {
            if index < count { qword(answer, LIST_HEADER + index * 8).unwrap_or(0) } else { 0 }
        };
        let identity = info(Self::REVISION_AND_DEVICE);
        Self {
            device_id: identity as u16,
            revision: (identity >> Self::REVISION_SHIFT) as u8,
            flags: info(Self::FLAGS),
            min_alignment: info(Self::MIN_ALIGNMENT),
            va_bits: info(Self::VA_BITS),
            max_queue_priority: info(Self::MAX_QUEUE_PRIORITY),
        }
    }

    pub fn has_vram(&self) -> bool {
        self.flags & Self::HAS_VRAM != 0
    }
}

/// A memory region, from DRM_XE_DEVICE_QUERY_MEM_REGIONS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    pub class: u16,
    pub instance: u16,
    pub min_page_size: u32,
    pub total: u64,
    pub used: u64,
    pub cpu_visible: u64,
}

impl MemoryRegion {
    /// DRM_XE_MEM_REGION_CLASS_SYSMEM.
    pub const SYSTEM: u16 = 0;
    const ENTRY: usize = 88;

    pub fn parse_all(answer: &[u8]) -> Vec<Self> {
        entries(answer, Self::ENTRY)
            .map(|entry| Self {
                class: word(entry, 0).unwrap_or(0),
                instance: word(entry, 2).unwrap_or(0),
                min_page_size: dword(entry, 4).unwrap_or(0),
                total: qword(entry, 8).unwrap_or(0),
                used: qword(entry, 16).unwrap_or(0),
                cpu_visible: qword(entry, 24).unwrap_or(0),
            })
            .collect()
    }
}

/// A hardware engine, from DRM_XE_DEVICE_QUERY_ENGINES.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Engine {
    pub class: u16,
    pub instance: u16,
    pub gt: u16,
}

impl Engine {
    pub const RENDER: u16 = 0;
    pub const COPY: u16 = 1;
    pub const VIDEO_DECODE: u16 = 2;
    pub const VIDEO_ENHANCE: u16 = 3;
    pub const COMPUTE: u16 = 4;
    const ENTRY: usize = 32;

    pub fn parse_all(answer: &[u8]) -> Vec<Self> {
        entries(answer, Self::ENTRY)
            .map(|entry| Self {
                class: word(entry, 0).unwrap_or(0),
                instance: word(entry, 2).unwrap_or(0),
                gt: word(entry, 4).unwrap_or(0),
            })
            .collect()
    }

    /// The short name the kernel gives this engine's class.
    pub fn class_name(&self) -> &'static str {
        match self.class {
            Self::RENDER => "rcs",
            Self::COPY => "bcs",
            Self::VIDEO_DECODE => "vcs",
            Self::VIDEO_ENHANCE => "vecs",
            Self::COMPUTE => "ccs",
            _ => "other",
        }
    }
}

/// A graphics or media IP's version, as xe reports it for each GT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub struct IpVersion {
    pub major: u16,
    pub minor: u16,
    pub revision: u16,
}

impl std::fmt::Display for IpVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02} rev {}", self.major, self.minor, self.revision)
    }
}

/// A GT, from DRM_XE_DEVICE_QUERY_GT_LIST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gt {
    pub kind: u16,
    pub tile: u16,
    pub id: u16,
    /// The timestamp's clock, in Hz.
    pub reference_clock: u32,
    pub near_memory: u64,
    pub far_memory: u64,
    pub ip: IpVersion,
}

impl Gt {
    /// DRM_XE_QUERY_GT_TYPE_MAIN: graphics and compute.
    pub const MAIN: u16 = 0;
    /// DRM_XE_QUERY_GT_TYPE_MEDIA.
    pub const MEDIA: u16 = 1;
    const ENTRY: usize = 96;

    pub fn parse_all(answer: &[u8]) -> Vec<Self> {
        entries(answer, Self::ENTRY)
            .map(|entry| Self {
                kind: word(entry, 0).unwrap_or(0),
                tile: word(entry, 2).unwrap_or(0),
                id: word(entry, 4).unwrap_or(0),
                reference_clock: dword(entry, 12).unwrap_or(0),
                near_memory: qword(entry, 16).unwrap_or(0),
                far_memory: qword(entry, 24).unwrap_or(0),
                ip: IpVersion {
                    major: word(entry, 32).unwrap_or(0),
                    minor: word(entry, 34).unwrap_or(0),
                    revision: word(entry, 36).unwrap_or(0),
                },
            })
            .collect()
    }
}

/// One mask of the GT topology, from DRM_XE_DEVICE_QUERY_GT_TOPOLOGY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyMask {
    pub gt: u16,
    pub kind: u16,
    pub mask: Vec<u8>,
}

impl TopologyMask {
    pub const DSS_GEOMETRY: u16 = 1;
    pub const DSS_COMPUTE: u16 = 2;
    pub const L3_BANK: u16 = 3;
    pub const EU_PER_DSS: u16 = 4;
    pub const SIMD16_EU_PER_DSS: u16 = 5;
    const HEADER: usize = 8;

    /// The masks follow one another, each its header and then as many bytes as it says.
    pub fn parse_all(answer: &[u8]) -> Vec<Self> {
        let mut masks = Vec::new();
        let mut at = 0;
        while let (Some(gt), Some(kind), Some(length)) =
            (word(answer, at), word(answer, at + 2), dword(answer, at + 4))
        {
            let start = at + Self::HEADER;
            let Some(mask) = answer.get(start..start + length as usize) else { break };
            masks.push(Self { gt, kind, mask: mask.to_vec() });
            at = start + length as usize;
        }
        masks
    }

    pub fn bits_set(&self) -> u32 {
        self.mask.iter().map(|byte| byte.count_ones()).sum()
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            Self::DSS_GEOMETRY => "dss-geometry",
            Self::DSS_COMPUTE => "dss-compute",
            Self::L3_BANK => "l3-bank",
            Self::EU_PER_DSS => "eu-per-dss",
            Self::SIMD16_EU_PER_DSS => "simd16-eu-per-dss",
            _ => "other",
        }
    }
}

/// A firmware's version, from DRM_XE_DEVICE_QUERY_UC_FW_VERSION.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FirmwareVersion {
    pub branch: u32,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl FirmwareVersion {
    /// XE_QUERY_UC_TYPE_GUC_SUBMISSION and XE_QUERY_UC_TYPE_HUC.
    pub const GUC: u16 = 0;
    pub const HUC: u16 = 1;
    pub const QUERY_SIZE: usize = 32;

    /// The buffer that asks for one firmware.
    pub fn request(firmware: u16) -> [u8; Self::QUERY_SIZE] {
        let mut buffer = [0; Self::QUERY_SIZE];
        buffer[..2].copy_from_slice(&firmware.to_le_bytes());
        buffer
    }

    pub fn parse(answer: &[u8]) -> Self {
        Self {
            branch: dword(answer, 4).unwrap_or(0),
            major: dword(answer, 8).unwrap_or(0),
            minor: dword(answer, 12).unwrap_or(0),
            patch: dword(answer, 16).unwrap_or(0),
        }
    }
}

impl std::fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.branch, self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::bytes::{put_dword, put_qword, put_word};

    /// A list answer of `count` entries of `entry` bytes, each filled in by `fill`.
    pub fn list(count: usize, entry: usize, fill: impl Fn(usize, &mut [u8])) -> Vec<u8> {
        let mut answer = vec![0u8; LIST_HEADER + count * entry];
        put_dword(&mut answer, 0, count as u32);
        for index in 0..count {
            let start = LIST_HEADER + index * entry;
            fill(index, &mut answer[start..start + entry]);
        }
        answer
    }

    #[test]
    fn the_config_gives_the_device_its_revision_and_its_flags() {
        let answer = list(5, 8, |index, entry| {
            let value = [4 << 16 | 0x64A0, Config::HAS_VRAM, 0x1000, 48, 2][index];
            put_qword(entry, 0, value);
        });
        let config = Config::parse(&answer);
        assert_eq!((config.device_id, config.revision), (0x64A0, 4));
        assert!(config.has_vram());
        assert_eq!((config.min_alignment, config.va_bits, config.max_queue_priority), (0x1000, 48, 2));
    }

    #[test]
    fn a_short_config_leaves_the_rest_zero() {
        let answer = list(1, 8, |_, entry| put_qword(entry, 0, 0x64A0));
        assert_eq!(Config::parse(&answer), Config { device_id: 0x64A0, ..Config::default() });
    }

    #[test]
    fn regions_engines_and_gts_are_read_by_their_entry_size() {
        let regions = list(1, MemoryRegion::ENTRY, |_, entry| {
            put_dword(entry, 4, 0x1000);
            put_qword(entry, 8, 32 << 30);
        });
        assert_eq!(MemoryRegion::parse_all(&regions)[0].total, 32 << 30);
        let engines = list(2, Engine::ENTRY, |index, entry| {
            put_word(entry, 0, [Engine::RENDER, Engine::COMPUTE][index]);
            put_word(entry, 4, 0);
        });
        let classes: Vec<_> = Engine::parse_all(&engines).iter().map(Engine::class_name).collect();
        assert_eq!(classes, ["rcs", "ccs"]);
        let gts = list(1, Gt::ENTRY, |_, entry| {
            put_dword(entry, 12, 19_200_000);
            put_word(entry, 32, 20);
            put_word(entry, 34, 4);
        });
        let gt = Gt::parse_all(&gts)[0];
        assert_eq!(gt.reference_clock, 19_200_000);
        assert_eq!(gt.ip.to_string(), "20.04 rev 0");
    }

    #[test]
    fn a_count_past_the_answer_stops_at_its_end() {
        let mut answer = list(1, Engine::ENTRY, |_, _| {});
        put_dword(&mut answer, 0, 5);
        assert_eq!(Engine::parse_all(&answer).len(), 1);
    }

    #[test]
    fn topology_masks_follow_one_another() {
        let mut answer = Vec::new();
        for (kind, mask) in
            [(TopologyMask::DSS_GEOMETRY, vec![0xFF, 0]), (TopologyMask::EU_PER_DSS, vec![0x0F])]
        {
            answer.extend_from_slice(&0u16.to_le_bytes());
            answer.extend_from_slice(&kind.to_le_bytes());
            answer.extend_from_slice(&(mask.len() as u32).to_le_bytes());
            answer.extend_from_slice(&mask);
        }
        let masks = TopologyMask::parse_all(&answer);
        assert_eq!(masks.len(), 2);
        assert_eq!((masks[0].name(), masks[0].bits_set()), ("dss-geometry", 8));
        assert_eq!((masks[1].name(), masks[1].bits_set()), ("eu-per-dss", 4));
        answer.truncate(answer.len() - 1);
        assert_eq!(TopologyMask::parse_all(&answer).len(), 1, "a mask cut short is dropped");
    }

    #[test]
    fn a_firmware_version_is_asked_by_its_kind() {
        assert_eq!(FirmwareVersion::request(FirmwareVersion::HUC)[..2], [1, 0]);
        let mut answer = [0u8; FirmwareVersion::QUERY_SIZE];
        for (at, value) in [(8, 70), (12, 36), (16, 0)] {
            put_dword(&mut answer, at, value);
        }
        assert_eq!(FirmwareVersion::parse(&answer).to_string(), "0.70.36.0");
    }
}
