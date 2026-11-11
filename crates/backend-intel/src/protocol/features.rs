//! The feature bits of the adapter record that this backend sets or clears.
//!
//! The record carries a feature table, a long run of one-bit flags the guest's driver and its
//! memory manager read to decide what the hardware can do. Only the flags whose place is known are
//! here, under gmmlib's names. The rest of the table stays zero.

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
}
