//! An allocation's private data: the header, a surface descriptor per surface, and an optional
//! second information block after them.
//!
//! The guest's user-mode driver computes all of it, and the kernel driver only checks it, so the
//! check here refuses what the kernel would refuse and the data goes back to the guest unchanged.
//! Nothing in a guest-built buffer is written back: the kernel fills in descriptors only for the
//! standard surfaces it makes itself, which the core answers on its own.

use super::bytes::{dword, index_xor_sum, qword};
use guesthouse_backend::{AllocationShape, MemoryKind, SurfaceShape};

/// The size of one surface descriptor, VCAM_SURFACE_DESC in AMD's open interop header.
pub const SURFACE_SIZE: usize = 0x120;
/// The size of the second information block.
pub const SECOND_BLOCK_SIZE: usize = 0x114;
/// The largest buffer the kernel takes, 128 KiB.
pub const MAX_SIZE: usize = 0x20000;

// The header.
/// The guest's own count of the buffer's bytes.
const CLAIMED_SIZE: usize = 0x00;
const FLAGS: usize = 0x04;
/// Set: the size is the 64-bit [`SIZE_64`]. Clear: the 32-bit [`SIZE_32`].
const FLAG_64_BIT_SIZE: u32 = 1 << 0;
/// A buffer descriptor's own size field, which must be [`BUFFER_DESCRIPTOR_SIZE`].
const BUFFER_DESCRIPTOR: usize = 0x08;
const BUFFER_DESCRIPTOR_SIZE: u32 = 0x80;
/// A client handle, which a guest allocation must leave zero.
const CLIENT_HANDLE: usize = 0x0C;
const VM_FLAGS: usize = 0x10;
/// Three ways of asking for special placement, of which at most one may be set.
const VM_PLACEMENT_REQUESTS: u32 = 0b111 << 18;
const SIZE_32: usize = 0x14;
const ALIGNMENT: usize = 0x18;
const HINTS: usize = 0x1C;
/// The hints must be below this.
const HINTS_LIMIT: u32 = 0x40000;
/// The low bits of the hints count the heap type bytes that follow.
const HEAP_COUNT_MASK: u32 = 0b111;
const HEAP_TYPES: usize = 0x20;
/// Each heap type is a number below this.
const HEAP_TYPE_LIMIT: u8 = 0xA;
const SIZE_64: usize = 0x68;
const SURFACE_COUNT: usize = 0x88;
/// The guest's [`index_xor_sum`] over the claimed size. Zero means the guest sent none.
const CHECKSUM: usize = 0x8C;
/// Where the second information block starts. Zero means there is none.
const SECOND_BLOCK_OFFSET: usize = 0xB0;
const FIRST_SURFACE: usize = 0xC4;

// A surface descriptor, by offset from its start.
const SURFACE_OWN_SIZE: usize = 0x00;
const SURFACE_FLAGS: usize = 0x04;
/// The guest says this surface needs no check of its shape.
const SURFACE_SKIP_SHAPE_CHECK: u32 = 1 << 15;
/// The addrlib swizzle mode the guest laid the surface out with.
const SURFACE_SWIZZLE_MODE: usize = 0x08;
/// 0 to 3. 1, 2 and 3 are 1D, 2D and 3D.
const SURFACE_DIMENSIONS: usize = 0x0C;
const MAX_DIMENSIONS: u32 = 3;
const SURFACE_FORMAT: usize = 0x10;
const SURFACE_WIDTH: usize = 0x14;
const SURFACE_HEIGHT: usize = 0x18;
const SURFACE_PITCH: usize = 0x1C;

/// The second block's own size field.
const SECOND_BLOCK_OWN_SIZE: usize = 0x00;

const PAGE: u64 = 0x1000;
const DEFAULT_SIZE: u64 = PAGE;

/// Why the kernel driver would refuse this private data, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// One surface, as its descriptor says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    pub flags: u32,
    pub swizzle_mode: u32,
    pub dimensions: u32,
    /// The guest driver's own pixel format number.
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

/// What a valid buffer asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub size: u64,
    pub alignment: u32,
    pub heap_types: Vec<u8>,
    pub surfaces: Vec<Surface>,
    pub has_second_block: bool,
}

/// The least the buffer must hold for this many surfaces. With neither surfaces nor a second block
/// there is still room for one surface.
fn least_size(surfaces: usize, second_block: bool) -> Option<usize> {
    let room = if surfaces == 0 && !second_block { 1 } else { surfaces };
    let surfaces = room.checked_mul(SURFACE_SIZE)?;
    FIRST_SURFACE.checked_add(surfaces)?.checked_add(if second_block { SECOND_BLOCK_SIZE } else { 0 })
}

fn surface_at(data: &[u8], index: usize) -> Result<Surface, Refused> {
    let start = FIRST_SURFACE + index * SURFACE_SIZE;
    let field = |offset: usize| dword(data, start + offset).ok_or(Refused("a surface runs past the end"));
    let surface = Surface {
        flags: field(SURFACE_FLAGS)?,
        swizzle_mode: field(SURFACE_SWIZZLE_MODE)?,
        dimensions: field(SURFACE_DIMENSIONS)?,
        format: field(SURFACE_FORMAT)?,
        width: field(SURFACE_WIDTH)?,
        height: field(SURFACE_HEIGHT)?,
        pitch: field(SURFACE_PITCH)?,
    };
    if index == 0 && field(SURFACE_OWN_SIZE)? != SURFACE_SIZE as u32 {
        return Err(Refused("the first surface descriptor has the wrong size"));
    }
    if surface.flags & SURFACE_SKIP_SHAPE_CHECK == 0 {
        if surface.dimensions > MAX_DIMENSIONS {
            return Err(Refused("a surface has more than three dimensions"));
        }
        if surface.width == 0 || surface.height == 0 || surface.pitch == 0 {
            return Err(Refused("a surface has no width, height or pitch"));
        }
    }
    Ok(surface)
}

/// Checks the buffer as the kernel driver would, and reads what it asks for.
pub fn parse(data: &[u8]) -> Result<Allocation, Refused> {
    let field = |at: usize| dword(data, at).ok_or(Refused("the header runs past the end"));
    let claimed = field(CLAIMED_SIZE)? as usize;
    if claimed > MAX_SIZE || claimed > data.len() {
        return Err(Refused("the claimed size is more than was sent"));
    }
    let data = &data[..claimed];
    let surface_count = field(SURFACE_COUNT)? as usize;
    let second_offset = field(SECOND_BLOCK_OFFSET)? as usize;
    let has_second_block = second_offset != 0;
    if least_size(surface_count, has_second_block).is_none_or(|least| claimed < least) {
        return Err(Refused("the buffer is too small for its surfaces"));
    }

    let flags = field(FLAGS)?;
    let size = if flags & FLAG_64_BIT_SIZE != 0 {
        qword(data, SIZE_64).ok_or(Refused("the header runs past the end"))?
    } else {
        u64::from(field(SIZE_32)?)
    };
    if size == 0 {
        return Err(Refused("no size"));
    }
    if field(BUFFER_DESCRIPTOR)? != BUFFER_DESCRIPTOR_SIZE {
        return Err(Refused("the buffer descriptor has the wrong size"));
    }
    if field(CLIENT_HANDLE)? != 0 {
        return Err(Refused("a client handle is set"));
    }
    if (field(VM_FLAGS)? & VM_PLACEMENT_REQUESTS).count_ones() > 1 {
        return Err(Refused("more than one placement is asked for"));
    }
    let alignment = field(ALIGNMENT)?;
    if alignment == 0 {
        return Err(Refused("no alignment"));
    }
    let hints = field(HINTS)?;
    if hints >= HINTS_LIMIT {
        return Err(Refused("the hints are out of range"));
    }
    let heap_count = (hints & HEAP_COUNT_MASK) as usize;
    let heap_types = data[HEAP_TYPES..HEAP_TYPES + heap_count].to_vec();
    if heap_types.iter().any(|&heap| heap >= HEAP_TYPE_LIMIT) {
        return Err(Refused("a heap type is out of range"));
    }

    if has_second_block {
        let end = second_offset.checked_add(SECOND_BLOCK_SIZE);
        if second_offset < FIRST_SURFACE || end.is_none_or(|end| end > claimed) {
            return Err(Refused("the second block is out of place"));
        }
        if field(second_offset + SECOND_BLOCK_OWN_SIZE)? != SECOND_BLOCK_SIZE as u32 {
            return Err(Refused("the second block has the wrong size"));
        }
    }
    let surfaces = (0..surface_count).map(|index| surface_at(data, index)).collect::<Result<_, _>>()?;

    let stored = field(CHECKSUM)?;
    if stored != 0 && index_xor_sum(data, claimed / 4, CHECKSUM) != stored {
        return Err(Refused("the checksum doesn't match"));
    }
    Ok(Allocation { size, alignment, heap_types, surfaces, has_second_block })
}

/// The shape the core makes the allocation in. Data the kernel would refuse gets a page, and
/// [`parse`] then refuses the call before anything is made.
pub fn shape(data: &[u8]) -> AllocationShape {
    let Ok(allocation) = parse(data) else {
        return AllocationShape {
            size: DEFAULT_SIZE,
            exact_size: DEFAULT_SIZE,
            surface: None,
            gpu_cached: false,
            kind: MemoryKind::PLAIN,
        };
    };
    let grain = u64::from(allocation.alignment).max(PAGE);
    let size = allocation.size.div_ceil(grain).saturating_mul(grain);
    let first = allocation.surfaces.first();
    AllocationShape {
        size,
        exact_size: allocation.size,
        surface: first.map(|surface| SurfaceShape {
            width: surface.width,
            height: surface.height,
            format: surface.format,
            pitch: surface.pitch,
            tiling: (surface.swizzle_mode != 0).then_some(surface.swizzle_mode),
            refresh: (0, 1),
            samples: (1, 0),
            colour: surface.format,
        }),
        gpu_cached: false,
        // The swizzle mode, so the allocation path can tile the host buffer to match.
        kind: first.map_or(MemoryKind::PLAIN, |surface| MemoryKind(surface.swizzle_mode)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::{put_dword, put_qword};

    /// Builds a buffer the way the guest's driver lays one out.
    struct Builder {
        surfaces: Vec<Surface>,
        second_block: bool,
        size: u64,
        wide_size: bool,
        checksum: bool,
        heap_types: Vec<u8>,
    }

    impl Default for Builder {
        fn default() -> Self {
            Self {
                surfaces: Vec::new(),
                second_block: false,
                size: 0x10000,
                wide_size: false,
                checksum: false,
                heap_types: vec![1],
            }
        }
    }

    fn surface(width: u32, height: u32, swizzle_mode: u32) -> Surface {
        Surface { flags: 0, swizzle_mode, dimensions: 2, format: 1, width, height, pitch: width }
    }

    impl Builder {
        fn build(&self) -> Vec<u8> {
            let length = if self.second_block {
                FIRST_SURFACE + self.surfaces.len() * SURFACE_SIZE + SECOND_BLOCK_SIZE
            } else {
                FIRST_SURFACE + self.surfaces.len().max(1) * SURFACE_SIZE
            };
            let mut data = vec![0u8; length];
            put_dword(&mut data, CLAIMED_SIZE, length as u32);
            put_dword(&mut data, BUFFER_DESCRIPTOR, BUFFER_DESCRIPTOR_SIZE);
            put_dword(&mut data, ALIGNMENT, 0x1000);
            put_dword(&mut data, HINTS, self.heap_types.len() as u32);
            data[HEAP_TYPES..HEAP_TYPES + self.heap_types.len()].copy_from_slice(&self.heap_types);
            if self.wide_size {
                put_dword(&mut data, FLAGS, FLAG_64_BIT_SIZE);
                put_qword(&mut data, SIZE_64, self.size);
            } else {
                put_dword(&mut data, SIZE_32, self.size as u32);
            }
            put_dword(&mut data, SURFACE_COUNT, self.surfaces.len() as u32);
            for (index, surface) in self.surfaces.iter().enumerate() {
                let start = FIRST_SURFACE + index * SURFACE_SIZE;
                put_dword(&mut data, start + SURFACE_OWN_SIZE, SURFACE_SIZE as u32);
                put_dword(&mut data, start + SURFACE_FLAGS, surface.flags);
                put_dword(&mut data, start + SURFACE_SWIZZLE_MODE, surface.swizzle_mode);
                put_dword(&mut data, start + SURFACE_DIMENSIONS, surface.dimensions);
                put_dword(&mut data, start + SURFACE_FORMAT, surface.format);
                put_dword(&mut data, start + SURFACE_WIDTH, surface.width);
                put_dword(&mut data, start + SURFACE_HEIGHT, surface.height);
                put_dword(&mut data, start + SURFACE_PITCH, surface.pitch);
            }
            if self.second_block {
                let at = FIRST_SURFACE + self.surfaces.len() * SURFACE_SIZE;
                put_dword(&mut data, SECOND_BLOCK_OFFSET, at as u32);
                put_dword(&mut data, at + SECOND_BLOCK_OWN_SIZE, SECOND_BLOCK_SIZE as u32);
            }
            if self.checksum {
                let sum = index_xor_sum(&data, length / 4, CHECKSUM);
                put_dword(&mut data, CHECKSUM, sum);
            }
            data
        }
    }

    fn refused(data: &[u8]) -> &'static str {
        parse(data).unwrap_err().0
    }

    #[test]
    fn a_plain_buffer_is_taken_and_sized_to_its_alignment() {
        let data = Builder { size: 0x1234, ..Builder::default() }.build();
        let allocation = parse(&data).unwrap();
        assert_eq!(allocation.size, 0x1234);
        assert!(allocation.surfaces.is_empty());
        let shape = shape(&data);
        assert_eq!(shape.size, 0x2000);
        assert_eq!(shape.exact_size, 0x1234);
        assert_eq!(shape.surface, None);
        assert_eq!(shape.kind, MemoryKind::PLAIN);
    }

    #[test]
    fn the_wide_size_is_used_when_the_flag_says() {
        let data = Builder { size: 0x2_0000_0000, wide_size: true, ..Builder::default() }.build();
        assert_eq!(parse(&data).unwrap().size, 0x2_0000_0000);
    }

    #[test]
    fn surfaces_and_the_second_block_are_read() {
        let surfaces = vec![surface(1920, 1080, 27), surface(960, 540, 27)];
        let data = Builder { surfaces, second_block: true, checksum: true, ..Builder::default() }.build();
        let allocation = parse(&data).unwrap();
        assert!(allocation.has_second_block);
        assert_eq!(allocation.surfaces.len(), 2);
        let shape = shape(&data);
        let surface = shape.surface.unwrap();
        assert_eq!((surface.width, surface.height, surface.tiling), (1920, 1080, Some(27)));
        assert_eq!(shape.kind, MemoryKind(27));
    }

    #[test]
    fn a_second_block_with_no_surfaces_fits_in_less_room() {
        let data = Builder { second_block: true, ..Builder::default() }.build();
        assert_eq!(data.len(), FIRST_SURFACE + SECOND_BLOCK_SIZE);
        assert!(parse(&data).is_ok());
    }

    #[test]
    fn a_good_checksum_passes_and_a_bad_one_does_not() {
        let mut data =
            Builder { surfaces: vec![surface(64, 64, 0)], checksum: true, ..Builder::default() }.build();
        assert!(parse(&data).is_ok());
        data[FIRST_SURFACE + SURFACE_FORMAT] ^= 1;
        assert_eq!(refused(&data), "the checksum doesn't match");
    }

    #[test]
    fn a_claimed_size_past_what_was_sent_is_refused() {
        let mut data = Builder::default().build();
        let past_the_end = data.len() as u32 + 4;
        put_dword(&mut data, CLAIMED_SIZE, past_the_end);
        assert_eq!(refused(&data), "the claimed size is more than was sent");
        let mut huge = vec![0u8; MAX_SIZE + 4];
        put_dword(&mut huge, CLAIMED_SIZE, (MAX_SIZE + 4) as u32);
        assert_eq!(refused(&huge), "the claimed size is more than was sent");
    }

    #[test]
    fn too_many_surfaces_for_the_room_is_refused() {
        let mut data = Builder { surfaces: vec![surface(8, 8, 0)], ..Builder::default() }.build();
        put_dword(&mut data, SURFACE_COUNT, 2);
        assert_eq!(refused(&data), "the buffer is too small for its surfaces");
        put_dword(&mut data, SURFACE_COUNT, u32::MAX);
        assert_eq!(refused(&data), "the buffer is too small for its surfaces");
    }

    #[test]
    fn header_rules_are_kept() {
        let good = Builder::default().build();
        let with = |at: usize, value: u32| {
            let mut data = good.clone();
            put_dword(&mut data, at, value);
            parse(&data).map(|_| ()).map_err(|refused| refused.0)
        };
        assert_eq!(with(SIZE_32, 0), Err("no size"));
        assert_eq!(with(BUFFER_DESCRIPTOR, 0x40), Err("the buffer descriptor has the wrong size"));
        assert_eq!(with(CLIENT_HANDLE, 1), Err("a client handle is set"));
        assert_eq!(with(VM_FLAGS, 0b11 << 18), Err("more than one placement is asked for"));
        assert_eq!(with(VM_FLAGS, 1 << 19), Ok(()));
        assert_eq!(with(ALIGNMENT, 0), Err("no alignment"));
        assert_eq!(with(HINTS, HINTS_LIMIT), Err("the hints are out of range"));
    }

    #[test]
    fn a_heap_type_out_of_range_is_refused() {
        let data = Builder { heap_types: vec![2, HEAP_TYPE_LIMIT], ..Builder::default() }.build();
        assert_eq!(refused(&data), "a heap type is out of range");
    }

    #[test]
    fn surface_rules_are_kept_unless_the_guest_skips_them() {
        let mut flat = surface(64, 0, 0);
        let data = Builder { surfaces: vec![flat], ..Builder::default() }.build();
        assert_eq!(refused(&data), "a surface has no width, height or pitch");
        flat.flags = SURFACE_SKIP_SHAPE_CHECK;
        assert!(parse(&Builder { surfaces: vec![flat], ..Builder::default() }.build()).is_ok());
        let mut deep = surface(8, 8, 0);
        deep.dimensions = 4;
        let data = Builder { surfaces: vec![deep], ..Builder::default() }.build();
        assert_eq!(refused(&data), "a surface has more than three dimensions");
    }

    #[test]
    fn the_first_descriptor_must_give_its_size() {
        let mut data = Builder { surfaces: vec![surface(8, 8, 0)], ..Builder::default() }.build();
        put_dword(&mut data, FIRST_SURFACE + SURFACE_OWN_SIZE, 0x100);
        assert_eq!(refused(&data), "the first surface descriptor has the wrong size");
    }

    #[test]
    fn the_second_block_must_be_in_place_and_sized() {
        let good = Builder { second_block: true, ..Builder::default() }.build();
        let mut early = good.clone();
        put_dword(&mut early, SECOND_BLOCK_OFFSET, 0x40);
        assert_eq!(refused(&early), "the second block is out of place");
        let mut late = good.clone();
        put_dword(&mut late, SECOND_BLOCK_OFFSET, u32::MAX);
        assert_eq!(refused(&late), "the second block is out of place");
        let mut wrong = good;
        put_dword(&mut wrong, FIRST_SURFACE + SECOND_BLOCK_OWN_SIZE, 0x100);
        assert_eq!(refused(&wrong), "the second block has the wrong size");
    }

    #[test]
    fn short_or_empty_data_is_refused_and_gets_a_page() {
        for data in [&[][..], &[0u8; 3][..], &[0u8; 0x40][..]] {
            assert!(parse(data).is_err());
            assert_eq!(shape(data).size, DEFAULT_SIZE);
        }
    }
}
