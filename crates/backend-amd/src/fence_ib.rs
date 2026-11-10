//! Command buffers of the daemon's own that have the card write a queue's fence.
//!
//! The guest waits on a fence in its memory. One small command buffer goes last in each submission
//! and stores the fence's new value: at end of pipe on the graphics and compute engines, and after
//! the earlier packets on the DMA engine, which runs them in order. So the card says when a run is
//! through, and nothing in the daemon waits for one.
//!
//! They sit in one buffer for each address space, mapped in the gap between the core's two ranges,
//! where the guest can never place anything. A run takes a slot of it, and the slot is free again
//! once the kernel says the run has retired.

use crate::device::Mapping;
use crate::info::{HW_IP_COMPUTE, HW_IP_DMA, HW_IP_GFX};
use crate::memory::{self, CpuAccess, DOMAIN_GTT, GemHandle, MapFlags, MemoryType};
use crate::submit::{self, Ib};
use std::fs::File;
use std::io;

/// Where the buffer sits in a space: in the gap below the core's upper range, past the copy buffer.
pub const ADDRESS: u64 = 0x1_0010_0000;
/// How many runs of one space can be on the card at once. A guest keeps a few frames ahead at most.
pub const SLOTS: usize = 256;
const SLOT_WORDS: usize = 8;
const SLOT_BYTES: usize = SLOT_WORDS * size_of::<u32>();

// A type 3 packet's header: the type, how many words follow less one, and the opcode.
const PACKET3: u32 = 3 << 30;
const PACKET3_COUNT_SHIFT: u32 = 16;
const PACKET3_OPCODE_SHIFT: u32 = 8;
/// Stores a value in memory when everything before it has left the pipe.
const RELEASE_MEM: u32 = 0x49;
const RELEASE_MEM_WORDS: u32 = 7;
/// Writes back and invalidates the caches at end of pipe, then stores the value, as amdgpu's own
/// fence does: what the run wrote is in memory before the guest sees its fence move.
const EVENT_CACHE_FLUSH_AND_INV_TS: u32 = 0x14;
/// The index every timestamp event carries.
const EVENT_INDEX_TS: u32 = 5 << 8;
// The cache actions of RELEASE_MEM on GFX 10 and 11, and its write policy.
const GCR_GLM_WB: u32 = 1 << 12;
const GCR_GLM_INV: u32 = 1 << 13;
const GCR_GL2_WB: u32 = 1 << 21;
const GCR_SEQ: u32 = 1 << 22;
const CACHE_POLICY_BYPASS: u32 = 3 << 25;
const FLUSH_CACHES: u32 = GCR_SEQ | GCR_GL2_WB | GCR_GLM_INV | GCR_GLM_WB | CACHE_POLICY_BYPASS;
const DATA_64_BIT: u32 = 2 << 29;
const SEND_DATA_AFTER_WRITE_CONFIRM: u32 = 3 << 24;

/// How an engine is told to store a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// The graphics and compute engines, at end of pipe.
    EndOfPipe,
    /// The DMA engine's write packet.
    Dma,
}

/// How this engine writes a fence, or `None` if this card's way isn't known here. The end-of-pipe
/// packet is the one for GFX 10 and 11.
pub fn writer_for(ip_type: u32, gfx_major: u32) -> Option<Writer> {
    match ip_type {
        HW_IP_GFX | HW_IP_COMPUTE if matches!(gfx_major, 10 | 11) => Some(Writer::EndOfPipe),
        HW_IP_DMA => Some(Writer::Dma),
        _ => None,
    }
}

/// The command buffer that stores `value` at the GPU address `fence`.
pub fn words(writer: Writer, fence: u64, value: u64) -> [u32; SLOT_WORDS] {
    let (low, high) = (value as u32, (value >> 32) as u32);
    match writer {
        Writer::EndOfPipe => [
            PACKET3 | (RELEASE_MEM_WORDS - 1) << PACKET3_COUNT_SHIFT | RELEASE_MEM << PACKET3_OPCODE_SHIFT,
            FLUSH_CACHES | EVENT_CACHE_FLUSH_AND_INV_TS | EVENT_INDEX_TS,
            DATA_64_BIT | SEND_DATA_AFTER_WRITE_CONFIRM,
            fence as u32,
            (fence >> 32) as u32,
            low,
            high,
            0,
        ],
        Writer::Dma => {
            let packet = submit::sdma_write_ib(fence, &[low, high]);
            packet.try_into().expect("a two-word write fills one slot")
        }
    }
}

/// Which slots are free. Kept apart from the buffer so it can be tested without a card.
#[derive(Debug)]
pub struct Slots {
    free: Vec<usize>,
}

impl Slots {
    pub fn new(count: usize) -> Self {
        Self { free: (0..count).rev().collect() }
    }

    pub fn take(&mut self) -> Option<usize> {
        self.free.pop()
    }

    pub fn give_back(&mut self, slot: usize) {
        debug_assert!(!self.free.contains(&slot), "a slot given back twice");
        self.free.push(slot);
    }
}

/// One space's buffer of fence writers.
#[derive(Debug)]
pub struct FenceIbs {
    handle: GemHandle,
    view: Mapping,
    slots: Slots,
}

impl FenceIbs {
    /// Makes the buffer on a space's own open and maps it there. The card only reads it, and
    /// uncached, as the CPU writes a slot again for each run.
    pub fn make(file: &File, alignment: u64) -> io::Result<Self> {
        let size = ((SLOTS * SLOT_BYTES) as u64).next_multiple_of(alignment.max(1));
        let handle = memory::create(file, size, alignment, DOMAIN_GTT, CpuAccess::Required)?;
        let flags =
            MapFlags { readable: true, writable: false, executable: true, memory_type: MemoryType::Uncached };
        let made = Mapping::of(file, handle, size as usize)
            .and_then(|view| memory::map(file, handle, ADDRESS, 0, size, flags).map(|()| view));
        match made {
            Ok(view) => Ok(Self { handle, view, slots: Slots::new(SLOTS) }),
            Err(error) => {
                memory::close(file, handle);
                Err(error)
            }
        }
    }

    /// The buffer's handle on the space's open, to name in a submission.
    pub fn handle(&self) -> GemHandle {
        self.handle
    }

    /// Writes a fence writer into a free slot. `None` when every slot's run is still on the card.
    pub fn write(&mut self, writer: Writer, fence: u64, value: u64) -> Option<(usize, Ib)> {
        let slot = self.slots.take()?;
        let start = slot * SLOT_BYTES;
        for (index, word) in words(writer, fence, value).into_iter().enumerate() {
            self.view.write_u32(start + index * size_of::<u32>(), word);
        }
        Some((slot, Ib { address: ADDRESS + start as u64, bytes: SLOT_BYTES as u32 }))
    }

    pub fn give_back(&mut self, slot: usize) {
        self.slots.give_back(slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FENCE: u64 = 0x12_3456_7008;
    const VALUE: u64 = 0x1_0000_0002;

    #[test]
    fn the_end_of_pipe_writer_is_one_packet_with_the_address_and_all_of_the_value() {
        let words = words(Writer::EndOfPipe, FENCE, VALUE);
        assert_eq!(words[0], 0xC006_4900);
        assert_eq!(words[1], 0x0660_3514, "the cache flush and fields of amdgpu's own fence");
        assert_eq!(words[2], 0x4300_0000);
        assert_eq!(words[3..7], [0x3456_7008, 0x12, 2, 1]);
        assert_eq!(words[7], 0);
    }

    #[test]
    fn the_dma_writer_stores_both_halves_and_fills_the_slot() {
        let words = words(Writer::Dma, FENCE, VALUE);
        assert_eq!(words[1..3], [0x3456_7008, 0x12]);
        assert_eq!(words[3], 1, "two words, less one");
        assert_eq!(words[4..6], [2, 1]);
        assert_eq!(words[6..], [0, 0], "padded with no-ops");
    }

    #[test]
    fn graphics_and_compute_write_at_end_of_pipe_only_on_cards_whose_packet_is_known() {
        assert_eq!(writer_for(HW_IP_GFX, 10), Some(Writer::EndOfPipe));
        assert_eq!(writer_for(HW_IP_COMPUTE, 11), Some(Writer::EndOfPipe));
        assert_eq!(writer_for(HW_IP_GFX, 9), None);
        assert_eq!(writer_for(HW_IP_GFX, 12), None);
        assert_eq!(writer_for(HW_IP_DMA, 12), Some(Writer::Dma));
    }

    #[test]
    fn a_slot_is_free_again_only_after_it_is_given_back() {
        let mut slots = Slots::new(2);
        let (first, second) = (slots.take().unwrap(), slots.take().unwrap());
        assert_ne!(first, second);
        assert_eq!(slots.take(), None);
        slots.give_back(first);
        assert_eq!(slots.take(), Some(first));
    }

    #[test]
    fn the_buffer_ends_before_the_cores_upper_range() {
        let end = ADDRESS + (SLOTS * SLOT_BYTES) as u64;
        assert!(end.next_multiple_of(0x1_0000) <= guesthouse_backend::GPU_RANGES[1].start);
        assert!(ADDRESS >= guesthouse_backend::GPU_RANGES[0].end + crate::copy_ib::BYTES as u64);
    }
}
