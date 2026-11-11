//! The feature bits of the adapter record that this backend sets or clears.
//!
//! The record carries a feature table, a long run of one-bit flags saying what the hardware can do.
//! Only the flags whose place is known are here, under gmmlib's names. The rest of the table stays zero.
//! The table's workaround counterpart has one entry set, [`set_workaround`].

/// One flag: the byte it is in, counted from the start of the record, and its bit there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    pub name: &'static str,
    pub byte: usize,
    pub bit: u8,
}

/// The flag at `bit` of the word that starts at byte `word` of the record.
const fn at(name: &'static str, word: usize, bit: usize) -> Feature {
    Feature { name, byte: word + bit / 8, bit: (bit % 8) as u8 }
}

/// Where three words of the table start.
const WORD_FIRST: usize = 0x2B4;
const WORD_A: usize = 0x2BC;
const WORD_B: usize = 0x2E4;
const WORD_C: usize = 0x2EC;

/// Per-process GPU page tables.
pub const PPGTT: Feature = at("FtrPPGTT", WORD_B, 22);
/// The GPU sharing the CPU's pointers.
pub const SVM: Feature = at("FtrSVM", WORD_B, 27);
/// The WDDM 2 model of GPU addresses, where each process has its own.
pub const WDDM2_GPU_MMU: Feature = at("FtrWddm2GpuMmu", WORD_B, 46);
/// The WDDM 2 form of [`SVM`].
pub const WDDM2_SVM: Feature = at("FtrWddm2Svm", WORD_B, 47);
/// 64 KiB pages in the GPU's page tables.
pub const WDDM2_64KB_PAGES: Feature = at("FtrWddm2_1_64kbPages", WORD_B, 50);
/// Memory on the card itself, which an integrated card hasn't got.
pub const LOCAL_MEMORY: Feature = at("FtrLocalMemory", WORD_B, 58);
/// The older Y tiling, which Xe2 has dropped.
pub const TILE_Y: Feature = at("FtrTileY", WORD_B, 62);
/// A 64-bit semaphore wait in the command streamer.
pub const HW_SEMAPHORE_64: Feature = at("FtrHwSemaphore64", WORD_A, 6);

/// The command streamer for the blitter, which copies.
pub const BLITTER_RING: Feature = at("FtrBlitterRing", WORD_FIRST, 14);
/// The ring for compute, and the node that submits to it.
pub const CCS_RING: Feature = at("FtrCCSRing", WORD_A, 8);
pub const CCS_NODE: Feature = at("FtrCCSNode", WORD_A, 12);
/// More than one compute engine.
pub const CCS_MULTI_INSTANCE: Feature = at("FtrCCSMultiInstance", WORD_A, 14);
/// Scheduling in hardware, which is what makes the guest's drivers use hardware queues.
pub const HW_SCHEDULING: Feature = at("FtrHwScheduling", WORD_C, 49);
/// Xe2's tiling formats.
pub const XE2_TILING: Feature = at("FtrXe2PlusTiling", WORD_C, 0);
/// Five levels of GPU page tables.
pub const PML5: Feature = at("FtrPml5Support", WORD_C, 16);
/// The walker's mid-thread preemption.
pub const WALKER_MTP: Feature = at("FtrWalkerMTP", WORD_B, 19);
/// Preemption of compute work at a batch boundary, a thread group boundary and mid-thread.
pub const GPGPU_MID_BATCH_PREEMPT: Feature = at("FtrGpGpuMidBatchPreempt", WORD_B, 9);
pub const GPGPU_THREAD_GROUP_PREEMPT: Feature = at("FtrGpGpuThreadGroupLevelPreempt", WORD_B, 10);
pub const GPGPU_MID_THREAD_PREEMPT: Feature = at("FtrGpGpuMidThreadLevelPreempt", WORD_B, 11);

/// The four flags that let the guest compress surfaces. Compression needs the host to set up
/// memory the same way, which this backend can't yet, so all four stay clear.
pub const COMPRESSION: [Feature; 4] = [
    at("FtrE2ECompression", WORD_B, 52),
    at("FtrLinearCCS", WORD_B, 54),
    at("FtrFlatPhysCCS", WORD_C, 1),
    at("FtrXe2Compression", WORD_C, 2),
];

/// Sets or clears a flag in a record.
pub fn set(record: &mut [u8], feature: Feature, on: bool) {
    let mask = 1 << feature.bit;
    if on {
        record[feature.byte] |= mask;
    } else {
        record[feature.byte] &= !mask;
    }
}

/// Where the workaround table starts in the record. Its entries are four bits wide, two to a byte,
/// the even one in the low half.
const WORKAROUND_TABLE: usize = 0x3A0;

/// Sets workaround entry `entry` to `value`, whose four low bits are kept. An entry past the
/// record is not set.
pub fn set_workaround(record: &mut [u8], entry: usize, value: u8) {
    let Some(byte) = record.get_mut(WORKAROUND_TABLE + entry / 2) else { return };
    let shift = 4 * (entry % 2);
    *byte = *byte & !(0xF << shift) | (value & 0xF) << shift;
}

pub fn workaround(record: &[u8], entry: usize) -> Option<u8> {
    let byte = record.get(WORKAROUND_TABLE + entry / 2)?;
    Some(byte >> (4 * (entry % 2)) & 0xF)
}

pub fn is_set(record: &[u8], feature: Feature) -> bool {
    record.get(feature.byte).is_some_and(|byte| byte & (1 << feature.bit) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::{dword, qword};

    #[test]
    fn a_flag_lands_on_its_bit_of_its_word() {
        let mut record = vec![0u8; 0x400];
        set(&mut record, WDDM2_GPU_MMU, true);
        assert_eq!(qword(&record, WORD_B), Some(1 << 46));
        set(&mut record, COMPRESSION[3], true);
        assert_eq!(dword(&record, WORD_C), Some(1 << 2));
        set(&mut record, WDDM2_GPU_MMU, false);
        assert_eq!(qword(&record, WORD_B), Some(0));
        assert!(is_set(&record, COMPRESSION[3]));
    }

    #[test]
    fn the_flags_each_have_a_place_of_their_own() {
        let all = [
            PPGTT,
            SVM,
            WDDM2_GPU_MMU,
            WDDM2_SVM,
            WDDM2_64KB_PAGES,
            LOCAL_MEMORY,
            TILE_Y,
            HW_SEMAPHORE_64,
            BLITTER_RING,
            CCS_RING,
            CCS_NODE,
            CCS_MULTI_INSTANCE,
            HW_SCHEDULING,
            XE2_TILING,
            PML5,
            WALKER_MTP,
            GPGPU_MID_BATCH_PREEMPT,
            GPGPU_THREAD_GROUP_PREEMPT,
            GPGPU_MID_THREAD_PREEMPT,
            COMPRESSION[0],
            COMPRESSION[1],
            COMPRESSION[2],
            COMPRESSION[3],
        ];
        for (at, one) in all.iter().enumerate() {
            for other in &all[at + 1..] {
                assert_ne!((one.byte, one.bit), (other.byte, other.bit), "{} {}", one.name, other.name);
            }
        }
        assert_eq!((HW_SCHEDULING.byte, HW_SCHEDULING.bit), (0x2EC + 6, 1));
        assert_eq!((BLITTER_RING.byte, BLITTER_RING.bit), (0x2B4 + 1, 6));
    }

    #[test]
    fn a_workaround_entry_is_a_nibble() {
        let mut record = vec![0u8; 0x600];
        record[0x584] = 0xA0;
        set_workaround(&mut record, 968, 1);
        assert_eq!(record[0x584], 0xA1, "the low half, the high half kept");
        set_workaround(&mut record, 969, 0x1F);
        assert_eq!((record[0x584], workaround(&record, 969)), (0xF1, Some(0xF)));
        set_workaround(&mut record, 100_000, 1);
        assert_eq!(workaround(&record, 100_000), None);
    }
}
