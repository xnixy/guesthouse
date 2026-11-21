//! The daemon's own copy engine channel, for frames and for moving an allocation's contents.
//!
//! A copy is started and then looked at later, so that the message thread never waits for one.
//! The engine releases a semaphore in the host area when it finishes. Method numbers and values are
//! in the open kernel module's header for the copy class.

use super::{COPY_SUBCHANNEL, Channel, l2_invalidate_words};
use crate::pushbuffer::{increasing, ring_entry};
use crate::rm::Rm;
use crate::rm::classes::SURFACE_REGISTER_WAITER;
use crate::rm::device::Device;
use crate::tiling::{BLOCK_LIMIT, origin_fits};
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// Ring entries, at the start of the area.
pub const RING_ENTRIES: u32 = 1024;
const ENTRY_SIZE: usize = 8;
const PUSH: usize = 0x12000;
const SEMAPHORE: usize = 0x14000;
/// How long a copy gets before the channel counts as dead.
const COPY_TIMEOUT: Duration = Duration::from_secs(2);

/// The most one window can hold, for moving an allocation's contents.
pub const COPY_WINDOW: u64 = 64 << 20;
/// The two windows, which follow a copy channel's area in GPU addresses.
pub const COPY_AREA_EXTRA: u64 = 2 * COPY_WINDOW;

pub(crate) const METHOD_SET_SEMAPHORE: u32 = 0x240;
const METHOD_SET_RENDER_ENABLE: u32 = 0x25C;
pub(crate) const METHOD_LAUNCH: u32 = 0x300;
pub(crate) const METHOD_OFFSET_IN: u32 = 0x400;
const METHOD_SET_TARGET_BLOCK_SIZE: u32 = 0x70C;
const METHOD_SET_SOURCE_BLOCK_SIZE: u32 = 0x728;
const METHOD_SET_REMAP_COMPONENTS: u32 = 0x708;
const RENDER_ALWAYS: u32 = 1;
/// Four one-byte components in and out, with the first and third changed over: the target's first
/// is the source's third, its second the second, its third the first (a zero) and its fourth the fourth.
const REMAP_RED_AND_BLUE: u32 = 0x2 | 0x1 << 4 | 0x3 << 12 | 0x3 << 20 | 0x3 << 24;
const LAUNCH_REMAP: u32 = 1 << 10;
/// With the remap on, the engine counts a row and a place in a tiled surface in pixels.
const TURNED_PIXEL_BYTES: u32 = 4;
/// Launches: non-pipelined, flushed, ending in a one-word semaphore.
const LAUNCH_PITCH_LINES: u32 = 0x38E;
const LAUNCH_BLOCK_TO_PITCH_LINES: u32 = 0x30E;
const LAUNCH_PITCH_TO_BLOCK_LINES: u32 = 0x28E;
const LAUNCH_BLOCK_TO_BLOCK_LINES: u32 = 0x20E;
const LAUNCH_PITCH_ONE_LINE: u32 = 0x18E;
/// Flushed, with nothing to transfer, ending in a one-word semaphore.
pub(crate) const LAUNCH_SEMAPHORE_ONLY: u32 = 0xC;
/// The same, as a four-word semaphore with a timestamp and a non-blocking interrupt: the release
/// that wakes a surface's waiters. Only this kind raises the interrupt.
pub(crate) const LAUNCH_SIGNAL_INTR: u32 = 0x14 | 0x40;
/// How long to go on spinning before the wait sleeps on the interrupt, matching the daemon's own
/// fence window.
const SPIN_LIMIT: Duration = Duration::from_micros(200);
const BLOCK_HEIGHT_SHIFT: u32 = 4;
const ORIGIN_Y_SHIFT: u32 = 16;
/// Blocks are one GOB wide and deep, and GOBs have eight rows.
const BLOCK_GOB_HEIGHT_8: u32 = 1 << 12;

/// A rectangle to copy, in bytes and rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rows {
    pub source: u64,
    pub target: u64,
    pub source_pitch: u32,
    pub target_pitch: u32,
    pub width: u32,
    pub count: u32,
    /// True if red and blue change places on the way, for a source and a target that keep them in
    /// different orders. The pixels are then four bytes wide.
    pub turned: bool,
}

/// Where a rectangle sits in a tiled surface. `x` and `surface_width` are in bytes, and `block` is
/// the log2 of the surface's block height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiledSide {
    pub x: u32,
    pub y: u32,
    pub surface_width: u32,
    pub surface_height: u32,
    pub block: u32,
}

impl TiledSide {
    /// The engine's words for this side, or `None` for numbers its fields can't hold.
    fn words(&self, method: u32) -> Option<[u32; 7]> {
        if self.block >= BLOCK_LIMIT || !origin_fits(u64::from(self.x), u64::from(self.y)) {
            return None;
        }
        Some([
            increasing(COPY_SUBCHANNEL, method, 6),
            self.block << BLOCK_HEIGHT_SHIFT | BLOCK_GOB_HEIGHT_8,
            self.surface_width,
            self.surface_height,
            1,
            0,
            self.x | self.y << ORIGIN_Y_SHIFT,
        ])
    }
}

/// A rectangle with a tiled surface on at least one side. A tiled side's address in `rows` is the
/// whole surface's, and the engine finds the rectangle in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tiled {
    pub rows: Rows,
    pub source: Option<TiledSide>,
    pub target: Option<TiledSide>,
}

/// The words that start a copy, with the remap ahead of them if red and blue change places.
fn launch(rows: &Rows, kind: u32) -> Vec<u32> {
    let start = increasing(COPY_SUBCHANNEL, METHOD_LAUNCH, 1);
    if !rows.turned {
        return vec![start, kind];
    }
    vec![
        increasing(COPY_SUBCHANNEL, METHOD_SET_REMAP_COMPONENTS, 1),
        REMAP_RED_AND_BLUE,
        start,
        kind | LAUNCH_REMAP,
    ]
}

/// See the module documentation.
#[derive(Debug)]
pub struct CopyChannel {
    pub channel: Channel,
    put: u32,
    sequence: u32,
    started: Option<Instant>,
    /// The last batch, to run again on a new channel if this one dies.
    pub last_words: Vec<u32>,
    /// A semaphore surface's slot to signal at the end of a batch, with the surface and the
    /// descriptor its waiters post to. `None` until a surface is bound, and then each batch ends
    /// by writing the slot in the way that raises the interrupt, and arms a waiter for it.
    pub signal: Option<(u64, u32, RawFd)>,
}

impl CopyChannel {
    pub fn new(channel: Channel) -> Self {
        channel.area().store_u64(0, channel.prelude_entry());
        Self { channel, put: 1, sequence: 0, started: None, last_words: Vec::new(), signal: None }
    }

    /// The methods every copy shares: where from and to, and the semaphore that marks the end.
    fn head(&mut self, rows: &Rows) -> Vec<u32> {
        self.sequence = self.sequence.wrapping_add(1);
        let done = self.channel.area_address() + SEMAPHORE as u64;
        let header = |method, count| increasing(COPY_SUBCHANNEL, method, count);
        vec![
            header(METHOD_SET_RENDER_ENABLE, 1),
            RENDER_ALWAYS,
            header(METHOD_OFFSET_IN, 8),
            (rows.source >> 32) as u32,
            rows.source as u32,
            (rows.target >> 32) as u32,
            rows.target as u32,
            rows.source_pitch,
            rows.target_pitch,
            if rows.turned { rows.width / TURNED_PIXEL_BYTES } else { rows.width },
            rows.count,
            header(METHOD_SET_SEMAPHORE, 3),
            (done >> 32) as u32,
            done as u32,
            self.sequence,
        ]
    }

    /// The batch for a rectangle between linear surfaces.
    pub fn rows(&mut self, rows: &Rows) -> Vec<u32> {
        let mut words = self.head(rows);
        words.extend(launch(rows, LAUNCH_PITCH_LINES));
        words
    }

    /// The batch for a rectangle out of a tiled surface, which the engine detiles by itself. `None`
    /// if a number wouldn't fit its field, as it would change the one beside it.
    pub fn tiled(&mut self, tiled: &Tiled) -> Option<Vec<u32>> {
        let in_pixels = |side: &TiledSide| {
            if !tiled.rows.turned {
                return *side;
            }
            TiledSide {
                x: side.x / TURNED_PIXEL_BYTES,
                surface_width: side.surface_width / TURNED_PIXEL_BYTES,
                ..*side
            }
        };
        let source = tiled.source.as_ref().map(|side| in_pixels(side).words(METHOD_SET_SOURCE_BLOCK_SIZE));
        let target = tiled.target.as_ref().map(|side| in_pixels(side).words(METHOD_SET_TARGET_BLOCK_SIZE));
        let kind = match (&source, &target) {
            (Some(_), Some(_)) => LAUNCH_BLOCK_TO_BLOCK_LINES,
            (Some(_), None) => LAUNCH_BLOCK_TO_PITCH_LINES,
            (None, Some(_)) => LAUNCH_PITCH_TO_BLOCK_LINES,
            (None, None) => LAUNCH_PITCH_LINES,
        };
        let sides = [source, target].into_iter().flatten().collect::<Option<Vec<_>>>()?;
        let mut words = self.head(&tiled.rows);
        words.extend(sides.into_iter().flatten());
        words.extend(launch(&tiled.rows, kind));
        Some(words)
    }

    /// The batch for plain bytes between GPU addresses.
    pub fn bytes(&mut self, source: u64, target: u64, length: u32) -> Vec<u32> {
        let rows =
            Rows { source, target, source_pitch: 0, target_pitch: 0, width: length, count: 1, turned: false };
        let mut words = self.head(&rows);
        words.extend([increasing(COPY_SUBCHANNEL, METHOD_LAUNCH, 1), LAUNCH_PITCH_ONE_LINE]);
        words
    }

    /// The batch that drops host memory from the L2 and then says so, for when it can't go in order
    /// ahead of a submission.
    pub fn l2_invalidate(&mut self) -> Vec<u32> {
        self.sequence += 1;
        let done = self.channel.area_address() + SEMAPHORE as u64;
        let mut words = l2_invalidate_words();
        words.extend([
            increasing(COPY_SUBCHANNEL, METHOD_SET_SEMAPHORE, 3),
            (done >> 32) as u32,
            done as u32,
            self.sequence,
            increasing(COPY_SUBCHANNEL, METHOD_LAUNCH, 1),
            LAUNCH_SEMAPHORE_ONLY,
        ]);
        words
    }

    /// Sends a batch to the GPU.
    pub fn start<R: Rm>(&mut self, device: &mut Device<R>, mut words: Vec<u32>) {
        if let Some((slot, surface, fd)) = self.signal {
            // A waiter for the sequence this batch will write, armed before the batch runs so the
            // interrupt cannot beat the arm. A refused arm leaves the poll to time out, which is
            // what the backstop is for.
            let mut arm = [0; 32];
            arm[8..16].copy_from_slice(&u64::from(self.sequence + 1).to_le_bytes());
            arm[24..28].copy_from_slice(&(fd as u32).to_le_bytes());
            let _ = device.rm.control(surface, SURFACE_REGISTER_WAITER, &mut arm);
            words.extend([
                increasing(COPY_SUBCHANNEL, METHOD_SET_SEMAPHORE, 4),
                (slot >> 32) as u32,
                slot as u32,
                self.sequence + 1,
                0,
                increasing(COPY_SUBCHANNEL, METHOD_LAUNCH, 1),
                LAUNCH_SIGNAL_INTR,
            ]);
        }
        let area = self.channel.area();
        area.store_words(PUSH, &words);
        let entry = ring_entry(self.channel.area_address() + PUSH as u64, words.len() as u32);
        area.store_u64(self.put as usize * ENTRY_SIZE, entry);
        self.put = (self.put + 1) % RING_ENTRIES;
        self.channel.advance(device, self.put);
        self.started = Some(Instant::now());
        self.last_words = words;
    }

    /// Does what the engine's semaphore would, as a fake has no engine.
    #[cfg(any(test, feature = "fake-rm"))]
    pub fn pretend_finished(&self) {
        self.channel.area().store(SEMAPHORE, self.sequence);
    }

    pub fn is_finished(&self) -> bool {
        self.channel.area().load(SEMAPHORE) == self.sequence
    }

    /// What the channel looks like, for the log when a copy goes wrong.
    pub fn state(&self) -> String {
        let area = self.channel.area();
        format!(
            "wanted semaphore {}, it says {}, ring put {}, GPU at {}",
            self.sequence,
            area.load(SEMAPHORE),
            self.put,
            self.channel.gp_get()
        )
    }

    /// Whether the copy under way has outlived its time.
    pub fn has_timed_out(&self, now: Instant) -> bool {
        self.started.is_some_and(|started| now.saturating_duration_since(started) > COPY_TIMEOUT)
    }

    /// Gives a batch a place in this channel's sequence, for one carried over from a dead channel.
    pub fn renumber(&mut self, words: &mut [u32]) {
        const SEQUENCE_WORD: usize = 14;
        self.sequence = self.sequence.wrapping_add(1);
        words[SEQUENCE_WORD] = self.sequence;
        let done = self.channel.area_address() + SEMAPHORE as u64;
        words[SEQUENCE_WORD - 2] = (done >> 32) as u32;
        words[SEQUENCE_WORD - 1] = done as u32;
    }

    /// Waits until the copy ends: a spin for what is nearly done, then a sleep on the interrupt,
    /// with the poll's timeout as the backstop. Only for what can't go on without it.
    pub fn wait(&self) -> bool {
        let started = Instant::now();
        while !self.is_finished() {
            if self.has_timed_out(Instant::now()) {
                return false;
            }
            match self.signal {
                Some((_, _, fd)) if started.elapsed() >= SPIN_LIMIT => {
                    let mut fds = [libc::pollfd { fd, events: libc::POLLIN | libc::POLLPRI, revents: 0 }];
                    // SAFETY: the poll writes only into the one structure.
                    unsafe { libc::poll(fds.as_mut_ptr(), 1, 1) };
                }
                _ => std::hint::spin_loop(),
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{AREA_SIZE, ChannelPlace, Engine};
    use crate::profile::{BLACKWELL, HOST_615};
    use crate::rm::fake::FakeRm;

    const AREA_ADDRESS: u64 = 0x800_0000;

    fn copier() -> (Device<FakeRm>, CopyChannel) {
        let mut device = Device::open(FakeRm::new(), &HOST_615).unwrap();
        let space = device.create_address_space(&[(0x400_0000, 0x1_0000_0000)]).unwrap();
        let place = ChannelPlace {
            ring_address: AREA_ADDRESS,
            ring_entries: RING_ENTRIES,
            area_address: AREA_ADDRESS,
            area_size: AREA_SIZE,
        };
        let channel = Channel::new(&mut device, &BLACKWELL, &HOST_615, &space, place, Engine::Copy).unwrap();
        (device, CopyChannel::new(channel))
    }

    #[test]
    fn the_sequence_comes_round_after_its_last_number() {
        let (_device, mut copier) = copier();
        copier.sequence = u32::MAX;
        assert_eq!(copier.rows(&rows())[14], 0);
    }

    fn rows() -> Rows {
        Rows {
            source: 0x1_0000_0000,
            target: 0x2000,
            source_pitch: 256,
            target_pitch: 512,
            width: 64,
            count: 8,
            turned: false,
        }
    }

    #[test]
    fn a_linear_copy_ends_with_a_launch() {
        let (_, mut copier) = copier();
        let words = copier.rows(&rows());
        assert_eq!(words.len(), 17);
        assert_eq!(words[3..5], [1, 0]);
        assert_eq!(words[16], LAUNCH_PITCH_LINES);
    }

    #[test]
    fn a_tiled_copy_describes_the_source_block() {
        let (_, mut copier) = copier();
        let tiled = Tiled { rows: rows(), source: Some(side()), target: None };
        let words = copier.tiled(&tiled).unwrap();
        assert_eq!(words[16], 3 << 4 | 1 << 12);
        assert_eq!(words[21], 4 | 2 << 16);
        assert_eq!(*words.last().unwrap(), LAUNCH_BLOCK_TO_PITCH_LINES);
    }

    fn side() -> TiledSide {
        TiledSide { x: 4, y: 2, surface_width: 1024, surface_height: 768, block: 3 }
    }

    #[test]
    fn a_copy_into_a_tiled_surface_describes_the_target_block() {
        let (_, mut copier) = copier();
        let words = copier.tiled(&Tiled { rows: rows(), source: None, target: Some(side()) }).unwrap();
        assert_eq!(words[15], increasing(COPY_SUBCHANNEL, METHOD_SET_TARGET_BLOCK_SIZE, 6));
        assert_eq!(words[16..22], [3 << 4 | 1 << 12, 1024, 768, 1, 0, 4 | 2 << 16]);
        assert_eq!(*words.last().unwrap(), LAUNCH_PITCH_TO_BLOCK_LINES);
    }

    #[test]
    fn a_copy_between_tiled_surfaces_describes_both() {
        let (_, mut copier) = copier();
        let target = TiledSide { x: 8, y: 16, ..side() };
        let words =
            copier.tiled(&Tiled { rows: rows(), source: Some(side()), target: Some(target) }).unwrap();
        assert_eq!(words[15], increasing(COPY_SUBCHANNEL, METHOD_SET_SOURCE_BLOCK_SIZE, 6));
        assert_eq!(words[22], increasing(COPY_SUBCHANNEL, METHOD_SET_TARGET_BLOCK_SIZE, 6));
        assert_eq!(words[28], 8 | 16 << 16);
        assert_eq!(*words.last().unwrap(), LAUNCH_BLOCK_TO_BLOCK_LINES);
    }

    #[test]
    fn a_number_past_its_field_makes_no_batch_on_either_side() {
        let (_, mut copier) = copier();
        let good = side();
        for bad in [
            TiledSide { block: 6, ..good },
            TiledSide { block: u32::MAX, ..good },
            TiledSide { x: 0x1_0000, ..good },
            TiledSide { y: 0x1_0000, ..good },
        ] {
            let source = Tiled { rows: rows(), source: Some(bad), target: Some(good) };
            let target = Tiled { rows: rows(), source: Some(good), target: Some(bad) };
            let alone = Tiled { rows: rows(), source: None, target: Some(bad) };
            assert_eq!(copier.tiled(&source), None, "{bad:?}");
            assert_eq!(copier.tiled(&target), None, "{bad:?}");
            assert_eq!(copier.tiled(&alone), None, "{bad:?}");
        }
        assert_eq!(copier.sequence, 0, "nothing was started");
    }

    #[test]
    fn a_copy_is_finished_when_its_semaphore_says_so() {
        let (mut device, mut copier) = copier();
        let words = copier.bytes(0x1000, 0x2000, 64);
        copier.start(&mut device, words);
        assert!(!copier.is_finished());
        copier.channel.area().store(SEMAPHORE, 1);
        assert!(copier.is_finished());
        assert_eq!(device.rm.doorbells().len(), 1);
    }

    #[test]
    fn a_stuck_copy_times_out() {
        let (mut device, mut copier) = copier();
        let words = copier.bytes(0x1000, 0x2000, 64);
        copier.start(&mut device, words);
        assert!(!copier.has_timed_out(Instant::now()));
        assert!(copier.has_timed_out(Instant::now() + COPY_TIMEOUT + Duration::from_secs(1)));
    }
}
