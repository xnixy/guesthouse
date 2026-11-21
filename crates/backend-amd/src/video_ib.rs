//! The daemon's copy of a guest's video command buffers, which is what the card's video ring is
//! given to run.
//!
//! amdgpu maps a video command buffer with the CPU to read it before it queues the work, as it does
//! for no graphics, compute or copy work. It can only map a buffer whose pages it holds itself, and
//! a guest's command buffer is guest pages, brought in as a dma-buf where the host allows, which
//! has none for it to map. So a guest's video commands are never handed over where they lie: they
//! are copied into a buffer made on the card's own driver, and that one is run. What the commands
//! name by GPU address stays where it is, as the engine reads it through the space's page tables
//! like any other work.
//!
//! Each address space has a buffer of its own for the copy, as it has for its fence writers, so
//! one guest process's commands are never mapped where another's work could read them. A run is
//! waited for before the next is written, so one buffer is enough.

use crate::device::Mapping;
use crate::memory::{self, CpuAccess, DOMAIN_GTT, GemHandle, MapFlags, MemoryType};
use crate::submit::Ib;
use crate::{copy_ib, fence_ib};
use std::fs::File;
use std::io;

/// Where the buffer sits in a space: in the gap between the core's two ranges, where the guest can
/// never place anything, after the copy commands' buffer and before the fence writers.
pub const ADDRESS: u64 = 0x1_0008_0000;
/// Room for one submission's commands. A guest's driver has sent 4 KiB at most.
pub const BYTES: usize = 0x1_0000;

const _: () = {
    assert!(copy_ib::ADDRESS + copy_ib::BYTES as u64 <= ADDRESS);
    assert!(ADDRESS + BYTES as u64 <= fence_ib::ADDRESS);
};

/// Whether a command buffer lies inside the daemon's buffer in a space, where a guest can place
/// nothing. What the video ring is given must.
pub fn holds(ib: &Ib) -> bool {
    let end = ib.address.checked_add(u64::from(ib.bytes));
    ib.address >= ADDRESS && end.is_some_and(|end| end <= ADDRESS + BYTES as u64)
}

/// Why a command buffer can't be copied, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// One submission's command buffers, laid one after another as they will sit in the daemon's
/// buffer, each starting where the video ring takes a command buffer's start.
#[derive(Debug, Default)]
pub struct Copied {
    words: Vec<u32>,
    runs: Vec<Ib>,
}

impl Copied {
    /// Empties it for the next submission, keeping its room.
    pub fn clear(&mut self) {
        self.words.clear();
        self.runs.clear();
    }

    /// Makes room for a command buffer of `bytes` bytes after those already placed, at the next
    /// multiple of `start_alignment` bytes, and returns its dwords to fill in. The dwords between
    /// two buffers are zero.
    pub fn place(&mut self, bytes: u32, start_alignment: u32) -> Result<&mut [u32], Refused> {
        let dword = size_of::<u32>();
        let length = bytes as usize;
        if length == 0 || !length.is_multiple_of(dword) {
            return Err(Refused("not a whole number of dwords"));
        }
        // The card's own alignment is a multiple of a dword. One that isn't would be no card's.
        let alignment = (start_alignment as usize).max(dword).next_multiple_of(dword);
        let start = (self.words.len() * dword).next_multiple_of(alignment);
        if start + length > BYTES {
            return Err(Refused("more than the daemon's buffer holds"));
        }
        self.words.resize((start + length) / dword, 0);
        self.runs.push(Ib { address: ADDRESS + start as u64, bytes });
        Ok(&mut self.words[start / dword..])
    }

    /// Everything placed, from the buffer's start.
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// The command buffers as the card is to run them: where each is in a space, and its length.
    pub fn runs(&self) -> &[Ib] {
        &self.runs
    }

    /// Each command buffer's dwords, in order.
    pub fn buffers(&self) -> impl Iterator<Item = &[u32]> {
        let dword = size_of::<u32>();
        self.runs.iter().map(move |run| {
            let start = (run.address - ADDRESS) as usize / dword;
            &self.words[start..start + run.bytes as usize / dword]
        })
    }
}

/// One space's buffer for the copy, made on the space's own open and mapped there.
#[derive(Debug)]
pub struct VideoIb {
    handle: GemHandle,
    view: Mapping,
}

impl VideoIb {
    /// Makes the buffer and maps it at [`ADDRESS`]. The card only reads it, and uncached, as the
    /// CPU writes it again for each run.
    pub fn make(file: &File, alignment: u64) -> io::Result<Self> {
        let size = (BYTES as u64).next_multiple_of(alignment.max(1));
        let handle = memory::create(file, size, alignment, DOMAIN_GTT, CpuAccess::Required)?;
        let flags =
            MapFlags { readable: true, writable: false, executable: true, memory_type: MemoryType::Uncached };
        let made = Mapping::of(file, handle, size as usize)
            .and_then(|view| memory::map(file, handle, ADDRESS, 0, size, flags).map(|()| view));
        match made {
            Ok(view) => Ok(Self { handle, view }),
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

    /// Writes a submission's commands at the buffer's start. They fit, as [`Copied::place`] takes
    /// no more than [`BYTES`].
    pub fn write(&mut self, copied: &Copied) {
        self.view.write_dwords(copied.words());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What amdgpu says of its video ring: a command buffer starts on a multiple of 256 bytes.
    const START: u32 = 256;

    fn placed(copied: &mut Copied, words: &[u32]) {
        let room = copied.place(size_of_val(words) as u32, START).unwrap();
        room.copy_from_slice(words);
    }

    #[test]
    fn one_command_buffer_sits_at_the_start_of_the_daemons_buffer() {
        let mut copied = Copied::default();
        placed(&mut copied, &[0x10, 0x3000_0001, 3, 0x9c]);
        assert_eq!(copied.runs(), [Ib { address: ADDRESS, bytes: 16 }]);
        assert_eq!(copied.words(), [0x10, 0x3000_0001, 3, 0x9c]);
    }

    #[test]
    fn each_further_buffer_starts_where_the_ring_takes_a_start_with_zeros_between() {
        let mut copied = Copied::default();
        placed(&mut copied, &[1, 2, 3]);
        placed(&mut copied, &[4; 65]);
        placed(&mut copied, &[5]);
        let runs = [
            Ib { address: ADDRESS, bytes: 12 },
            Ib { address: ADDRESS + 256, bytes: 260 },
            Ib { address: ADDRESS + 768, bytes: 4 },
        ];
        assert_eq!(copied.runs(), runs);
        assert_eq!(copied.words().len(), 768 / 4 + 1);
        assert_eq!(copied.words()[..3], [1, 2, 3]);
        assert!(copied.words()[3..64].iter().all(|&word| word == 0));
        assert!(copied.words()[64 + 65..192].iter().all(|&word| word == 0));
        let buffers: Vec<&[u32]> = copied.buffers().collect();
        assert_eq!(buffers, [&[1, 2, 3][..], &[4; 65][..], &[5][..]]);
    }

    #[test]
    fn a_buffer_that_is_not_whole_dwords_or_holds_nothing_is_refused() {
        let mut copied = Copied::default();
        for bytes in [0, 1, 6, 4095] {
            assert_eq!(copied.place(bytes, START).err(), Some(Refused("not a whole number of dwords")));
        }
        assert!(copied.runs().is_empty() && copied.words().is_empty());
    }

    #[test]
    fn commands_past_the_daemons_buffer_are_refused_and_what_was_placed_stays() {
        let mut copied = Copied::default();
        assert_eq!(copied.place(BYTES as u32, START).map(|room| room.len()), Ok(BYTES / 4));
        let full = Refused("more than the daemon's buffer holds");
        assert_eq!(copied.place(4, START).err(), Some(full));
        assert_eq!(copied.place(u32::MAX - 3, START).err(), Some(full));
        assert_eq!(copied.runs().len(), 1);

        copied.clear();
        placed(&mut copied, &[7; 64]);
        assert_eq!(copied.place((BYTES - 256) as u32, START).map(|room| room.len()), Ok((BYTES - 256) / 4));
        assert_eq!(copied.place(4, START).err(), Some(full));
    }

    #[test]
    fn only_what_lies_inside_the_daemons_buffer_is_held_by_it() {
        let mut copied = Copied::default();
        placed(&mut copied, &[1; 3]);
        copied.place(BYTES as u32 - 256, START).unwrap();
        assert!(copied.runs().iter().all(holds), "every run a copy is laid out as");
        let end = ADDRESS + BYTES as u64;
        assert!(holds(&Ib { address: end - 4, bytes: 4 }));
        for outside in [
            Ib { address: ADDRESS - 4, bytes: 8 },
            Ib { address: end - 4, bytes: 8 },
            Ib { address: end, bytes: 4 },
            Ib { address: 0x4_2000_1000, bytes: 0x40 },
            Ib { address: u64::MAX, bytes: 4 },
        ] {
            assert!(!holds(&outside), "{outside:?}");
        }
    }

    #[test]
    fn clearing_starts_over_at_the_buffers_start() {
        let mut copied = Copied::default();
        placed(&mut copied, &[1; 100]);
        copied.clear();
        placed(&mut copied, &[2, 2]);
        assert_eq!(copied.runs(), [Ib { address: ADDRESS, bytes: 8 }]);
        assert_eq!(copied.words(), [2, 2]);
    }

    #[test]
    fn an_alignment_the_card_did_not_give_still_starts_buffers_on_whole_dwords() {
        let mut copied = Copied::default();
        copied.place(4, 0).unwrap();
        copied.place(4, 0).unwrap();
        copied.place(4, 6).unwrap();
        let starts: Vec<u64> = copied.runs().iter().map(|run| run.address - ADDRESS).collect();
        assert_eq!(starts, [0, 4, 8]);
    }
}
