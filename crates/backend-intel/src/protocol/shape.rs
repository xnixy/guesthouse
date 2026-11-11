//! The slice arrays of the record's GPU shape: which slices, dual subslices, subslices and EUs
//! there are, as compute-runtime's topology reader walks them when it opens a device.
//!
//! The card's topology is a mask of Xe cores (xe calls them dual subslices) and one mask of the
//! EUs in each. This backend turns each Xe core into one subslice of the record: the core's bit
//! counted along a slice is its subslice's index there, and the record's dual subslices each hold
//! two consecutive subslices, so that index is twice the dual subslice's place plus the subslice's
//! place in it. A dual subslice is enabled when either of its subslices is.
//!
//! The record has room for more slices than it has places for, and for more dual subslices in a
//! slice than the reader looks at. A core that falls outside the places the reader looks at is
//! dropped by [`representable`], and the counts are taken from what is left, so what is said and
//! what is written agree. Slots past the ones the reader looks at are never written.

use super::bytes::put_dword;

/// Where the first slice starts in the record, and how far apart slices are.
const FIRST_SLICE: usize = 0xBA8;
const SLICE_STRIDE: usize = 0x14C;
/// The slices the reader looks at.
pub const READ_SLICES: u32 = 8;

/// Inside a slice: the slice's own flag, the flat array of subslices, the dual subslices, and the
/// two counts that end it.
mod slice {
    pub const ENABLED: usize = 0x0;
    pub const SUBSLICES: usize = 0x4;
    pub const DUAL_SUBSLICES: usize = 0x64;
    pub const SUBSLICE_COUNT: usize = 0x144;
    pub const DUAL_SUBSLICE_COUNT: usize = 0x148;
}

/// The flat array has eight subslices, the dual subslices six of two each; the reader looks at
/// all of them.
const FLAT_SUBSLICES: u32 = 8;
const DUAL_SUBSLICES: u32 = 6;
const SUBSLICES_PER_DUAL: u32 = 2;
const SUBSLICE_STRIDE: usize = 0xC;
const DUAL_STRIDE: usize = 0x1C;
/// Inside a dual subslice, its flag and then its two subslices.
const DUAL_FIRST_SUBSLICE: usize = 0x4;

/// Inside a subslice: its flag, its EU count and its EU mask.
mod subslice {
    pub const ENABLED: usize = 0x0;
    pub const EU_COUNT: usize = 0x4;
    pub const EU_MASK: usize = 0x8;
}

/// One Xe core: the slice it is in, its place counted along that slice, and its EUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Core {
    pub slice: u32,
    pub index: u32,
    pub eu_count: u32,
    pub eu_mask: u32,
}

/// The cores that fit where the reader looks: a slice below both [`READ_SLICES`] and the most
/// slices the platform has, and a place along it that a dual subslice holds.
pub fn representable(cores: &[Core], max_slices: u32) -> Vec<Core> {
    let places = DUAL_SUBSLICES * SUBSLICES_PER_DUAL;
    cores
        .iter()
        .copied()
        .filter(|core| core.slice < READ_SLICES.min(max_slices) && core.index < places)
        .collect()
}

/// How many slices the cores are in.
pub fn slices_in(cores: &[Core]) -> u32 {
    let mut seen = 0u32;
    for core in cores {
        seen |= 1u32.checked_shl(core.slice).unwrap_or(0);
    }
    seen.count_ones()
}

/// The place of a slice's field in the record.
fn at(slice: u32, field: usize) -> usize {
    FIRST_SLICE + SLICE_STRIDE * slice as usize + field
}

fn put_subslice(record: &mut [u8], start: usize, core: &Core) {
    record[start + subslice::ENABLED] = 1;
    put_dword(record, start + subslice::EU_COUNT, core.eu_count);
    put_dword(record, start + subslice::EU_MASK, core.eu_mask);
}

/// Writes the cores into the record. They must be [`representable`].
pub fn write(record: &mut [u8], cores: &[Core]) {
    for slice in 0..READ_SLICES {
        let mine: Vec<&Core> = cores.iter().filter(|core| core.slice == slice).collect();
        if mine.is_empty() {
            continue;
        }
        record[at(slice, slice::ENABLED)] = 1;
        for core in &mine {
            if core.index < FLAT_SUBSLICES {
                put_subslice(
                    record,
                    at(slice, slice::SUBSLICES) + SUBSLICE_STRIDE * core.index as usize,
                    core,
                );
            }
            let dual = core.index / SUBSLICES_PER_DUAL;
            let within = core.index % SUBSLICES_PER_DUAL;
            let start = at(slice, slice::DUAL_SUBSLICES) + DUAL_STRIDE * dual as usize;
            record[start] = 1;
            put_subslice(record, start + DUAL_FIRST_SUBSLICE + SUBSLICE_STRIDE * within as usize, core);
        }
        let count = mine.len() as u32;
        put_dword(record, at(slice, slice::SUBSLICE_COUNT), count);
        // Both counts are of Xe cores, as the scalar counts of the GPU's shape are.
        put_dword(record, at(slice, slice::DUAL_SUBSLICE_COUNT), count);
    }
}

/// Whether any slice is enabled in a record.
pub fn filled(record: &[u8]) -> bool {
    (0..READ_SLICES).any(|slice| record.get(at(slice, slice::ENABLED)).is_some_and(|&flag| flag != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::dword;

    fn core(slice: u32, index: u32) -> Core {
        Core { slice, index, eu_count: 8, eu_mask: 0xFF }
    }

    #[test]
    fn the_slice_fields_are_where_the_layout_has_them() {
        let mut record = vec![0u8; 0x2574];
        write(&mut record, &[core(1, 3)]);
        // Slice 1 starts 0x14C after slice 0.
        assert_eq!(record[0xBA8 + 0x14C], 1);
        // Subslice 3 of the flat array, then core 3 as the second subslice of dual subslice 1.
        assert_eq!(record[0xBA8 + 0x14C + 4 + 3 * 12], 1);
        let dual = 0xBA8 + 0x14C + 0x64 + 0x1C;
        assert_eq!((record[dual], record[dual + 4 + 12]), (1, 1));
        assert_eq!(record[dual + 4], 0, "the first subslice of that dual subslice is not there");
        assert_eq!(dword(&record, dual + 4 + 12 + 4), Some(8));
        assert_eq!(dword(&record, dual + 4 + 12 + 8), Some(0xFF));
        assert_eq!(dword(&record, 0xBA8 + 0x14C + 0x144), Some(1));
        assert_eq!(record[0xBA8], 0, "slice 0 has no core");
        assert!(filled(&record));
    }

    #[test]
    fn what_the_reader_cannot_reach_is_dropped() {
        let cores = [core(0, 11), core(0, 12), core(7, 0), core(8, 0), core(2, 0)];
        let kept = representable(&cores, 4);
        assert_eq!(kept, vec![core(0, 11), core(2, 0)]);
        assert_eq!(slices_in(&representable(&cores, 16)), 3);
    }
}
