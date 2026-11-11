//! Which byte of a pixel is red, and putting a copy right when its two surfaces disagree.
//!
//! A windowed program's present is a copy from its back buffer to the kernel's staging surface,
//! which is always blue first. AMD's kernel driver converts on the way, so the daemon has to: a back
//! buffer that is red first, as a Direct3D 11 `R8G8B8A8` swap chain's is, would otherwise reach the
//! window with red and blue changed over. The engines here copy bytes as they are, so the pixels
//! are turned round in the target afterwards.
//!
//! A surface's format is the WDK's D3DDDIFORMAT number, which the guest's driver puts in its
//! descriptor and the kernel gives its own surfaces.

use crate::screen_copy::PIXEL_BYTES;
use guesthouse_backend::{StagingCopy, SurfaceShape};

const A8R8G8B8: u32 = 21;
const X8R8G8B8: u32 = 22;
const A8B8G8R8: u32 = 32;
const X8B8G8R8: u32 = 33;

const RED_IN_RGBA: usize = 0;
const BLUE_IN_RGBA: usize = 2;

/// The order of a pixel's bytes in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelOrder {
    BlueFirst,
    RedFirst,
}

/// A surface's order, or `None` for a format that isn't four 8-bit channels or isn't known here.
fn order_of(surface: &SurfaceShape) -> Option<ChannelOrder> {
    match surface.format {
        A8R8G8B8 | X8R8G8B8 => Some(ChannelOrder::BlueFirst),
        A8B8G8R8 | X8B8G8R8 => Some(ChannelOrder::RedFirst),
        _ => None,
    }
}

/// Whether a copy between these two leaves red where blue belongs. Surfaces of formats not known
/// here are copied as they are.
pub fn differs(source: &SurfaceShape, target: &SurfaceShape) -> bool {
    matches!((order_of(source), order_of(target)), (Some(from), Some(to)) if from != to)
}

/// The pixels a copy wrote, as rows of bytes in its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrittenRows {
    /// Of the first byte written, from the start of the target.
    pub start: u64,
    pub pitch: u64,
    pub row_bytes: u64,
    pub rows: u64,
}

impl WrittenRows {
    /// What a copy writes into a target of rows in order, or `None` for a tiled target, an empty
    /// rectangle or one that starts before the target does.
    pub fn of(copy: &StagingCopy) -> Option<Self> {
        if copy.target.tiling.is_some() {
            return None;
        }
        let width = u64::try_from(copy.right.checked_sub(copy.left)?).ok()?;
        let rows = u64::try_from(copy.bottom.checked_sub(copy.top)?).ok()?;
        let left = u64::try_from(copy.target_left).ok()?;
        let top = u64::try_from(copy.target_top).ok()?;
        let pitch = u64::from(copy.target.pitch);
        if width == 0 || rows == 0 {
            return None;
        }
        Some(Self { start: top * pitch + left * PIXEL_BYTES, pitch, row_bytes: width * PIXEL_BYTES, rows })
    }

    /// How far into the target the rows reach.
    pub fn end(&self) -> u64 {
        self.start + (self.rows - 1) * self.pitch + self.row_bytes
    }

    /// Changes red and blue over in every pixel of the rows.
    ///
    /// # Safety
    /// `target` must be readable and writable for [`Self::end`] bytes.
    pub unsafe fn swap_red_and_blue(&self, target: *mut u8) {
        for row in 0..self.rows {
            let at = (self.start + row * self.pitch) as usize;
            // SAFETY: the caller vouches for the bytes up to the end, which every row is inside.
            let pixels = unsafe { std::slice::from_raw_parts_mut(target.add(at), self.row_bytes as usize) };
            for pixel in pixels.as_chunks_mut::<{ PIXEL_BYTES as usize }>().0 {
                pixel.swap(RED_IN_RGBA, BLUE_IN_RGBA);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::SpaceId;

    fn shape(format: u32, pitch: u32, tiling: Option<u32>) -> SurfaceShape {
        SurfaceShape {
            width: 4,
            height: 3,
            format,
            pitch,
            tiling,
            refresh: (60, 1),
            samples: (1, 0),
            colour: format,
        }
    }

    fn copy(source: SurfaceShape, target: SurfaceShape, rect: [i32; 4], to: (i32, i32)) -> StagingCopy {
        StagingCopy {
            source,
            target,
            source_address: 0,
            target_address: 0,
            left: rect[0],
            top: rect[1],
            right: rect[2],
            bottom: rect[3],
            target_left: to.0,
            target_top: to.1,
            target_guest_address: None,
            is_screen: false,
            own_space: SpaceId(1),
        }
    }

    #[test]
    fn a_red_first_back_buffer_differs_from_the_staging_surface() {
        let staging = shape(X8R8G8B8, 16, None);
        assert!(differs(&shape(A8B8G8R8, 16, Some(27)), &staging));
        assert!(differs(&shape(X8B8G8R8, 16, None), &shape(A8R8G8B8, 16, None)));
        assert!(differs(&staging, &shape(A8B8G8R8, 16, None)));
    }

    #[test]
    fn surfaces_in_one_order_or_of_a_format_not_known_are_copied_as_they_are() {
        let staging = shape(X8R8G8B8, 16, None);
        assert!(!differs(&shape(A8R8G8B8, 16, Some(27)), &staging));
        assert!(!differs(&shape(A8B8G8R8, 16, None), &shape(X8B8G8R8, 16, None)));
        // D3DDDIFMT_A2B10G10R10 and D3DDDIFMT_A16B16G16R16F, which a swap of bytes wouldn't put right.
        assert!(!differs(&shape(31, 16, None), &staging));
        assert!(!differs(&shape(113, 16, None), &staging));
        assert!(!differs(&shape(0, 16, None), &staging));
    }

    #[test]
    fn only_the_rectangle_written_is_turned_round() {
        let request = copy(shape(A8B8G8R8, 16, Some(27)), shape(X8R8G8B8, 20, None), [0, 0, 2, 2], (1, 1));
        let rows = WrittenRows::of(&request).unwrap();
        assert_eq!(rows, WrittenRows { start: 24, pitch: 20, row_bytes: 8, rows: 2 });
        assert_eq!(rows.end(), 52);
        let before: Vec<u8> = (0..60).collect();
        let mut target = before.clone();
        // SAFETY: the rows end at byte 52 of 60.
        unsafe { rows.swap_red_and_blue(target.as_mut_ptr()) };
        assert_eq!(&target[24..32], &[26, 25, 24, 27, 30, 29, 28, 31]);
        assert_eq!(&target[44..52], &[46, 45, 44, 47, 50, 49, 48, 51]);
        assert_eq!(&target[..24], &before[..24]);
        assert_eq!(&target[32..44], &before[32..44]);
        assert_eq!(&target[52..], &before[52..]);
    }

    #[test]
    fn a_tiled_target_or_an_empty_rectangle_has_no_rows() {
        let back_buffer = shape(A8B8G8R8, 16, None);
        let staging = shape(X8R8G8B8, 16, None);
        let tiled = shape(X8R8G8B8, 16, Some(27));
        assert_eq!(WrittenRows::of(&copy(back_buffer, tiled, [0, 0, 2, 2], (0, 0))), None);
        assert_eq!(WrittenRows::of(&copy(back_buffer, staging, [2, 0, 2, 2], (0, 0))), None);
        assert_eq!(WrittenRows::of(&copy(back_buffer, staging, [0, 0, 2, 2], (-1, 0))), None);
    }
}
