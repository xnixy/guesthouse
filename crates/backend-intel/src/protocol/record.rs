//! The adapter record: the answer to the private adapter query, KMTQAITYPE_UMDRIVERPRIVATE.
//!
//! A four-byte ask gets the number of linked adapters, which is one, and an ask of [`RECORD_SIZE`]
//! bytes gets the record. Any other size is refused, as the vendor's kernel driver wouldn't take it.
//!
//! The record is built once, from the card and its generation: the platform and core families,
//! the card's ids, clocks and shape, the feature flags this backend sets, and the GPU address
//! partition. Compression is off, as the host can't set up compressed memory yet, and the byte that
//! marks the adapter as a virtual machine's is set. What is left at zero is listed in
//! [`super::gaps::RECORD`].

use super::bytes::{dword, put_dword, put_qword, put_word};
use super::features::{self, Feature};
use super::partition;
use guesthouse_backend::{Status, VendorQuery};

/// KMTQAITYPE_UMDRIVERPRIVATE.
pub const QUERY_TYPE: u32 = 0;
/// One adapter's record.
pub const RECORD_SIZE: usize = 0x2574;
/// The ask for how many linked adapters there are.
pub const COUNT_SIZE: usize = 4;
/// A paravirtual adapter is never linked to another.
const LINKED_ADAPTERS: u32 = 1;

/// Where the record's fields are. The names are those of gmmlib's ADAPTER_INFO and the structures
/// in it, though the record's order is not that header's.
mod at {
    /// A word the guest writes in its ask, which goes back as it came.
    pub const BUILD_STAMP: usize = 0x004;
    pub const PRODUCT_FAMILY: usize = 0x208;
    pub const PCH_FAMILY: usize = 0x20C;
    pub const DISPLAY_CORE: usize = 0x210;
    pub const RENDER_CORE: usize = 0x214;
    pub const PLATFORM_TYPE: usize = 0x218;
    pub const DEVICE_ID: usize = 0x21C;
    pub const REVISION: usize = 0x21E;
    pub const GT_TYPE: usize = 0x228;
    pub const RENDER_IP: usize = 0x238;
    pub const TIMESTAMP_FREQUENCY: usize = 0x998;
    pub const MIN_RENDER_FREQUENCY: usize = 0x9AC;
    pub const MAX_RENDER_FREQUENCY: usize = 0x9B0;
    pub const NUMBER_OF_EUS: usize = 0x9BC;
    pub const CACHE_LINE_SIZE: usize = 0x9E4;
    pub const DEDICATED_VIDEO_MEMORY: usize = 0x9F4;
    pub const SYSTEM_SHARED_MEMORY: usize = 0x9FC;
    pub const SYSTEM_VIDEO_MEMORY: usize = 0xA04;
    /// GT_SYSTEM_INFO.
    pub const EU_COUNT: usize = 0xB34;
    pub const THREAD_COUNT: usize = 0xB38;
    pub const SLICE_COUNT: usize = 0xB3C;
    pub const SUBSLICE_COUNT: usize = 0xB40;
    pub const DUAL_SUBSLICE_COUNT: usize = 0xB44;
    pub const MAX_EU_PER_SUBSLICE: usize = 0xB94;
    pub const MAX_SLICES: usize = 0xB98;
    pub const MAX_SUBSLICES: usize = 0xB9C;
    pub const MAX_DUAL_SUBSLICES: usize = 0xBA0;
    /// GMM_GFX_PARTITIONING, [`super::partition`].
    pub const PARTITION: usize = 0x23B8;
    /// A byte that marks the adapter as a virtual machine's.
    pub const FROM_VIRTUAL_MACHINE: usize = 0x2468;
    /// A byte that marks that machine as a secure one.
    pub const SECURE_VIRTUAL_MACHINE: usize = 0x2469;
}

/// The bytes a CPU cache line takes on the hosts this runs on.
const CACHE_LINE_BYTES: u32 = 64;

/// What the record says the platform is. Families and types are gmmlib's enums.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Platform {
    pub product_family: u32,
    pub pch_family: u32,
    pub display_core: u32,
    pub render_core: u32,
    pub platform_type: u32,
    pub device_id: u16,
    pub revision: u16,
    pub gt_type: u32,
    pub render_ip: IpWord,
}

/// An IP version as the record packs it: architecture in bits 22 to 31, release in 14 to 21 and
/// revision in 0 to 5, as compute-runtime's HardwareIpVersion lays it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IpWord(pub u32);

impl IpWord {
    const ARCHITECTURE_SHIFT: u32 = 22;
    const RELEASE_SHIFT: u32 = 14;
    const RELEASE_MASK: u32 = 0xFF;
    const REVISION_MASK: u32 = 0x3F;

    pub fn new(architecture: u16, release: u16, revision: u16) -> Self {
        Self(
            u32::from(architecture) << Self::ARCHITECTURE_SHIFT
                | (u32::from(release) & Self::RELEASE_MASK) << Self::RELEASE_SHIFT
                | u32::from(revision) & Self::REVISION_MASK,
        )
    }
}

/// The GPU's shape, as GT_SYSTEM_INFO counts it. Zero where the card didn't say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GtSystem {
    pub eu_count: u32,
    pub thread_count: u32,
    pub slice_count: u32,
    pub subslice_count: u32,
    pub dual_subslice_count: u32,
    pub max_eu_per_subslice: u32,
    pub max_slices: u32,
    pub max_subslices: u32,
    pub max_dual_subslices: u32,
}

/// Memory as the record describes it, in bytes. An integrated card has none of its own and shares
/// the system's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Memory {
    pub dedicated: u64,
    pub system_shared: u64,
    pub system_video: u64,
}

/// Everything the record is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordFacts {
    pub platform: Platform,
    pub gt: GtSystem,
    /// The GPU timestamp's frequency, in Hz.
    pub timestamp_hz: u32,
    /// The render clock's least and most, in MHz.
    pub render_mhz: (u32, u32),
    pub memory: Memory,
    pub local_memory: bool,
}

/// Builds the record.
pub fn build(facts: &RecordFacts) -> Vec<u8> {
    let mut record = vec![0u8; RECORD_SIZE];
    let platform = &facts.platform;
    for (field, value) in [
        (at::PRODUCT_FAMILY, platform.product_family),
        (at::PCH_FAMILY, platform.pch_family),
        (at::DISPLAY_CORE, platform.display_core),
        (at::RENDER_CORE, platform.render_core),
        (at::PLATFORM_TYPE, platform.platform_type),
        (at::GT_TYPE, platform.gt_type),
        (at::RENDER_IP, platform.render_ip.0),
    ] {
        put_dword(&mut record, field, value);
    }
    put_word(&mut record, at::DEVICE_ID, platform.device_id);
    put_word(&mut record, at::REVISION, platform.revision);

    put_dword(&mut record, at::TIMESTAMP_FREQUENCY, facts.timestamp_hz);
    put_dword(&mut record, at::MIN_RENDER_FREQUENCY, facts.render_mhz.0);
    put_dword(&mut record, at::MAX_RENDER_FREQUENCY, facts.render_mhz.1);
    put_dword(&mut record, at::NUMBER_OF_EUS, facts.gt.eu_count);
    put_dword(&mut record, at::CACHE_LINE_SIZE, CACHE_LINE_BYTES);
    put_qword(&mut record, at::DEDICATED_VIDEO_MEMORY, facts.memory.dedicated);
    put_qword(&mut record, at::SYSTEM_SHARED_MEMORY, facts.memory.system_shared);
    put_qword(&mut record, at::SYSTEM_VIDEO_MEMORY, facts.memory.system_video);

    let gt = &facts.gt;
    for (field, value) in [
        (at::EU_COUNT, gt.eu_count),
        (at::THREAD_COUNT, gt.thread_count),
        (at::SLICE_COUNT, gt.slice_count),
        (at::SUBSLICE_COUNT, gt.subslice_count),
        (at::DUAL_SUBSLICE_COUNT, gt.dual_subslice_count),
        (at::MAX_EU_PER_SUBSLICE, gt.max_eu_per_subslice),
        (at::MAX_SLICES, gt.max_slices),
        (at::MAX_SUBSLICES, gt.max_subslices),
        (at::MAX_DUAL_SUBSLICES, gt.max_dual_subslices),
    ] {
        put_dword(&mut record, field, value);
    }

    set_features(&mut record, facts.local_memory);
    let block = &mut record[at::PARTITION..at::PARTITION + partition::SIZE];
    partition::write(block, &partition::ours());
    record[at::FROM_VIRTUAL_MACHINE] = 1;
    record[at::SECURE_VIRTUAL_MACHINE] = 0;
    record
}

/// The model of memory the core offers, and no compression.
fn set_features(record: &mut [u8], local_memory: bool) {
    let flags: [(Feature, bool); 8] = [
        (features::PPGTT, true),
        (features::WDDM2_GPU_MMU, true),
        (features::WDDM2_64KB_PAGES, true),
        // The core's GPU addresses stop at 1 TiB, below where a process's own pointers live, so the
        // GPU can't share them.
        (features::SVM, false),
        (features::WDDM2_SVM, false),
        (features::LOCAL_MEMORY, local_memory),
        (features::TILE_Y, false),
        (features::HW_SEMAPHORE_64, false),
    ];
    for (feature, on) in flags {
        features::set(record, feature, on);
    }
    for feature in features::COMPRESSION {
        features::set(record, feature, false);
    }
}

/// What a record query asked for, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Asked {
    Count,
    Record { build_stamp: u32 },
    Refused(&'static str),
}

/// Answers a record query with `record`, keeping the guest's build stamp.
pub fn answer(record: &[u8], query: &VendorQuery<'_>, out: &mut Vec<u8>) -> (Status, Asked) {
    if query.size == COUNT_SIZE {
        out.extend_from_slice(&LINKED_ADAPTERS.to_le_bytes());
        return (Status::SUCCESS, Asked::Count);
    }
    if query.size < RECORD_SIZE {
        return (Status::INVALID_PARAMETER, Asked::Refused("shorter than one record"));
    }
    if query.size != RECORD_SIZE * LINKED_ADAPTERS as usize {
        return (Status::INVALID_PARAMETER, Asked::Refused("not the size of one record"));
    }
    let start = out.len();
    out.extend_from_slice(record);
    let build_stamp = dword(query.data, at::BUILD_STAMP).unwrap_or(0);
    put_dword(&mut out[start..], at::BUILD_STAMP, build_stamp);
    (Status::SUCCESS, Asked::Record { build_stamp })
}

/// The record's fields a log line names.
pub fn summary(record: &[u8]) -> RecordSummary {
    let field = |at| dword(record, at).unwrap_or(0);
    RecordSummary {
        product_family: field(at::PRODUCT_FAMILY),
        render_core: field(at::RENDER_CORE),
        device_id: field(at::DEVICE_ID) & 0xFFFF,
        render_ip: field(at::RENDER_IP),
        eu_count: field(at::EU_COUNT),
        virtual_machine: record.get(at::FROM_VIRTUAL_MACHINE).copied().unwrap_or(0),
        compression: features::COMPRESSION.iter().any(|&feature| features::is_set(record, feature)),
    }
}

/// The few fields of a record a person checks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordSummary {
    pub product_family: u32,
    pub render_core: u32,
    pub device_id: u32,
    pub render_ip: u32,
    pub eu_count: u32,
    pub virtual_machine: u8,
    pub compression: bool,
}

impl std::fmt::Display for RecordSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "product_family={} render_core={:#x} device={:#06x} render_ip={:#x} eus={} vm={} compression={}",
            self.product_family,
            self.render_core,
            self.device_id,
            self.render_ip,
            self.eu_count,
            self.virtual_machine,
            if self.compression { "on" } else { "off" }
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::bytes::{qword, word};

    pub fn facts() -> RecordFacts {
        RecordFacts {
            platform: Platform {
                product_family: 1275,
                render_core: 0x0C09,
                display_core: 0x0C09,
                device_id: 0x64A0,
                revision: 4,
                gt_type: 18,
                render_ip: IpWord::new(20, 4, 4),
                ..Platform::default()
            },
            gt: GtSystem { eu_count: 64, thread_count: 512, dual_subslice_count: 8, ..GtSystem::default() },
            timestamp_hz: 19_200_000,
            render_mhz: (400, 2050),
            memory: Memory { system_shared: 16 << 30, ..Memory::default() },
            local_memory: false,
        }
    }

    fn query(size: usize, data: &[u8]) -> VendorQuery<'_> {
        VendorQuery { kind: QUERY_TYPE, size, data, process_is_32_bit: false }
    }

    #[test]
    fn the_record_is_one_size_and_names_the_platform_the_guest_picks_its_module_by() {
        let record = build(&facts());
        assert_eq!(record.len(), RECORD_SIZE);
        assert_eq!(dword(&record, at::PRODUCT_FAMILY), Some(1275));
        assert_eq!(dword(&record, at::RENDER_CORE), Some(0x0C09));
        assert_eq!(word(&record, at::DEVICE_ID), Some(0x64A0));
        assert_eq!(word(&record, at::REVISION), Some(4));
        assert_eq!(dword(&record, at::GT_TYPE), Some(18));
    }

    #[test]
    fn ip_versions_pack_as_architecture_release_and_revision() {
        assert_eq!(IpWord::new(20, 4, 4).0, 20 << 22 | 4 << 14 | 4);
        assert_eq!(IpWord::new(20, 4, 0x41).0 & 0x3F, 1, "the revision has six bits");
        let record = build(&facts());
        assert_eq!(dword(&record, at::RENDER_IP), Some(IpWord::new(20, 4, 4).0));
    }

    #[test]
    fn the_record_says_virtual_machine_and_not_secure() {
        let record = build(&facts());
        assert_eq!(record[at::FROM_VIRTUAL_MACHINE], 1);
        assert_eq!(record[at::SECURE_VIRTUAL_MACHINE], 0);
    }

    #[test]
    fn compression_is_off_whatever_else_is_on() {
        let record = build(&facts());
        for feature in features::COMPRESSION {
            assert!(!features::is_set(&record, feature), "{}", feature.name);
        }
        assert!(!summary(&record).compression);
    }

    #[test]
    fn the_memory_model_is_the_cores() {
        let record = build(&facts());
        for feature in [features::PPGTT, features::WDDM2_GPU_MMU, features::WDDM2_64KB_PAGES] {
            assert!(features::is_set(&record, feature), "{}", feature.name);
        }
        for feature in [features::SVM, features::WDDM2_SVM, features::LOCAL_MEMORY, features::TILE_Y] {
            assert!(!features::is_set(&record, feature), "{}", feature.name);
        }
        let discrete = build(&RecordFacts { local_memory: true, ..facts() });
        assert!(features::is_set(&discrete, features::LOCAL_MEMORY));
    }

    #[test]
    fn clocks_counts_and_memory_land_where_they_belong() {
        let record = build(&facts());
        assert_eq!(dword(&record, at::TIMESTAMP_FREQUENCY), Some(19_200_000));
        assert_eq!(dword(&record, at::TIMESTAMP_FREQUENCY + 4), Some(0), "a 32-bit field");
        assert_eq!(dword(&record, at::MIN_RENDER_FREQUENCY), Some(400));
        assert_eq!(dword(&record, at::MAX_RENDER_FREQUENCY), Some(2050));
        assert_eq!(dword(&record, at::NUMBER_OF_EUS), Some(64));
        assert_eq!(dword(&record, at::EU_COUNT), Some(64));
        assert_eq!(dword(&record, at::THREAD_COUNT), Some(512));
        assert_eq!(dword(&record, at::DUAL_SUBSLICE_COUNT), Some(8));
        assert_eq!(qword(&record, at::SYSTEM_SHARED_MEMORY), Some(16 << 30));
        assert_eq!(qword(&record, at::DEDICATED_VIDEO_MEMORY), Some(0));
    }

    #[test]
    fn the_kernel_drivers_interface_version_is_a_gap_left_at_zero() {
        assert_eq!(dword(&build(&facts()), 0), Some(0));
    }

    #[test]
    fn the_registry_path_is_a_gap_left_empty() {
        assert!(build(&facts())[0x8..0x208].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn the_record_carries_the_partition_the_escape_gives() {
        let record = build(&facts());
        assert_eq!(
            partition::read(&record[at::PARTITION..at::PARTITION + partition::SIZE]),
            partition::ours()
        );
        assert_eq!(
            at::PARTITION + partition::SIZE,
            at::FROM_VIRTUAL_MACHINE,
            "it ends where the next field starts"
        );
    }

    #[test]
    fn the_first_ask_is_for_the_adapter_count() {
        let mut out = Vec::new();
        let (status, asked) = answer(&build(&facts()), &query(COUNT_SIZE, &[0; 4]), &mut out);
        assert_eq!((status, asked), (Status::SUCCESS, Asked::Count));
        assert_eq!(out, 1u32.to_le_bytes());
    }

    #[test]
    fn the_record_goes_back_with_the_guests_build_stamp() {
        let mut ask = vec![0u8; RECORD_SIZE];
        put_dword(&mut ask, at::BUILD_STAMP, 0x000F_0001);
        let mut out = vec![0xAA; 4];
        let (status, asked) = answer(&build(&facts()), &query(RECORD_SIZE, &ask), &mut out);
        assert_eq!(status, Status::SUCCESS);
        assert_eq!(asked, Asked::Record { build_stamp: 0x000F_0001 });
        assert_eq!(out.len(), 4 + RECORD_SIZE);
        assert_eq!(dword(&out[4..], at::BUILD_STAMP), Some(0x000F_0001));
        assert_eq!(dword(&out[4..], at::RENDER_CORE), Some(0x0C09));
    }

    #[test]
    fn any_other_size_is_refused_as_the_kernel_driver_refuses_it() {
        for size in [0, 8, RECORD_SIZE - 1, RECORD_SIZE + 1, 2 * RECORD_SIZE] {
            let mut out = Vec::new();
            let (status, asked) = answer(&build(&facts()), &query(size, &[]), &mut out);
            assert_eq!(status, Status::INVALID_PARAMETER, "{size}");
            assert!(matches!(asked, Asked::Refused(_)), "{size}");
            assert!(out.is_empty());
        }
    }

    #[test]
    fn the_summary_reads_back_what_was_built() {
        let text = summary(&build(&facts())).to_string();
        assert_eq!(
            text,
            "product_family=1275 render_core=0xc09 device=0x64a0 render_ip=0x5010004 eus=64 vm=1 compression=off"
        );
    }
}
