//! The addresses a guest's DMA commands name.
//!
//! The DMA engine's packets carry the addresses they read and write in the clear, so a guest's
//! copy work can be checked before it runs: a packet naming an address with nothing mapped in its
//! space would fault on the card. The graphics and compute engines take their addresses from state
//! and shaders, and can't be checked this way.
//!
//! The layouts are the open ones of SDMA 5 and 6, as the kernel's amdgpu and Mesa's RADV send them.
//! A packet not known here ends the walk, as its length isn't known either, and what follows it goes
//! unchecked. A copy between rectangles gives only where each surface starts, not how far the copy
//! runs into it.
//!
//! The commands are in memory the guest can write, so this catches a driver's mistake, and not a
//! guest that changes them after the check.

/// A run of bytes a packet reads or writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reach {
    pub address: u64,
    pub bytes: u64,
}

// A packet's first dword: its opcode, its sub-opcode, and bits that differ by packet.
const OPCODE_MASK: u32 = 0xff;
const SUB_OPCODE_SHIFT: u32 = 8;
const SUB_OPCODE_MASK: u32 = 0xff;

const OPCODE_NOP: u32 = 0;
const OPCODE_COPY: u32 = 1;
const OPCODE_WRITE: u32 = 2;
const OPCODE_FENCE: u32 = 5;
const OPCODE_TRAP: u32 = 6;
const OPCODE_POLL: u32 = 8;
const OPCODE_CONDITION: u32 = 9;
const OPCODE_FILL: u32 = 11;
const OPCODE_TIMESTAMP: u32 = 13;

const COPY_LINEAR: u32 = 0;
const COPY_LINEAR_WINDOW: u32 = 4;
const COPY_TILED_WINDOW: u32 = 5;
const COPY_TILED_TO_TILED: u32 = 6;
const WRITE_LINEAR: u32 = 0;
const TIMESTAMP_SET: u32 = 0;

/// A NOP says in its header how many dwords after it the engine skips.
const NOP_COUNT_SHIFT: u32 = 16;
const NOP_COUNT_MASK: u32 = 0x3fff;
/// A linear copy with this bit writes to two targets, in a longer packet.
const COPY_BROADCAST: u32 = 1 << 27;
/// A copy to or from a compressed surface carries its metadata in three more dwords.
const COPY_COMPRESSED: u32 = 1 << 19;
const COMPRESSED_EXTRA: usize = 3;
/// A poll with this bit reads memory, and without it a register.
const POLL_MEMORY: u32 = 1 << 31;

/// A copy's or a fill's byte count, less one. The newest of these engines carry 30 bits of it.
const BYTE_COUNT_MASK: u32 = 0x3fff_ffff;
/// A write's dword count, less one.
const WRITE_COUNT_MASK: u32 = 0xf_ffff;

const DWORD_BYTES: u64 = 4;
/// What the engine writes for a timestamp.
const TIMESTAMP_BYTES: u64 = 8;

/// The address in two dwords of a packet, low half first.
fn address(packet: &[u32], at: usize) -> u64 {
    u64::from(packet[at]) | u64::from(packet[at + 1]) << 32
}

/// A packet's length in dwords, and where in it each address is, with how far it reaches from there.
type Layout = (usize, Vec<(usize, Extent)>);

/// The layout of the packet `packet` starts with, by its first dwords. `None` for one not known here.
fn layout(packet: &[u32]) -> Option<Layout> {
    let header = *packet.first()?;
    let sub_opcode = header >> SUB_OPCODE_SHIFT & SUB_OPCODE_MASK;
    let compressed = if header & COPY_COMPRESSED != 0 { COMPRESSED_EXTRA } else { 0 };
    Some(match (header & OPCODE_MASK, sub_opcode) {
        (OPCODE_NOP, _) => (1 + (header >> NOP_COUNT_SHIFT & NOP_COUNT_MASK) as usize, vec![]),
        (OPCODE_COPY, COPY_LINEAR) if header & COPY_BROADCAST == 0 => {
            (7, vec![(3, Extent::BytesLessOneAt(1)), (5, Extent::BytesLessOneAt(1))])
        }
        (OPCODE_COPY, COPY_LINEAR_WINDOW) => (13, vec![(1, Extent::Start), (6, Extent::Start)]),
        (OPCODE_COPY, COPY_TILED_WINDOW) => (14 + compressed, vec![(1, Extent::Start), (7, Extent::Start)]),
        (OPCODE_COPY, COPY_TILED_TO_TILED) => (15 + compressed, vec![(1, Extent::Start), (7, Extent::Start)]),
        (OPCODE_WRITE, WRITE_LINEAR) => {
            let dwords = (*packet.get(3)? & WRITE_COUNT_MASK) as usize + 1;
            (4 + dwords, vec![(1, Extent::Bytes(dwords as u64 * DWORD_BYTES))])
        }
        (OPCODE_FENCE, _) => (4, vec![(1, Extent::Bytes(DWORD_BYTES))]),
        (OPCODE_TRAP, _) => (2, vec![]),
        (OPCODE_POLL, _) if header & POLL_MEMORY != 0 => (6, vec![(1, Extent::Bytes(DWORD_BYTES))]),
        (OPCODE_POLL, _) => (6, vec![]),
        (OPCODE_CONDITION, _) => (5, vec![(1, Extent::Bytes(DWORD_BYTES))]),
        (OPCODE_FILL, _) => (5, vec![(1, Extent::BytesLessOneAt(4))]),
        (OPCODE_TIMESTAMP, TIMESTAMP_SET) => (3, vec![]),
        (OPCODE_TIMESTAMP, _) => (3, vec![(1, Extent::Bytes(TIMESTAMP_BYTES))]),
        _ => return None,
    })
}

/// How far a packet reaches from one of its addresses.
#[derive(Debug, Clone, Copy)]
enum Extent {
    /// Only where the surface starts is known.
    Start,
    Bytes(u64),
    /// The dword at this place in the packet holds the byte count, less one.
    BytesLessOneAt(usize),
}

impl Extent {
    fn bytes(self, packet: &[u32]) -> u64 {
        match self {
            Self::Start => 1,
            Self::Bytes(bytes) => bytes,
            Self::BytesLessOneAt(at) => u64::from(packet[at] & BYTE_COUNT_MASK) + 1,
        }
    }
}

/// Every run of bytes the packets in `commands` name, up to the first packet not known here or cut
/// short by the end of the buffer.
pub fn reaches(commands: &[u32]) -> Vec<Reach> {
    let mut found = Vec::new();
    let mut rest = commands;
    while let Some((length, addresses)) = layout(rest) {
        let Some(packet) = rest.get(..length) else { break };
        for (at, extent) in addresses {
            found.push(Reach { address: address(packet, at), bytes: extent.bytes(packet) });
        }
        rest = &rest[length..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tiling::Layout as Tiling;
    use crate::sdma_copy::{EngineForm, LinearCopy, TiledCopy};
    use crate::submit::sdma_write_ib;

    const SOURCE: u64 = 0x4_5d00_0000;
    const TARGET: u64 = 0x4_5d23_0000;

    #[test]
    fn a_plain_copy_names_both_ends_for_all_its_bytes() {
        let copy = LinearCopy { source_address: SOURCE, target_address: TARGET, bytes: 0x6000 };
        let named = reaches(&copy.commands(EngineForm::Sdma5));
        assert_eq!(
            named,
            [Reach { address: SOURCE, bytes: 0x6000 }, Reach { address: TARGET, bytes: 0x6000 }]
        );
    }

    #[test]
    fn a_copy_in_pieces_names_each_piece() {
        let copy = LinearCopy { source_address: SOURCE, target_address: TARGET, bytes: 0x80_0000 };
        let named = reaches(&copy.commands(EngineForm::Sdma5));
        assert_eq!(named.len(), 6);
        assert_eq!(named.iter().step_by(2).map(|reach| reach.bytes).sum::<u64>(), 0x80_0000);
        assert_eq!(named[2].address, SOURCE + named[0].bytes);
    }

    #[test]
    fn a_copy_out_of_tiles_names_where_both_surfaces_start() {
        let copy = TiledCopy {
            tiled_address: SOURCE | Tiling { swizzle_mode: 27, tile_swizzle: 0x80 }.address_bits((10, 3)),
            tiled_width: 640,
            tiled_height: 400,
            swizzle_mode: 27,
            form: EngineForm::Sdma5,
            linear_address: TARGET,
            linear_pitch: 640,
            linear_height: 400,
            from: (0, 0),
            to: (0, 0),
            width: 640,
            height: 400,
        };
        let named = reaches(&copy.commands());
        assert_eq!(
            named,
            [Reach { address: copy.tiled_address, bytes: 1 }, Reach { address: TARGET, bytes: 1 }]
        );
    }

    #[test]
    fn a_copy_between_tiled_surfaces_names_where_both_start() {
        let header = OPCODE_COPY | COPY_TILED_TO_TILED << SUB_OPCODE_SHIFT;
        let mut copy = [0u32; 15];
        copy[0] = header;
        copy[1] = SOURCE as u32;
        copy[2] = (SOURCE >> 32) as u32;
        copy[7] = TARGET as u32;
        copy[8] = (TARGET >> 32) as u32;
        let named = reaches(&copy);
        assert_eq!(named, [Reach { address: SOURCE, bytes: 1 }, Reach { address: TARGET, bytes: 1 }]);
    }

    #[test]
    fn a_write_names_its_target_for_the_dwords_it_carries() {
        let named = reaches(&sdma_write_ib(TARGET, &[1, 2, 3]));
        assert_eq!(named, [Reach { address: TARGET, bytes: 12 }]);
    }

    #[test]
    fn a_fill_and_a_fence_name_what_they_write() {
        let fill = [OPCODE_FILL, TARGET as u32, (TARGET >> 32) as u32, 0, 0xfff];
        let fence = [OPCODE_FENCE, SOURCE as u32, (SOURCE >> 32) as u32, 7];
        let trap = [OPCODE_TRAP, 0];
        let named = reaches(&[&fill[..], &fence[..], &trap[..]].concat());
        assert_eq!(named, [Reach { address: TARGET, bytes: 0x1000 }, Reach { address: SOURCE, bytes: 4 }]);
    }

    #[test]
    fn what_a_nop_skips_is_not_read_as_packets() {
        let skipped = [OPCODE_COPY, 0xffff, 0];
        let nop = OPCODE_NOP | (skipped.len() as u32) << NOP_COUNT_SHIFT;
        let mut commands = vec![nop];
        commands.extend(skipped);
        commands.extend(sdma_write_ib(TARGET, &[1]));
        assert_eq!(reaches(&commands), [Reach { address: TARGET, bytes: 4 }]);
    }

    #[test]
    fn a_packet_not_known_here_ends_the_walk() {
        let unknown = 0x7f;
        let mut commands = sdma_write_ib(SOURCE, &[1]);
        commands.push(unknown);
        commands.extend(sdma_write_ib(TARGET, &[1]));
        assert_eq!(reaches(&commands), [Reach { address: SOURCE, bytes: 4 }]);
        let broadcast = [OPCODE_COPY | COPY_BROADCAST, 0xfff, 0, 0, 1, 0, 2, 0, 3];
        assert_eq!(reaches(&broadcast), []);
    }

    #[test]
    fn a_packet_cut_short_names_nothing() {
        let copy = LinearCopy { source_address: SOURCE, target_address: TARGET, bytes: 0x1000 };
        assert_eq!(reaches(&copy.commands(EngineForm::Sdma5)[..5]), []);
        assert_eq!(reaches(&[OPCODE_WRITE, TARGET as u32, (TARGET >> 32) as u32]), []);
    }
}
