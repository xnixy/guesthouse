//! Moving what an allocation holds into new memory.
//!
//! The guest's first CPU lock of an allocation moves it from a buffer on the card to pages shared
//! with the guest, and what the GPU wrote there has to come along: a surface the guest reads back
//! after drawing, like a window's picture on its way to GDI, would otherwise arrive empty.
//!
//! The DMA engine copies it. Both buffers are mapped for the copy at two fixed windows in the
//! daemon's own space, in the gap between the core's two address ranges, past the copy commands'
//! buffer, where no guest mapping can land. A buffer larger than a window goes a window at a time.

use crate::copy_ib;
use guesthouse_backend::GPU_RANGES;

/// Where the buffer being moved is mapped for the copy.
pub const SOURCE_WINDOW: u64 = 0x1_1000_0000;
/// Where the new memory is mapped for the copy.
pub const TARGET_WINDOW: u64 = 0x1_2000_0000;
/// How much of each buffer one copy takes. The two windows sit side by side at this size.
pub const WINDOW_BYTES: u64 = 0x1000_0000;
/// Mappings are made in whole pages.
const PAGE_BYTES: u64 = 0x1000;
/// Room for the copy commands' buffer, which is rounded up to the card's mapping alignment: a few
/// megabytes at most.
const COMMANDS_ROOM: u64 = 0x100_0000;

const _: () = {
    assert!(GPU_RANGES[0].end <= copy_ib::ADDRESS);
    assert!(copy_ib::ADDRESS + COMMANDS_ROOM <= SOURCE_WINDOW);
    assert!(SOURCE_WINDOW + WINDOW_BYTES <= TARGET_WINDOW);
    assert!(TARGET_WINDOW + WINDOW_BYTES <= GPU_RANGES[1].start);
};

/// One window's worth of a move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    /// Where it starts in both buffers.
    pub offset: u64,
    /// How many bytes are copied.
    pub bytes: u64,
    /// How many bytes are mapped, which is the copy rounded up to whole pages.
    pub mapped: u64,
}

/// The pieces a move of `size` bytes goes in, in order.
pub fn pieces(size: u64) -> impl Iterator<Item = Piece> {
    (0..size).step_by(WINDOW_BYTES as usize).map(move |offset| {
        let bytes = (size - offset).min(WINDOW_BYTES);
        Piece { offset, bytes, mapped: bytes.next_multiple_of(PAGE_BYTES) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdma_copy::{EngineForm, LinearCopy};

    #[test]
    fn a_small_buffer_goes_in_one_piece_mapped_in_whole_pages() {
        let all: Vec<Piece> = pieces(0xa_0010).collect();
        assert_eq!(all, [Piece { offset: 0, bytes: 0xa_0010, mapped: 0xa_1000 }]);
    }

    #[test]
    fn a_large_buffer_goes_a_window_at_a_time() {
        let all: Vec<Piece> = pieces(2 * WINDOW_BYTES + 0x3000).collect();
        assert_eq!(all.len(), 3);
        assert_eq!(all[1], Piece { offset: WINDOW_BYTES, bytes: WINDOW_BYTES, mapped: WINDOW_BYTES });
        assert_eq!(all[2], Piece { offset: 2 * WINDOW_BYTES, bytes: 0x3000, mapped: 0x3000 });
    }

    #[test]
    fn nothing_to_move_is_no_pieces() {
        assert_eq!(pieces(0).count(), 0);
    }

    #[test]
    fn a_whole_windows_commands_fit_the_commands_buffer() {
        let copy =
            LinearCopy { source_address: SOURCE_WINDOW, target_address: TARGET_WINDOW, bytes: WINDOW_BYTES };
        for form in [EngineForm::Sdma5, EngineForm::Sdma7] {
            assert!(size_of_val(copy.commands(form).as_slice()) <= copy_ib::BYTES);
        }
    }
}
