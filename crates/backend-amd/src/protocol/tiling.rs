//! From a surface's swizzle mode to the host buffer that holds it and the format modifier a frame
//! exports with.
//!
//! The guest lays a surface out with addrlib, AMD's open address library. On GFX9 to GFX11 its
//! descriptor names the swizzle mode by the hardware's own SW_MODE number, not by addrlib's
//! AddrSwizzleMode, which the kernel driver translates it to only for its own queries. SW_MODE is
//! the value amdgpu's AMDGPU_TILING_SWIZZLE_MODE and the modifier's AMD_FMT_MOD_TILE field carry.
//! On GFX12 the descriptor holds Addr3SwizzleMode, which matches the GFX12 tiling field. So the mode
//! passes through, and only the modifier's other fields are worked out here, from the card's
//! GB_ADDR_CONFIG, the way amdgpu does it for a buffer that has tiling flags and no modifier.
//!
//! A mode too big for the tiling field gets none. The kernel driver's address library refuses modes
//! past 0x20, but not on the allocation path that was read for a GFX10.3 part, so this is a choice
//! of layout here and not a refusal. 0x20 itself is addrlib's linear-general, which has no hardware
//! number.
//!
//! Every number here is from the open uAPI headers, amdgpu_drm.h and drm_fourcc.h.

use crate::memory::{CpuAccess, DOMAIN_VRAM};

/// DRM_FORMAT_MOD_LINEAR.
pub const LINEAR: u64 = 0;
/// Linear in both swizzle enums.
const SWIZZLE_LINEAR: u32 = 0;

// AMDGPU_TILING_SWIZZLE_MODE and its GFX12 form.
const TILING_SWIZZLE_MASK: u64 = 0x1f;
const TILING_GFX12_SWIZZLE_MASK: u64 = 0x7;

// The modifier, from drm_fourcc.h.
const VENDOR_AMD: u64 = 0x02;
const VENDOR_SHIFT: u32 = 56;
const TILE_VERSION_SHIFT: u32 = 0;
const TILE_SHIFT: u32 = 8;
const PIPE_XOR_BITS_SHIFT: u32 = 21;
const BANK_XOR_BITS_SHIFT: u32 = 24;
const PACKERS_SHIFT: u32 = 27;
const TILE_VER_GFX9: u64 = 1;
const TILE_VER_GFX10: u64 = 2;
const TILE_VER_GFX10_RBPLUS: u64 = 3;
const TILE_VER_GFX11: u64 = 4;
const TILE_VER_GFX12: u64 = 5;

// GB_ADDR_CONFIG's fields, each a power of two given as its log.
const NUM_PIPES_SHIFT: u32 = 0;
const NUM_PKRS_SHIFT: u32 = 8;
const NUM_BANKS_SHIFT: u32 = 12;
const NUM_SHADER_ENGINES_SHIFT: u32 = 19;
const THREE_BITS: u32 = 0b111;
const TWO_BITS: u32 = 0b11;

/// Modes from this one on XOR the address with pipe and bank bits.
const FIRST_XOR_MODE: u32 = 16;
/// The low two bits of a GFX9 to GFX11 mode say how pixels sit inside a block.
const MICRO_MASK: u32 = 0b11;
const MICRO_DEPTH: u32 = 0;
const MICRO_STANDARD: u32 = 1;
const MICRO_DISPLAY: u32 = 2;
/// The display layout keeps the GFX10 tile version only for 4-byte pixels when it has no XOR.
const DISPLAY_PIXEL_BYTES: u32 = 4;

/// A graphics block version, major and minor.
pub type GfxVersion = (u32, u32);

/// Where the tile swizzle sits in a surface's tiling number, above the swizzle mode.
const TILE_SWIZZLE_SHIFT: u32 = 8;
const SWIZZLE_MODE_MASK: u32 = 0xff;

/// A tiled surface's layout, as this backend keeps it in a surface shape's tiling number.
///
/// The tile swizzle is what the guest's driver XORs into the address bits of a surface in an XOR
/// mode, so that surfaces of the same size don't all start on the same memory channel. It is Mesa's
/// `tile_swizzle`: a copy that walks the tiles ORs it into the surface's address, shifted by 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub swizzle_mode: u32,
    pub tile_swizzle: u8,
}

impl Layout {
    /// The tiling number for a surface shape, or `None` for a linear surface.
    pub fn tiling(self) -> Option<u32> {
        let mode = self.swizzle_mode & SWIZZLE_MODE_MASK;
        (mode != SWIZZLE_LINEAR).then_some(mode | u32::from(self.tile_swizzle) << TILE_SWIZZLE_SHIFT)
    }

    /// The layout a tiling number stands for.
    pub fn of(tiling: u32) -> Self {
        let tile_swizzle = (tiling >> TILE_SWIZZLE_SHIFT) as u8;
        Self { swizzle_mode: tiling & SWIZZLE_MODE_MASK, tile_swizzle }
    }

    /// The bits a copy ORs into the surface's address: the tile swizzle in the address bits it stands
    /// for, kept inside the first block so the copy can't start past it. Zero for a mode without XOR.
    /// GFX12 numbers its modes differently and has no tile swizzle in the address.
    pub fn address_bits(self, gfx: GfxVersion) -> u64 {
        if gfx.0 >= 12 || self.swizzle_mode < FIRST_XOR_MODE {
            return 0;
        }
        let Some(block_bits) = block_size_bits(self.swizzle_mode, gfx) else { return 0 };
        (u64::from(self.tile_swizzle) << TILE_SWIZZLE_SHIFT) & ((1 << block_bits) - 1)
    }
}

/// How the host buffer for a surface is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostBuffer {
    pub domain: u64,
    pub cpu_access: CpuAccess,
    /// AMDGPU_GEM_METADATA's tiling_info, zero for a linear buffer.
    pub tiling_info: u64,
}

/// The host buffer for a surface the guest laid out with this swizzle mode. A tiled surface only
/// goes through the GPU, so it may sit in VRAM the CPU can't see.
pub fn host_buffer(swizzle_mode: u32, gfx: GfxVersion) -> HostBuffer {
    let tiling_info = tiling_info(swizzle_mode, gfx);
    let cpu_access = if tiling_info == 0 { CpuAccess::Required } else { CpuAccess::NotNeeded };
    HostBuffer { domain: DOMAIN_VRAM, cpu_access, tiling_info }
}

/// The tiling flags amdgpu keeps on the buffer, so an importer that reads them lays it out the same.
/// Zero, linear, for a mode the field can't hold.
pub fn tiling_info(swizzle_mode: u32, gfx: GfxVersion) -> u64 {
    let mask = if gfx.0 >= 12 { TILING_GFX12_SWIZZLE_MASK } else { TILING_SWIZZLE_MASK };
    let mode = u64::from(swizzle_mode);
    if mode & !mask != 0 { 0 } else { mode }
}

fn field(config: u32, shift: u32, mask: u32) -> u32 {
    config >> shift & mask
}

/// The log of a block's size in bytes, by the mode's group of four.
fn block_size_bits(swizzle_mode: u32, gfx: GfxVersion) -> Option<u32> {
    match swizzle_mode >> 2 {
        0 => Some(8),
        1 | 5 => Some(12),
        2 | 4 | 6 => Some(16),
        // 256 KiB blocks exist from GFX11. Before that this group is the variable-size modes.
        7 if gfx.0 >= 11 => Some(18),
        _ => None,
    }
}

/// The modifier a frame in this layout exports with, or `None` if it has none: a depth layout, a
/// reserved mode, or a display layout whose pixel size decides its version and isn't known.
/// `pixel_bytes` is `None` when the guest's format can't be read.
pub fn modifier(
    swizzle_mode: u32,
    gfx: GfxVersion,
    gb_addr_config: u32,
    pixel_bytes: Option<u32>,
) -> Option<u64> {
    if swizzle_mode == SWIZZLE_LINEAR {
        return Some(LINEAR);
    }
    if tiling_info(swizzle_mode, gfx) == 0 {
        return None;
    }
    let vendor = VENDOR_AMD << VENDOR_SHIFT;
    if gfx.0 >= 12 {
        let tile = u64::from(swizzle_mode) & TILING_GFX12_SWIZZLE_MASK;
        return Some(vendor | TILE_VER_GFX12 << TILE_VERSION_SHIFT | tile << TILE_SHIFT);
    }
    let block_bits = block_size_bits(swizzle_mode, gfx)?;
    let has_xor = swizzle_mode >= FIRST_XOR_MODE;
    let mut version = match gfx {
        (11.., _) => TILE_VER_GFX11,
        (10, 3..) => TILE_VER_GFX10_RBPLUS,
        (10, _) => TILE_VER_GFX10,
        _ => TILE_VER_GFX9,
    };
    match swizzle_mode & MICRO_MASK {
        MICRO_DEPTH => return None,
        MICRO_STANDARD if !has_xor => version = TILE_VER_GFX9,
        MICRO_DISPLAY if !has_xor => match pixel_bytes {
            Some(DISPLAY_PIXEL_BYTES) => {}
            Some(_) => version = TILE_VER_GFX9,
            None => return None,
        },
        _ => {}
    }

    let (mut pipe_xor, mut bank_xor, mut packers) = (0, 0, 0);
    if has_xor {
        let room = block_bits - 8;
        let pipes = field(gb_addr_config, NUM_PIPES_SHIFT, THREE_BITS);
        match version {
            TILE_VER_GFX11 | TILE_VER_GFX10_RBPLUS => {
                pipe_xor = room.min(pipes);
                packers = (room - pipe_xor).min(field(gb_addr_config, NUM_PKRS_SHIFT, THREE_BITS));
            }
            TILE_VER_GFX10 => pipe_xor = room.min(pipes),
            _ => {
                let engines = field(gb_addr_config, NUM_SHADER_ENGINES_SHIFT, TWO_BITS);
                pipe_xor = room.min(pipes + engines);
                bank_xor = (room - pipe_xor).min(field(gb_addr_config, NUM_BANKS_SHIFT, THREE_BITS));
            }
        }
    }
    Some(
        vendor
            | version << TILE_VERSION_SHIFT
            | u64::from(swizzle_mode) << TILE_SHIFT
            | u64::from(pipe_xor) << PIPE_XOR_BITS_SHIFT
            | u64::from(bank_xor) << BANK_XOR_BITS_SHIFT
            | u64::from(packers) << PACKERS_SHIFT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// addrlib's numbers for the modes the tests use.
    const SW_64KB_S: u32 = 9;
    const SW_64KB_D: u32 = 10;
    const SW_64KB_Z_X: u32 = 24;
    const SW_64KB_R_X: u32 = 27;
    const SW_VAR_R_X: u32 = 31;
    const SW3_256KB_2D: u32 = 4;

    /// Four pipes and one packer, as the host's GFX10.3 iGPU reports.
    const SMALL_RDNA2: u32 = 0x42;

    fn mod_field(modifier: u64, shift: u32, mask: u64) -> u64 {
        modifier >> shift & mask
    }

    #[test]
    fn linear_stays_linear_everywhere() {
        assert_eq!(modifier(0, (9, 0), 0, None), Some(LINEAR));
        assert_eq!(modifier(0, (12, 0), 0, None), Some(LINEAR));
        assert_eq!(tiling_info(0, (10, 3)), 0);
        let buffer = host_buffer(0, (10, 3));
        assert_eq!(buffer.cpu_access, CpuAccess::Required);
        assert_eq!(buffer.domain, DOMAIN_VRAM);
    }

    #[test]
    fn a_rdna2_render_target_gets_pipe_and_packer_bits() {
        let modifier = modifier(SW_64KB_R_X, (10, 3), SMALL_RDNA2, Some(4)).unwrap();
        assert_eq!(modifier >> VENDOR_SHIFT, VENDOR_AMD);
        assert_eq!(mod_field(modifier, TILE_VERSION_SHIFT, 0xff), TILE_VER_GFX10_RBPLUS);
        assert_eq!(mod_field(modifier, TILE_SHIFT, 0x1f), u64::from(SW_64KB_R_X));
        assert_eq!(mod_field(modifier, PIPE_XOR_BITS_SHIFT, 0x7), 2);
        assert_eq!(mod_field(modifier, PACKERS_SHIFT, 0x7), 0);
        assert_eq!(mod_field(modifier, BANK_XOR_BITS_SHIFT, 0x7), 0);
    }

    #[test]
    fn the_xor_bits_never_pass_the_block() {
        // Eight pipes and eight packers would want more bits than a 4 KiB block has.
        let config = 3 << NUM_PIPES_SHIFT | 3 << NUM_PKRS_SHIFT;
        let four_k_r_x = 23;
        let modifier = modifier(four_k_r_x, (10, 3), config, Some(4)).unwrap();
        let pipes = mod_field(modifier, PIPE_XOR_BITS_SHIFT, 0x7);
        let packers = mod_field(modifier, PACKERS_SHIFT, 0x7);
        assert_eq!((pipes, packers), (3, 1));
    }

    #[test]
    fn gfx9_spreads_over_engines_and_banks() {
        let config = 2 << NUM_PIPES_SHIFT | 1 << NUM_SHADER_ENGINES_SHIFT | 2 << NUM_BANKS_SHIFT;
        let modifier = modifier(SW_64KB_R_X, (9, 0), config, Some(4)).unwrap();
        assert_eq!(mod_field(modifier, TILE_VERSION_SHIFT, 0xff), TILE_VER_GFX9);
        assert_eq!(mod_field(modifier, PIPE_XOR_BITS_SHIFT, 0x7), 3);
        assert_eq!(mod_field(modifier, BANK_XOR_BITS_SHIFT, 0x7), 2);
    }

    #[test]
    fn plain_standard_and_display_layouts_fall_back_to_the_gfx9_version() {
        let standard = modifier(SW_64KB_S, (10, 3), SMALL_RDNA2, None).unwrap();
        assert_eq!(mod_field(standard, TILE_VERSION_SHIFT, 0xff), TILE_VER_GFX9);
        let display_wide = modifier(SW_64KB_D, (10, 3), SMALL_RDNA2, Some(8)).unwrap();
        assert_eq!(mod_field(display_wide, TILE_VERSION_SHIFT, 0xff), TILE_VER_GFX9);
        let display = modifier(SW_64KB_D, (10, 3), SMALL_RDNA2, Some(4)).unwrap();
        assert_eq!(mod_field(display, TILE_VERSION_SHIFT, 0xff), TILE_VER_GFX10_RBPLUS);
        assert_eq!(modifier(SW_64KB_D, (10, 3), SMALL_RDNA2, None), None);
    }

    #[test]
    fn depth_and_reserved_layouts_have_no_modifier() {
        assert_eq!(modifier(SW_64KB_Z_X, (10, 3), SMALL_RDNA2, Some(4)), None);
        assert_eq!(modifier(13, (10, 3), SMALL_RDNA2, Some(4)), None);
        assert_eq!(modifier(SW_VAR_R_X, (10, 3), SMALL_RDNA2, Some(4)), None);
        assert!(modifier(SW_VAR_R_X, (11, 0), SMALL_RDNA2, Some(4)).is_some());
    }

    #[test]
    fn gfx12_uses_its_own_numbers() {
        let modifier = modifier(SW3_256KB_2D, (12, 0), SMALL_RDNA2, None).unwrap();
        assert_eq!(mod_field(modifier, TILE_VERSION_SHIFT, 0xff), TILE_VER_GFX12);
        assert_eq!(mod_field(modifier, TILE_SHIFT, 0x1f), u64::from(SW3_256KB_2D));
        assert_eq!(tiling_info(0x4, (12, 0)), 0x4);
        assert_eq!(tiling_info(SW_64KB_R_X, (10, 3)), u64::from(SW_64KB_R_X));
    }

    #[test]
    fn a_mode_the_tiling_field_cant_hold_gets_no_tiling() {
        let linear_general = 0x20;
        assert_eq!(tiling_info(linear_general, (10, 3)), 0);
        assert_eq!(modifier(linear_general, (10, 3), SMALL_RDNA2, Some(4)), None);
        assert_eq!(host_buffer(0x21, (10, 3)).cpu_access, CpuAccess::Required);
        assert_eq!(tiling_info(0x1C, (12, 0)), 0);
        assert_eq!(modifier(0x1C, (12, 0), SMALL_RDNA2, None), None);
    }

    #[test]
    fn a_layout_keeps_its_tile_swizzle_above_the_mode() {
        let layout = Layout { swizzle_mode: SW_64KB_R_X, tile_swizzle: 0xe0 };
        assert_eq!(layout.tiling(), Some(0xe01b));
        assert_eq!(Layout::of(0xe01b), layout);
        assert_eq!(Layout { swizzle_mode: 0, tile_swizzle: 0xe0 }.tiling(), None);
    }

    #[test]
    fn the_tile_swizzle_lands_in_address_bits_inside_the_first_block() {
        let swizzled = |swizzle_mode| Layout { swizzle_mode, tile_swizzle: 0xff };
        assert_eq!(swizzled(SW_64KB_R_X).address_bits((10, 3)), 0xff00);
        let four_k_r_x = 23;
        assert_eq!(swizzled(four_k_r_x).address_bits((10, 3)), 0xf00);
        assert_eq!(swizzled(SW_64KB_D).address_bits((10, 3)), 0);
        assert_eq!(swizzled(SW3_256KB_2D).address_bits((12, 0)), 0);
    }

    #[test]
    fn a_tiled_buffer_needs_no_cpu_access() {
        let buffer = host_buffer(SW_64KB_R_X, (10, 3));
        assert_eq!(buffer.cpu_access, CpuAccess::NotNeeded);
        assert_eq!(buffer.tiling_info, u64::from(SW_64KB_R_X));
    }
}
