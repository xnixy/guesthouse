//! How much video memory the guest has, for OpenGL's `GL_NVX_gpu_memory_info`.
//!
//! The guest's OpenGL driver asks for this with an escape and passes on what it hears, and so does
//! `nvidia-smi`. With nothing here every figure reads 0, and programs that size themselves by it make
//! their own guess. Which field is which comes from filling each with a different number and reading
//! the GL values, or `nvidia-smi`'s, back.

use super::put;
use guesthouse_backend::VideoMemoryUse;

/// All in KiB, as the extension reports them.
const DEDICATED: usize = 0x30;
/// The part of the dedicated memory programs may have. `nvidia-smi` reports what is left of the
/// dedicated figure as reserved, and takes that from its used figure.
const USABLE: usize = 0x34;
/// Only counts. The extension's total is this and the dedicated figure added together.
const EXTRA_BEYOND_DEDICATED: usize = 0x3C;
const AVAILABLE: usize = 0x38;
const EVICTED: usize = 0x44;
const EVICTIONS: usize = 0x48;

const BYTES_PER_KIB: u64 = 1024;

fn kib(bytes: u64) -> u32 {
    u32::try_from(bytes / BYTES_PER_KIB).unwrap_or(u32::MAX)
}

/// Nothing is ever evicted here, as the guest's memory stays where it was put.
pub(super) fn answer(memory: VideoMemoryUse, data: &mut [u8]) {
    put(data, DEDICATED, kib(memory.total));
    put(data, USABLE, kib(memory.total));
    put(data, EXTRA_BEYOND_DEDICATED, 0);
    put(data, AVAILABLE, kib(memory.total.saturating_sub(memory.used)));
    put(data, EVICTED, 0);
    put(data, EVICTIONS, 0);
}

#[cfg(test)]
mod tests {
    use super::super::get;
    use super::*;

    const SIZE: usize = 0x54;

    #[test]
    fn the_card_and_what_is_left_are_reported_in_kib() {
        let mut data = [0; SIZE];
        answer(VideoMemoryUse { total: 32 << 30, used: 8 << 30 }, &mut data);
        assert_eq!(get(&data, DEDICATED), Some(32 << 20));
        assert_eq!(get(&data, AVAILABLE), Some(24 << 20));
    }

    #[test]
    fn none_of_the_card_is_held_back() {
        let mut data = [0; SIZE];
        answer(VideoMemoryUse { total: 32 << 30, used: 8 << 30 }, &mut data);
        assert_eq!(get(&data, USABLE), get(&data, DEDICATED));
    }

    #[test]
    fn more_used_than_there_is_leaves_none() {
        let mut data = [0xFF; SIZE];
        answer(VideoMemoryUse { total: 1 << 20, used: 2 << 20 }, &mut data);
        assert_eq!(get(&data, AVAILABLE), Some(0));
    }

    #[test]
    fn evictions_read_zero_whatever_the_guest_sent() {
        let mut data = [0xFF; SIZE];
        answer(VideoMemoryUse { total: 1 << 20, used: 0 }, &mut data);
        assert_eq!(get(&data, EVICTED), Some(0));
        assert_eq!(get(&data, EVICTIONS), Some(0));
        assert_eq!(get(&data, EXTRA_BEYOND_DEDICATED), Some(0));
    }

    #[test]
    fn a_size_past_what_the_field_holds_is_the_most_it_can_say() {
        assert_eq!(kib(u64::MAX), u32::MAX);
    }

    #[test]
    fn a_short_buffer_is_not_a_problem() {
        answer(VideoMemoryUse { total: 1 << 20, used: 0 }, &mut [0; 8]);
    }
}
