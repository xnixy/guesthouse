//! Copies with the card's DMA engine: a tiled surface into a linear one, and frames into the
//! viewer's slots, which the CPU would read slowly from video memory.
//!
//! A guest's OpenGL window draws into a tiled surface and presents it as a copy to a linear one.
//! The CPU can't read a tiled layout in order, but the DMA engine's sub-window copy can: given the
//! surface's swizzle mode it walks the tiles itself. The packet is the open one that Mesa's RADV
//! sends on SDMA 5 and 6 (GFX10 to GFX11), with the engine's default cache policy. GFX12's engine
//! (SDMA 7) takes the same packet with a different info dword and a cache policy in the last one.
//!
//! A plain copy moves bytes in order between two addresses: a frame into a slot, or a buffer's
//! contents into new memory. It is the packet the kernel's amdgpu sends to move buffers, which on
//! SDMA 7 is one dword longer.

use crate::protocol::tiling::{GfxVersion, Layout};
use crate::screen_copy::{CopyPlan, PIXEL_BYTES};
use guesthouse_backend::{StagingCopy, SurfaceShape};

// The packet's header: opcode, sub-opcode, and the direction bit.
const OPCODE_COPY: u32 = 0x1;
const SUB_OPCODE_TILED_SUB_WINDOW: u32 = 0x5;
const SUB_OPCODE_SHIFT: u32 = 8;
const DETILE: u32 = 1 << 31;
const OPCODE_NOP: u32 = 0x0;
/// The engine fetches its command buffer in blocks of eight dwords.
const IB_ALIGNMENT_DWORDS: usize = 8;

// The tiled surface's info dword.
const SWIZZLE_SHIFT: u32 = 3;
const DIMENSION_SHIFT: u32 = 9;
const RESOURCE_2D: u32 = 1;
/// log2 of [`PIXEL_BYTES`], the element size field.
const ELEMENT_SIZE_LOG2: u32 = PIXEL_BYTES.trailing_zeros();

/// The engine's limit on each side of a copy, from the smallest field it is carried in.
const MAX_SIDE: u32 = 1 << 14;
/// Swizzle modes 1 to 27 are the tiled ones on GFX10 and GFX11. 0 is linear.
const LAST_SWIZZLE_MODE: u32 = 27;
/// GFX12's tiled modes that are copied: the 2D ones, 1 to 4. 5 to 7 are 3D, and 0 is linear.
const LAST_SWIZZLE_MODE_GFX12: u32 = 4;

/// The side of a swizzle mode's block, in 4-byte pixels, for a 2D surface. Blocks are square at this
/// pixel size. On GFX10 and GFX11 256 bytes are 8 by 8, 4 KiB 32 by 32, and 64 KiB 128 by 128; modes
/// 12 to 15 are the variable-size ones they don't have. GFX12 has 256 bytes, 4 KiB, 64 KiB and
/// 256 KiB, which are 8, 32, 128 and 256 pixels a side.
fn block_side(swizzle_mode: u32, gfx: GfxVersion) -> Option<u64> {
    if gfx.0 >= 12 {
        return match swizzle_mode {
            1 => Some(8),
            2 => Some(32),
            3 => Some(128),
            4 => Some(256),
            _ => None,
        };
    }
    match swizzle_mode {
        1..=3 => Some(8),
        4..=7 | 20..=23 => Some(32),
        8..=11 | 16..=19 | 24..=27 => Some(128),
        _ => None,
    }
}

/// The bytes a tiled surface fills: whole blocks across its rows and down its height.
pub fn surface_bytes(pitch_bytes: u32, height: u32, swizzle_mode: u32, gfx: GfxVersion) -> Option<u64> {
    let side = block_side(swizzle_mode, gfx)?;
    let across = (u64::from(pitch_bytes) / PIXEL_BYTES).div_ceil(side);
    let down = u64::from(height).div_ceil(side);
    Some(across * down * side * side * PIXEL_BYTES)
}

/// A plain copy's sub-opcode: bytes in order, from one address to another.
const SUB_OPCODE_LINEAR: u32 = 0x0;
/// Set in an SDMA 7 plain copy's header: one more dword follows the addresses. amdgpu always sets
/// it, and leaves that dword zero for memory that isn't compressed, which nothing copied here is.
const LINEAR_HAS_LAST_DWORD: u32 = 1 << 19;
/// The most one plain copy moves. The engine's count field is 22 bits on the oldest of these
/// engines, so each packet stays under 4 MiB, kept to whole 256-byte blocks.
const MOST_LINEAR_BYTES: u64 = 0x3f_ff00;

/// A copy of rows between two linear surfaces with the same pitch, which is one run of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinearCopy {
    pub source_address: u64,
    pub target_address: u64,
    pub bytes: u64,
}

impl LinearCopy {
    /// The copy for a plan whose two surfaces have the same pitch, or `None` if they don't, as rows
    /// then aren't one run on both sides. The padding between rows goes along, which is harmless:
    /// it is inside both surfaces.
    pub fn of(plan: &CopyPlan, source_address: u64, target_address: u64) -> Option<Self> {
        if plan.source_pitch != plan.target_pitch || plan.rows == 0 {
            return None;
        }
        Some(Self {
            source_address: source_address + plan.source_start,
            target_address: target_address + plan.target_start,
            bytes: plan.source_end() - plan.source_start,
        })
    }

    /// The command buffer for one engine: one plain copy for each piece, padded to the engine's
    /// alignment.
    pub fn commands(&self, form: EngineForm) -> Vec<u32> {
        let mut header = OPCODE_COPY | SUB_OPCODE_LINEAR << SUB_OPCODE_SHIFT;
        if form == EngineForm::Sdma7 {
            header |= LINEAR_HAS_LAST_DWORD;
        }
        let mut commands = Vec::new();
        let mut done = 0;
        while done < self.bytes {
            let piece = (self.bytes - done).min(MOST_LINEAR_BYTES);
            let (from, to) = (self.source_address + done, self.target_address + done);
            commands.extend([
                header,
                (piece - 1) as u32,
                // No byte swapping on either side.
                0,
                from as u32,
                (from >> 32) as u32,
                to as u32,
                (to >> 32) as u32,
            ]);
            if form == EngineForm::Sdma7 {
                commands.push(0);
            }
            done += piece;
        }
        pad(&mut commands);
        commands
    }
}

fn pad(commands: &mut Vec<u32>) {
    while !commands.len().is_multiple_of(IB_ALIGNMENT_DWORDS) {
        commands.push(OPCODE_NOP);
    }
}

/// Which engine's form of a packet to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineForm {
    /// SDMA 5 and 6, GFX10 and GFX11.
    Sdma5,
    /// SDMA 7, GFX12. A tiled copy has no resource dimension in its info dword and a cache policy
    /// in its last, and a plain copy has one more dword.
    Sdma7,
}

impl EngineForm {
    /// The form a card's engine takes, or `None` for a card whose packets aren't written here.
    pub fn of(gfx: GfxVersion) -> Option<Self> {
        match gfx.0 {
            10 | 11 => Some(Self::Sdma5),
            12 => Some(Self::Sdma7),
            _ => None,
        }
    }
}

/// The memory cache policy field for non-temporal, as the last dword of an SDMA 7 packet has it for
/// each side.
const POLICY_NON_TEMPORAL: u32 = 1;
const TILED_POLICY_SHIFT: u32 = 28;
const LINEAR_POLICY_SHIFT: u32 = 20;

/// A copy of a rectangle from a tiled surface to a linear one, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiledCopy {
    pub tiled_address: u64,
    pub tiled_width: u32,
    pub tiled_height: u32,
    pub swizzle_mode: u32,
    pub form: EngineForm,
    pub linear_address: u64,
    /// The linear surface's row length and height, in pixels.
    pub linear_pitch: u32,
    pub linear_height: u32,
    pub from: (u32, u32),
    pub to: (u32, u32),
    pub width: u32,
    pub height: u32,
}

/// `(x, y)` as the engine takes an offset: x in the low half, y in the high.
fn halves(low: u32, high: u32) -> u32 {
    low | high << 16
}

impl TiledCopy {
    /// The copy for a staging copy from a tiled surface to a linear one, or `None` for any other
    /// pair, or a rectangle past either surface. The source's tile swizzle goes into its address, as
    /// the engine reads the tiles the way the GPU wrote them only from there.
    pub fn of(copy: &StagingCopy, gfx: GfxVersion) -> Option<Self> {
        let layout = Layout::of(copy.source.tiling?);
        let form = EngineForm::of(gfx)?;
        let last_mode = match form {
            EngineForm::Sdma5 => LAST_SWIZZLE_MODE,
            EngineForm::Sdma7 => LAST_SWIZZLE_MODE_GFX12,
        };
        let swizzle_mode = Some(layout.swizzle_mode).filter(|mode| (1..=last_mode).contains(mode))?;
        if copy.target.tiling.is_some() {
            return None;
        }
        let side = |value: i32| u32::try_from(value).ok().filter(|&value| value < MAX_SIDE);
        let width = side(copy.right.checked_sub(copy.left)?)?;
        let height = side(copy.bottom.checked_sub(copy.top)?)?;
        let from = (side(copy.left)?, side(copy.top)?);
        let to = (side(copy.target_left)?, side(copy.target_top)?);
        let fits = |shape: &SurfaceShape, (x, y): (u32, u32)| {
            shape.width < MAX_SIDE
                && shape.height < MAX_SIDE
                && x + width <= shape.width
                && y + height <= shape.height
        };
        let pitch_bytes = u64::from(copy.target.pitch);
        let pitch_fits =
            pitch_bytes % PIXEL_BYTES == 0 && pitch_bytes >= u64::from(copy.target.width) * PIXEL_BYTES;
        if width == 0 || height == 0 || !fits(&copy.source, from) || !fits(&copy.target, to) || !pitch_fits {
            return None;
        }
        Some(Self {
            tiled_address: copy.source_address | layout.address_bits(gfx),
            tiled_width: copy.source.width,
            tiled_height: copy.source.height,
            swizzle_mode,
            form,
            linear_address: copy.target_address,
            linear_pitch: u32::try_from(pitch_bytes / PIXEL_BYTES).ok().filter(|&pitch| pitch <= MAX_SIDE)?,
            linear_height: copy.target.height,
            from,
            to,
            width,
            height,
        })
    }

    /// The command buffer, padded to the engine's alignment.
    pub fn commands(&self) -> Vec<u32> {
        let header = OPCODE_COPY | SUB_OPCODE_TILED_SUB_WINDOW << SUB_OPCODE_SHIFT | DETILE;
        let mut info = ELEMENT_SIZE_LOG2 | self.swizzle_mode << SWIZZLE_SHIFT;
        // The depth of the copy, less one, and on SDMA 7 the cache policy of each side.
        let mut last = 0;
        match self.form {
            EngineForm::Sdma5 => info |= RESOURCE_2D << DIMENSION_SHIFT,
            EngineForm::Sdma7 => {
                last = POLICY_NON_TEMPORAL << TILED_POLICY_SHIFT | POLICY_NON_TEMPORAL << LINEAR_POLICY_SHIFT;
            }
        }
        let linear_slice = self.linear_pitch * self.linear_height;
        let mut commands = vec![
            header,
            self.tiled_address as u32,
            (self.tiled_address >> 32) as u32,
            halves(self.from.0, self.from.1),
            // The tiled z offset, then its width.
            halves(0, self.tiled_width - 1),
            // Its height, then its depth.
            halves(self.tiled_height - 1, 0),
            info,
            self.linear_address as u32,
            (self.linear_address >> 32) as u32,
            halves(self.to.0, self.to.1),
            // The linear z offset, then its pitch.
            halves(0, self.linear_pitch - 1),
            linear_slice - 1,
            halves(self.width - 1, self.height - 1),
            last,
        ];
        pad(&mut commands);
        commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::SpaceId;

    const RDNA2: GfxVersion = (10, 3);
    const RDNA4: GfxVersion = (12, 0);

    fn shape(width: u32, height: u32, tiling: Option<u32>) -> SurfaceShape {
        let format = 21;
        SurfaceShape {
            width,
            height,
            format,
            pitch: width * 4,
            tiling,
            refresh: (0, 0),
            samples: (1, 0),
            colour: format,
        }
    }

    fn copy(source: SurfaceShape, target: SurfaceShape) -> StagingCopy {
        StagingCopy {
            source,
            target,
            source_address: 0x1_2345_0000,
            target_address: 0x2_0000_0000,
            left: 0,
            top: 0,
            right: 640,
            bottom: 400,
            target_left: 0,
            target_top: 0,
            target_guest_address: None,
            is_screen: false,
            own_space: SpaceId(1),
        }
    }

    #[test]
    fn a_window_copies_from_its_tiles_into_rows() {
        let planned = TiledCopy::of(&copy(shape(640, 400, Some(27)), shape(640, 400, None)), RDNA2).unwrap();
        let commands = planned.commands();
        assert_eq!(commands.len(), 16);
        assert_eq!(commands[0], 0x8000_0501);
        assert_eq!(commands[1..3], [0x2345_0000, 0x1]);
        assert_eq!(commands[4], 639 << 16);
        assert_eq!(commands[5], 399);
        assert_eq!(commands[6], 2 | 27 << 3 | 1 << 9);
        assert_eq!(commands[7..9], [0, 0x2]);
        assert_eq!(commands[10], 639 << 16);
        assert_eq!(commands[11], 640 * 400 - 1);
        assert_eq!(commands[12], 639 | 399 << 16);
        assert!(commands[14..].iter().all(|&word| word == OPCODE_NOP));
    }

    #[test]
    fn the_tile_swizzle_goes_into_the_tiled_address() {
        let swizzled = Layout { swizzle_mode: 27, tile_swizzle: 0x80 }.tiling();
        let planned = TiledCopy::of(&copy(shape(640, 400, swizzled), shape(640, 400, None)), RDNA2).unwrap();
        assert_eq!(planned.swizzle_mode, 27);
        assert_eq!(planned.commands()[1..3], [0x2345_8000, 0x1]);
    }

    #[test]
    fn a_window_of_rows_is_one_run_from_its_first_pixel() {
        let frame = shape(1920, 1080, None);
        let plan = CopyPlan::of(&copy(frame, frame)).unwrap();
        let linear = LinearCopy::of(&plan, 0x1_0000_0000, 0x2_0000_0000).unwrap();
        assert_eq!(linear.bytes, 399 * 1920 * 4 + 640 * 4);
        assert_eq!((linear.source_address, linear.target_address), (0x1_0000_0000, 0x2_0000_0000));
    }

    #[test]
    fn a_whole_frame_goes_in_pieces_the_engine_takes() {
        let linear = LinearCopy {
            source_address: 0x1_0000_0000,
            target_address: 0x2_0000_0000,
            bytes: 1920 * 1080 * 4,
        };
        let commands = linear.commands(EngineForm::Sdma5);
        assert_eq!(commands[0..7], [0x1, 0x3f_feff, 0, 0, 0x1, 0, 0x2]);
        let rest = 1920 * 1080 * 4 - MOST_LINEAR_BYTES;
        assert_eq!(commands[7..14], [0x1, rest as u32 - 1, 0, 0x3f_ff00, 0x1, 0x3f_ff00, 0x2]);
        assert_eq!(commands.len(), 16);
        assert!(commands[14..].iter().all(|&word| word == OPCODE_NOP));
    }

    #[test]
    fn a_gfx12_plain_copy_has_one_more_dword() {
        let linear = LinearCopy {
            source_address: 0x1_0000_0000,
            target_address: 0x2_0000_0000,
            bytes: MOST_LINEAR_BYTES + 0x100,
        };
        let commands = linear.commands(EngineForm::Sdma7);
        assert_eq!(commands[0..8], [0x8_0001, 0x3f_feff, 0, 0, 0x1, 0, 0x2, 0]);
        assert_eq!(commands[8..16], [0x8_0001, 0xff, 0, 0x3f_ff00, 0x1, 0x3f_ff00, 0x2, 0]);
        assert_eq!(commands.len(), 16);
    }

    #[test]
    fn each_card_gets_its_engines_form() {
        assert_eq!(EngineForm::of(RDNA2), Some(EngineForm::Sdma5));
        assert_eq!(EngineForm::of((11, 5)), Some(EngineForm::Sdma5));
        assert_eq!(EngineForm::of(RDNA4), Some(EngineForm::Sdma7));
        assert_eq!(EngineForm::of((9, 0)), None);
        assert_eq!(EngineForm::of((13, 0)), None);
    }

    #[test]
    fn surfaces_with_different_pitches_are_left_to_the_cpu() {
        let target = SurfaceShape { pitch: 1920 * 4 + 64, ..shape(1920, 1080, None) };
        let plan = CopyPlan::of(&copy(shape(1920, 1080, None), target)).unwrap();
        assert!(LinearCopy::of(&plan, 0x1_0000_0000, 0x2_0000_0000).is_none());
    }

    #[test]
    fn a_tiled_surface_fills_whole_blocks() {
        assert_eq!(surface_bytes(640 * 4, 400, 27, RDNA2), Some(5 * 4 * 0x10000));
        assert_eq!(surface_bytes(64 * 4, 32, 7, RDNA2), Some(2 * 0x1000));
        assert_eq!(surface_bytes(8 * 4, 9, 1, RDNA2), Some(2 * 0x100));
        assert_eq!(surface_bytes(640 * 4, 400, 13, RDNA2), None);
    }

    #[test]
    fn a_gfx12_surface_fills_whole_blocks_of_its_own_sizes() {
        assert_eq!(surface_bytes(8 * 4, 9, 1, RDNA4), Some(2 * 0x100));
        assert_eq!(surface_bytes(64 * 4, 32, 2, RDNA4), Some(2 * 0x1000));
        assert_eq!(surface_bytes(640 * 4, 400, 3, RDNA4), Some(5 * 4 * 0x10000));
        assert_eq!(surface_bytes(640 * 4, 400, 4, RDNA4), Some(3 * 2 * 0x40000));
        assert_eq!(surface_bytes(640 * 4, 400, 5, RDNA4), None);
        assert_eq!(surface_bytes(640 * 4, 400, 27, RDNA4), None);
    }

    #[test]
    fn gfx12_copies_only_its_2d_modes() {
        let to_rows = shape(640, 400, None);
        assert!(TiledCopy::of(&copy(shape(640, 400, Some(4)), to_rows), RDNA4).is_some());
        assert!(TiledCopy::of(&copy(shape(640, 400, Some(5)), to_rows), RDNA4).is_none());
        assert!(TiledCopy::of(&copy(shape(640, 400, Some(27)), to_rows), RDNA4).is_none());
    }

    #[test]
    fn gfx12_has_no_dimension_and_a_cache_policy() {
        let planned = TiledCopy::of(&copy(shape(640, 400, Some(3)), shape(640, 400, None)), RDNA4).unwrap();
        let commands = planned.commands();
        assert_eq!(commands.len(), 16);
        assert_eq!(commands[0], 0x8000_0501);
        assert_eq!(commands[1..3], [0x2345_0000, 0x1]);
        assert_eq!(commands[6], 2 | 3 << 3);
        assert_eq!(commands[7..9], [0, 0x2]);
        assert_eq!(commands[12], 639 | 399 << 16);
        assert_eq!(commands[13], 0x1010_0000);
        assert!(commands[14..].iter().all(|&word| word == OPCODE_NOP));
    }

    #[test]
    fn only_a_tiled_source_and_a_linear_target_are_taken() {
        assert!(TiledCopy::of(&copy(shape(640, 400, None), shape(640, 400, None)), RDNA2).is_none());
        assert!(TiledCopy::of(&copy(shape(640, 400, Some(27)), shape(640, 400, Some(27))), RDNA2).is_none());
        assert!(TiledCopy::of(&copy(shape(640, 400, Some(0)), shape(640, 400, None)), RDNA2).is_none());
    }

    #[test]
    fn a_rectangle_past_either_surface_is_refused() {
        assert!(TiledCopy::of(&copy(shape(320, 400, Some(27)), shape(640, 400, None)), RDNA2).is_none());
        assert!(TiledCopy::of(&copy(shape(640, 400, Some(27)), shape(640, 200, None)), RDNA2).is_none());
    }
}
