//! The GPU address partition, GMM_GFX_PARTITIONING: where a process's memory manager puts each of
//! its heaps of GPU addresses. It is in the adapter record, and the guest can ask for it, or set its
//! own, with a memory manager escape.
//!
//! The block is [`SIZE`] bytes of eleven ranges, each a base and an inclusive limit. The first ten
//! are in the order of compute-runtime's GMM_GFX_PARTITIONING: Standard, Standard64KB, two reserved,
//! SVM, TR and four 32-bit heaps. The eleventh has no name, and stays empty.
//!
//! Every range given lies in the upper of the core's GPU ranges, above the hole near 4 GiB and below
//! its ceiling at 1 TiB, so whatever the guest reserves inside them the core can place. An empty
//! partition would leave compute-runtime to build its heaps near the top of a 47-bit space, where
//! the core has no addresses at all.

use super::bytes::{put_qword, qword};
use guesthouse_backend::GPU_RANGES;

pub const SIZE: usize = 0xB0;
const RANGES: usize = SIZE / RANGE_SIZE;
const RANGE_SIZE: usize = 16;

const GIB: u64 = 1 << 30;
/// What a 32-bit heap spans: offsets from its base fit 32 bits.
const HEAP_32_SIZE: u64 = 4 * GIB;
const HEAPS_32: usize = 4;

/// The ranges by their place in the block.
mod index {
    pub const STANDARD: usize = 0;
    pub const STANDARD_64KB: usize = 1;
    pub const HEAP_32: usize = 6;
}

/// A range of GPU addresses, its limit included, as the block holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Range {
    pub base: u64,
    pub limit: u64,
}

impl Range {
    fn spanning(base: u64, size: u64) -> Self {
        Self { base, limit: base + size - 1 }
    }

    pub fn is_empty(&self) -> bool {
        self.base == 0 && self.limit == 0
    }
}

/// The partition this backend gives: the four 32-bit heaps first, then Standard, then
/// Standard64KB to the end of the core's addresses. SVM stays empty, as the record says the GPU
/// can't share the CPU's pointers.
pub fn ours() -> [Range; RANGES] {
    let upper = GPU_RANGES[1];
    let heaps_start = upper.start.next_multiple_of(HEAP_32_SIZE);
    let standard_start = heaps_start + HEAPS_32 as u64 * HEAP_32_SIZE;
    let standard_64kb_start = standard_start + (upper.end - standard_start) / 2;
    let mut ranges = [Range::default(); RANGES];
    for heap in 0..HEAPS_32 {
        ranges[index::HEAP_32 + heap] =
            Range::spanning(heaps_start + heap as u64 * HEAP_32_SIZE, HEAP_32_SIZE);
    }
    ranges[index::STANDARD] = Range::spanning(standard_start, standard_64kb_start - standard_start);
    ranges[index::STANDARD_64KB] = Range::spanning(standard_64kb_start, upper.end - standard_64kb_start);
    ranges
}

pub fn write(block: &mut [u8], ranges: &[Range; RANGES]) {
    for (at, range) in ranges.iter().enumerate() {
        put_qword(block, at * RANGE_SIZE, range.base);
        put_qword(block, at * RANGE_SIZE + 8, range.limit);
    }
}

/// The ranges in a block, as far as it goes.
pub fn read(block: &[u8]) -> [Range; RANGES] {
    let mut ranges = [Range::default(); RANGES];
    for (at, range) in ranges.iter_mut().enumerate() {
        range.base = qword(block, at * RANGE_SIZE).unwrap_or(0);
        range.limit = qword(block, at * RANGE_SIZE + 8).unwrap_or(0);
    }
    ranges
}

/// A partition's ranges as `base-limit`, `-` for an empty one.
pub struct Text(pub [Range; RANGES]);

impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (at, range) in self.0.iter().enumerate() {
            let space = if at == 0 { "" } else { " " };
            if range.is_empty() {
                write!(f, "{space}-")?;
            } else {
                write!(f, "{space}{:#x}-{:#x}", range.base, range.limit)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_range_lies_in_the_cores_upper_addresses() {
        let upper = GPU_RANGES[1];
        for range in ours().iter().filter(|range| !range.is_empty()) {
            assert!(upper.contains(range.base) && upper.contains(range.limit), "{range:x?}");
            assert!(range.base <= range.limit);
        }
    }

    #[test]
    fn no_two_ranges_overlap() {
        let ranges: Vec<Range> = ours().into_iter().filter(|range| !range.is_empty()).collect();
        for (at, one) in ranges.iter().enumerate() {
            for other in &ranges[at + 1..] {
                assert!(one.limit < other.base || other.limit < one.base, "{one:x?} {other:x?}");
            }
        }
    }

    #[test]
    fn the_32_bit_heaps_each_span_4_gib_and_the_64kb_heap_is_aligned() {
        let ranges = ours();
        for heap in &ranges[index::HEAP_32..index::HEAP_32 + HEAPS_32] {
            assert_eq!(heap.limit - heap.base + 1, HEAP_32_SIZE);
        }
        assert_eq!(ranges[index::STANDARD_64KB].base % 0x1_0000, 0);
        assert_eq!(ranges[index::STANDARD_64KB].limit, GPU_RANGES[1].end - 1);
        assert!(ranges[4].is_empty(), "SVM");
        assert!(ranges[RANGES - 1].is_empty(), "the eleventh, which has no name");
    }

    #[test]
    fn a_written_block_reads_back() {
        let mut block = [0u8; SIZE];
        write(&mut block, &ours());
        assert_eq!(read(&block), ours());
        assert!(Text(ours()).to_string().starts_with("0x"));
    }
}
