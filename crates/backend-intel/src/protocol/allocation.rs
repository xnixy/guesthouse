//! An allocation's private data: a copy of the guest's memory manager's description of the
//! resource, gmmlib's GmmResourceInfo, which the kernel driver reads to place the memory.
//!
//! It is [`OBJECT_SIZE`] bytes. When the resource has a companion, a second resource placed after
//! it in the same memory, a trailer follows that says where. The kernel driver takes only those two
//! sizes, and refuses a resource type past [`LAST_RESOURCE_TYPE`]. Nothing is written back.
//!
//! The object starts with a pointer from the guest's process, which means nothing here. Its texture
//! description (`Surf`, gmmlib's GMM_TEXTURE_INFO) starts at [`SURFACE`] and holds what the host
//! needs: the size, the tiling, whether it is compressed, and the cache use, which will choose the
//! page attributes once memory is really mapped. Names are gmmlib's.

use super::bytes::{dword, qword};
use guesthouse_backend::{AllocationShape, MemoryKind, SurfaceShape};

/// The resource object's size.
pub const OBJECT_SIZE: usize = 0x930;
/// The trailer that follows it for a resource with a companion.
pub const TRAILER_SIZE: usize = 0x28;
/// The highest GMM_RESOURCE_TYPE the kernel driver takes.
pub const LAST_RESOURCE_TYPE: u32 = 0x22;
/// GMM_RESOURCE_TYPE's RESOURCE_2D.
const RESOURCE_2D: u32 = 2;
/// The size given for an allocation whose data says nothing usable, a page.
const SMALLEST: u64 = 0x1000;
const PAGE: u64 = 0x1000;

/// Where the object's fields are.
mod at {
    pub const CLIENT_TYPE: usize = 0x008;
    /// GMM_TEXTURE_INFO starts here; the fields after it are inside it.
    pub const SURFACE: usize = 0x010;
    pub const FORMAT: usize = 0x014;
    pub const BITS_PER_PIXEL: usize = 0x018;
    /// Flags.Gpu, a 64-bit word of what the GPU does with it.
    pub const GPU_FLAGS: usize = 0x01C;
    /// Flags.Info, a 64-bit word of how it is laid out.
    pub const INFO_FLAGS: usize = 0x024;
    /// Flags.Wa.
    pub const WA_FLAGS: usize = 0x02C;
    pub const BASE_WIDTH: usize = 0x030;
    pub const BASE_HEIGHT: usize = 0x038;
    pub const ARRAY_SIZE: usize = 0x044;
    /// CachePolicy.Usage, GMM_RESOURCE_USAGE_TYPE.
    pub const USAGE: usize = 0x04C;
    pub const PITCH: usize = 0x108;
    pub const SIZE: usize = 0x118;
    pub const CC_SIZE: usize = 0x120;
    pub const UNPADDED_SIZE: usize = 0x128;
    pub const SIZE_REPORTED: usize = 0x130;
    pub const TILE_MODE: usize = 0x1C8;
    pub const CCS_MODE_ALIGN: usize = 0x1CC;
    /// The user-mode driver's build, which igdgmm writes here too.
    pub const BUILD_STAMP: usize = 0x8B0;
}

/// The trailer's fields, from its start.
mod trailer {
    pub const MARKER: usize = 0x00;
    pub const VERSION: usize = 0x04;
    pub const COMPANION_SIZE: usize = 0x08;
    pub const COMPANION_OFFSET: usize = 0x10;
    pub const FURTHER_OFFSET: usize = 0x18;
    pub const ALIGNMENT: usize = 0x20;
    pub const VALUE: usize = 0x24;
}

/// The four letters that start a trailer. Which way round they are in memory is not known yet, so
/// either is taken and the log says which came.
const MARKER: [u8; 4] = *b"LCTS";
const TRAILER_VERSION: u32 = 1;

/// Flags.Gpu bits.
pub mod gpu_flag {
    pub const CCS: u64 = 1 << 1;
    pub const DEPTH: u64 = 1 << 6;
    pub const HIZ: u64 = 1 << 10;
    pub const MCS: u64 = 1 << 15;
    pub const RENDER_TARGET: u64 = 1 << 23;
    pub const UNIFIED_AUX_SURFACE: u64 = 1 << 37;
}

/// Flags.Info bits.
pub mod info_flag {
    pub const LINEAR: u64 = 1 << 19;
    pub const MEDIA_COMPRESSED: u64 = 1 << 20;
    pub const NOT_LOCKABLE: u64 = 1 << 24;
    pub const RENDER_COMPRESSED: u64 = 1 << 30;
    pub const TILE_4: u64 = 1 << 35;
    pub const TILE_64: u64 = 1 << 36;
}

/// The ranges [`Resource::parse`] reads, so a log line can list what else the object holds.
pub const DECODED: [(usize, usize); 11] = [
    (0, 8),
    (at::CLIENT_TYPE, at::CLIENT_TYPE + 4),
    (at::SURFACE, at::BASE_HEIGHT + 4),
    (at::ARRAY_SIZE, at::ARRAY_SIZE + 4),
    (at::USAGE, at::USAGE + 4),
    (at::PITCH, at::PITCH + 8),
    (at::SIZE, at::SIZE_REPORTED + 8),
    (at::TILE_MODE, at::CCS_MODE_ALIGN + 4),
    (at::BUILD_STAMP, at::BUILD_STAMP + 4),
    (0x7A8, at::BUILD_STAMP),
    (OBJECT_SIZE, OBJECT_SIZE + TRAILER_SIZE),
];

/// Why the kernel driver would refuse this data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// The second resource in the same memory, from the trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Companion {
    pub marker: [u8; 4],
    pub size: u64,
    pub offset: u64,
    pub further_offset: u64,
    pub alignment: u32,
    pub value: u32,
}

/// What an allocation's data says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Resource {
    pub client_type: u32,
    pub resource_type: u32,
    pub format: u32,
    pub bits_per_pixel: u32,
    pub gpu_flags: u64,
    pub info_flags: u64,
    pub wa_flags: u32,
    pub width: u64,
    pub height: u32,
    pub array_size: u32,
    pub usage: u32,
    pub pitch: u64,
    pub size: u64,
    pub cc_size: u64,
    pub unpadded_size: u64,
    pub size_reported: u64,
    pub tile_mode: u32,
    pub ccs_mode_align: u32,
    pub build_stamp: u32,
    pub companion: Option<Companion>,
}

impl Resource {
    /// Reads and checks the data as the kernel driver would. Data of no length is taken, with
    /// nothing in it, as the guest's kernel makes some allocations of its own with none.
    pub fn parse(data: &[u8]) -> Result<Self, Refused> {
        if data.is_empty() {
            return Ok(Self::default());
        }
        if data.len() != OBJECT_SIZE && data.len() != OBJECT_SIZE + TRAILER_SIZE {
            return Err(Refused("its size is neither the resource object's nor that and a trailer"));
        }
        let field = |at| dword(data, at).unwrap_or(0);
        let wide = |at| qword(data, at).unwrap_or(0);
        let resource = Self {
            client_type: field(at::CLIENT_TYPE),
            resource_type: field(at::SURFACE),
            format: field(at::FORMAT),
            bits_per_pixel: field(at::BITS_PER_PIXEL),
            gpu_flags: wide(at::GPU_FLAGS),
            info_flags: wide(at::INFO_FLAGS),
            wa_flags: field(at::WA_FLAGS),
            width: wide(at::BASE_WIDTH),
            height: field(at::BASE_HEIGHT),
            array_size: field(at::ARRAY_SIZE),
            usage: field(at::USAGE),
            pitch: wide(at::PITCH),
            size: wide(at::SIZE),
            cc_size: wide(at::CC_SIZE),
            unpadded_size: wide(at::UNPADDED_SIZE),
            size_reported: wide(at::SIZE_REPORTED),
            tile_mode: field(at::TILE_MODE),
            ccs_mode_align: field(at::CCS_MODE_ALIGN),
            build_stamp: field(at::BUILD_STAMP),
            companion: None,
        };
        if resource.resource_type > LAST_RESOURCE_TYPE {
            return Err(Refused("its resource type is past the last the kernel driver takes"));
        }
        let tail = &data[OBJECT_SIZE..];
        if tail.is_empty() {
            return Ok(resource);
        }
        let marker: [u8; 4] = tail[trailer::MARKER..trailer::MARKER + 4].try_into().expect("four bytes");
        let reversed = [MARKER[3], MARKER[2], MARKER[1], MARKER[0]];
        if (marker != MARKER && marker != reversed) || dword(tail, trailer::VERSION) != Some(TRAILER_VERSION)
        {
            return Err(Refused("its trailer has no marker or isn't the version known"));
        }
        let companion = Companion {
            marker,
            size: qword(tail, trailer::COMPANION_SIZE).unwrap_or(0),
            offset: qword(tail, trailer::COMPANION_OFFSET).unwrap_or(0),
            further_offset: qword(tail, trailer::FURTHER_OFFSET).unwrap_or(0),
            alignment: dword(tail, trailer::ALIGNMENT).unwrap_or(0),
            value: dword(tail, trailer::VALUE).unwrap_or(0),
        };
        Ok(Self { companion: Some(companion), ..resource })
    }

    /// Whether the guest asked for any kind of compression, which it shouldn't with the record's
    /// flags clear.
    pub fn compressed(&self) -> bool {
        self.info_flags & (info_flag::RENDER_COMPRESSED | info_flag::MEDIA_COMPRESSED) != 0
            || self.gpu_flags & (gpu_flag::CCS | gpu_flag::UNIFIED_AUX_SURFACE) != 0
    }

    pub fn linear(&self) -> bool {
        self.info_flags & info_flag::LINEAR != 0
    }

    /// The bytes the memory must hold: the resource, and its companion where that ends later.
    pub fn bytes(&self) -> u64 {
        let own = if self.size != 0 { self.size } else { self.size_reported };
        let companion = self.companion.map_or(0, |companion| companion.offset.saturating_add(companion.size));
        own.max(companion)
    }

    /// The shape the core gives the allocation. Video memory is whole pages; memory the guest
    /// lends is locked to the byte.
    pub fn shape(&self) -> AllocationShape {
        let exact = self.bytes().max(SMALLEST);
        AllocationShape {
            size: exact.div_ceil(PAGE) * PAGE,
            exact_size: exact,
            surface: self.surface(),
            gpu_cached: false,
            kind: MemoryKind(self.usage),
        }
    }

    /// A 2D resource's shape, for copies to the screen. The format word the guest's kernel reads
    /// is left unknown, as gmmlib's formats are not Windows' own.
    fn surface(&self) -> Option<SurfaceShape> {
        if self.resource_type != RESOURCE_2D {
            return None;
        }
        let (width, pitch) = (u32::try_from(self.width).ok()?, u32::try_from(self.pitch).ok()?);
        if width == 0 || self.height == 0 || pitch == 0 {
            return None;
        }
        Some(SurfaceShape {
            width,
            height: self.height,
            format: FORMAT_UNKNOWN,
            pitch,
            tiling: (!self.linear()).then_some(self.tile_mode),
            refresh: (0, 0),
            samples: (1, 0),
            colour: self.format,
        })
    }
}

/// D3DDDIFMT_UNKNOWN.
const FORMAT_UNKNOWN: u32 = 0;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::bytes::{put_dword, put_qword};

    /// A 256 by 64 render target in rows, as the guest's memory manager would describe it.
    pub fn render_target() -> Vec<u8> {
        let mut data = vec![0u8; OBJECT_SIZE];
        put_qword(&mut data, 0, 0x7FF8_1234_5678);
        put_dword(&mut data, at::SURFACE, RESOURCE_2D);
        put_dword(&mut data, at::FORMAT, 0x2B);
        put_dword(&mut data, at::BITS_PER_PIXEL, 32);
        put_qword(&mut data, at::GPU_FLAGS, gpu_flag::RENDER_TARGET);
        put_qword(&mut data, at::INFO_FLAGS, info_flag::LINEAR);
        put_qword(&mut data, at::BASE_WIDTH, 256);
        put_dword(&mut data, at::BASE_HEIGHT, 64);
        put_dword(&mut data, at::USAGE, 0x55);
        put_qword(&mut data, at::PITCH, 1024);
        put_qword(&mut data, at::SIZE, 0x1_0000);
        put_dword(&mut data, at::BUILD_STAMP, 0x000F_0001);
        data
    }

    fn with_trailer(marker: [u8; 4], version: u32) -> Vec<u8> {
        let mut data = render_target();
        data.resize(OBJECT_SIZE + TRAILER_SIZE, 0);
        let trailer = &mut data[OBJECT_SIZE..];
        trailer[..4].copy_from_slice(&marker);
        put_dword(trailer, trailer::VERSION, version);
        put_qword(trailer, trailer::COMPANION_SIZE, 0x8000);
        put_qword(trailer, trailer::COMPANION_OFFSET, 0x1_0000);
        put_dword(trailer, trailer::ALIGNMENT, 0x1000);
        data
    }

    #[test]
    fn a_render_target_is_read_field_by_field() {
        let resource = Resource::parse(&render_target()).unwrap();
        assert_eq!((resource.resource_type, resource.format, resource.bits_per_pixel), (2, 0x2B, 32));
        assert_eq!((resource.width, resource.height, resource.pitch), (256, 64, 1024));
        assert_eq!((resource.size, resource.usage, resource.build_stamp), (0x1_0000, 0x55, 0x000F_0001));
        assert!(resource.linear() && !resource.compressed());
        assert_eq!(resource.companion, None);
    }

    #[test]
    fn its_shape_is_its_size_and_rows() {
        let shape = Resource::parse(&render_target()).unwrap().shape();
        assert_eq!((shape.size, shape.exact_size, shape.kind), (0x1_0000, 0x1_0000, MemoryKind(0x55)));
        let surface = shape.surface.unwrap();
        assert_eq!((surface.width, surface.height, surface.pitch, surface.tiling), (256, 64, 1024, None));
        assert_eq!((surface.format, surface.colour), (FORMAT_UNKNOWN, 0x2B));
    }

    #[test]
    fn a_tiled_surface_keeps_its_tile_mode() {
        let mut data = render_target();
        put_qword(&mut data, at::INFO_FLAGS, info_flag::TILE_4);
        put_dword(&mut data, at::TILE_MODE, 9);
        let surface = Resource::parse(&data).unwrap().shape().surface.unwrap();
        assert_eq!(surface.tiling, Some(9));
    }

    #[test]
    fn a_buffer_has_no_surface_and_an_odd_size_is_locked_exactly() {
        let mut data = vec![0u8; OBJECT_SIZE];
        put_dword(&mut data, at::SURFACE, 6);
        put_qword(&mut data, at::SIZE, 0x1234);
        let shape = Resource::parse(&data).unwrap().shape();
        assert_eq!((shape.size, shape.exact_size, shape.surface), (0x2000, 0x1234, None));
    }

    #[test]
    fn the_reported_size_stands_in_when_the_size_is_zero() {
        let mut data = vec![0u8; OBJECT_SIZE];
        put_qword(&mut data, at::SIZE_REPORTED, 0x3000);
        assert_eq!(Resource::parse(&data).unwrap().bytes(), 0x3000);
    }

    #[test]
    fn a_companion_makes_the_memory_cover_it_too() {
        for marker in [*b"LCTS", *b"STCL"] {
            let resource = Resource::parse(&with_trailer(marker, TRAILER_VERSION)).unwrap();
            let companion = resource.companion.unwrap();
            assert_eq!((companion.marker, companion.size, companion.offset), (marker, 0x8000, 0x1_0000));
            assert_eq!(resource.bytes(), 0x1_8000);
        }
    }

    #[test]
    fn a_trailer_without_its_marker_or_version_is_refused() {
        assert!(Resource::parse(&with_trailer(*b"ABCD", TRAILER_VERSION)).is_err());
        assert!(Resource::parse(&with_trailer(MARKER, 2)).is_err());
    }

    #[test]
    fn only_the_two_sizes_are_taken() {
        for size in [4, OBJECT_SIZE - 1, OBJECT_SIZE + 1, OBJECT_SIZE + TRAILER_SIZE + 4] {
            assert!(Resource::parse(&vec![0; size]).is_err(), "{size:#x}");
        }
        assert_eq!(Resource::parse(&[]), Ok(Resource::default()));
    }

    #[test]
    fn a_resource_type_past_the_last_is_refused() {
        let mut data = render_target();
        put_dword(&mut data, at::SURFACE, LAST_RESOURCE_TYPE + 1);
        assert!(Resource::parse(&data).is_err());
        put_dword(&mut data, at::SURFACE, LAST_RESOURCE_TYPE);
        assert!(Resource::parse(&data).is_ok());
    }

    #[test]
    fn compression_shows_in_any_of_its_flags() {
        for (word, flag) in [
            (at::INFO_FLAGS, info_flag::RENDER_COMPRESSED),
            (at::INFO_FLAGS, info_flag::MEDIA_COMPRESSED),
            (at::GPU_FLAGS, gpu_flag::CCS),
            (at::GPU_FLAGS, gpu_flag::UNIFIED_AUX_SURFACE),
        ] {
            let mut data = render_target();
            put_qword(&mut data, word, flag);
            assert!(Resource::parse(&data).unwrap().compressed(), "{flag:#x}");
        }
    }
}
