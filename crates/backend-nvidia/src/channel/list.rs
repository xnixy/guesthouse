//! A channel whose ring lives here, for queues that hand over a list of entries with each submission.
//!
//! Each list is followed by a release of the queue's fence. That fence is also how the ring knows
//! what is free, as the control block's read position stays at zero on these channels. So nothing
//! frees the ring during a list, and a whole list has to fit.

use super::copy::{LAUNCH_SEMAPHORE_ONLY, METHOD_LAUNCH, METHOD_SET_SEMAPHORE};
use super::{AREA_SIZE, COPY_SUBCHANNEL, Channel, Engine, HOST_SUBCHANNEL, VIDEO_SUBCHANNEL};
#[cfg(any(test, feature = "fake-rm"))]
use crate::pushbuffer::RingEntry;
use crate::pushbuffer::{increasing, ring_entry};
use std::collections::VecDeque;

const RING_ENTRIES: u32 = 0x4000;
const ENTRY_SIZE: usize = 8;
const RING: usize = AREA_SIZE;
const RELEASE_SLOT: usize = 32;
const RELEASES: usize = RING + RING_ENTRIES as usize * ENTRY_SIZE;

/// The size of the host area of a list channel. A slot per ring entry means none is reused
/// while the GPU can still read it.
pub const LIST_AREA: usize = RELEASES + RING_ENTRIES as usize * RELEASE_SLOT;

const SEMAPHORE_ADDRESS_LOW: u32 = 0x5C;
const SEMAPHORE_WORDS: u32 = 5;
/// Release, after earlier work has finished, with a 64-bit value.
const SEMAPHORE_RELEASE_64: u32 = 0x0110_0001;

/// A video engine's own semaphore, from the public headers for the video classes, so the fence moves
/// in the engine's own order. It writes one word, so a value past that gets a host release.
const VIDEO_SEMAPHORE_ADDRESS: u32 = 0x240;
const VIDEO_SEMAPHORE_ADDRESS_WORDS: u32 = 3;
const VIDEO_SEMAPHORE_RELEASE: u32 = 0x304;
const VIDEO_RELEASE_ONE_WORD: u32 = 0;

/// What releases a fence at the end of a list.
fn release_words(engine: Engine, fence_gpu_address: u64, value: u64) -> [u32; 6] {
    match engine {
        Engine::Video(_) if value <= u64::from(u32::MAX) => [
            increasing(VIDEO_SUBCHANNEL, VIDEO_SEMAPHORE_ADDRESS, VIDEO_SEMAPHORE_ADDRESS_WORDS),
            (fence_gpu_address >> 32) as u32,
            fence_gpu_address as u32,
            value as u32,
            increasing(VIDEO_SUBCHANNEL, VIDEO_SEMAPHORE_RELEASE, 1),
            VIDEO_RELEASE_ONE_WORD,
        ],
        Engine::Copy if value <= u64::from(u32::MAX) => [
            increasing(COPY_SUBCHANNEL, METHOD_SET_SEMAPHORE, 3),
            (fence_gpu_address >> 32) as u32,
            fence_gpu_address as u32,
            value as u32,
            increasing(COPY_SUBCHANNEL, METHOD_LAUNCH, 1),
            LAUNCH_SEMAPHORE_ONLY,
        ],
        _ => [
            increasing(HOST_SUBCHANNEL, SEMAPHORE_ADDRESS_LOW, SEMAPHORE_WORDS),
            fence_gpu_address as u32,
            (fence_gpu_address >> 32) as u32,
            value as u32,
            (value >> 32) as u32,
            SEMAPHORE_RELEASE_64,
        ],
    }
}

/// The ring has no room for a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListFull {
    /// Not now. There will be room when the GPU catches up.
    ForNow,
    /// This list could never fit.
    TooLong,
}

/// See the module documentation.
#[derive(Debug)]
pub struct ListChannel {
    pub channel: Channel,
    put: u32,
    releases: usize,
    free_from: u32,
    /// (fence value, the last ring entry it covers), oldest first.
    unfinished: VecDeque<(u64, u32)>,
}

impl ListChannel {
    pub const RING_ENTRIES: u32 = RING_ENTRIES;

    /// The GPU address of the ring, given where the host area goes.
    pub fn ring_address(area_address: u64) -> u64 {
        area_address + RING as u64
    }

    pub fn new(channel: Channel) -> Self {
        channel.area().store_u64(RING, channel.prelude_entry());
        Self { channel, put: 1, releases: 0, free_from: 0, unfinished: VecDeque::new() }
    }

    /// What has been put on the ring since the start, for tests that play the GPU.
    #[cfg(any(test, feature = "fake-rm"))]
    pub fn entries(&self) -> Vec<RingEntry> {
        let entry_at = |index: u32| {
            let words = self.channel.area().load_words(RING + index as usize * ENTRY_SIZE, 2);
            RingEntry::parse((u64::from(words[0]) | u64::from(words[1]) << 32).to_le_bytes())
        };
        (0..self.put).map(entry_at).collect()
    }

    fn free_entries(&self) -> u32 {
        RING_ENTRIES - 1 - (self.put + RING_ENTRIES - self.free_from) % RING_ENTRIES
    }

    fn reclaim(&mut self, finished: u64) {
        while let Some(&(value, last)) = self.unfinished.front()
            && value <= finished
        {
            self.free_from = last;
            self.unfinished.pop_front();
        }
    }

    fn append(&mut self, entry: u64) {
        self.channel.area().store_u64(RING + self.put as usize * ENTRY_SIZE, entry);
        self.put = (self.put + 1) % RING_ENTRIES;
    }

    /// Copies entries to the ring, with the fence's release, and returns the new position.
    /// `finished` is the fence's value now.
    pub fn add(
        &mut self,
        entries: &[u8],
        fence_gpu_address: u64,
        value: u64,
        finished: u64,
    ) -> Result<u32, ListFull> {
        self.reclaim(finished);
        // The list, the entry ahead of it and the release after it.
        let needed = entries.len().div_ceil(ENTRY_SIZE) as u32 + 2;
        if needed > RING_ENTRIES - 1 {
            return Err(ListFull::TooLong);
        }
        if needed > self.free_entries() {
            return Err(ListFull::ForNow);
        }
        self.append(self.channel.entry_ahead());
        self.channel.entry_ahead_placed();
        for entry in entries.chunks(ENTRY_SIZE) {
            let mut bytes = [0; ENTRY_SIZE];
            bytes[..entry.len()].copy_from_slice(entry);
            self.append(u64::from_le_bytes(bytes));
        }
        let slot = RELEASES + self.releases % RING_ENTRIES as usize * RELEASE_SLOT;
        self.releases += 1;
        let words = release_words(self.channel.engine(), fence_gpu_address, value);
        self.channel.area().store_words(slot, &words);
        self.append(ring_entry(self.channel.area_address() + slot as u64, words.len() as u32));
        // The queue's own commands can write this value before the GPU has read the release's entry.
        self.unfinished.push_back((value, (self.put + RING_ENTRIES - 1) % RING_ENTRIES));
        Ok(self.put)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelPlace, Engine};
    use crate::profile::{BLACKWELL, HOST_615};
    use crate::rm::device::Device;
    use crate::rm::fake::FakeRm;

    const AREA_ADDRESS: u64 = 0x800_0000;
    const FENCE: u64 = 0x1_2345_6000;

    fn list() -> ListChannel {
        let mut device = Device::open(FakeRm::new(), &HOST_615).unwrap();
        let space = device.create_address_space(&[(0x400_0000, 0x1_0000_0000)]).unwrap();
        let place = ChannelPlace {
            ring_address: ListChannel::ring_address(AREA_ADDRESS),
            ring_entries: RING_ENTRIES,
            area_address: AREA_ADDRESS,
            area_size: LIST_AREA,
        };
        let channel =
            Channel::new(&mut device, &BLACKWELL, &HOST_615, &space, place, Engine::Graphics).unwrap();
        ListChannel::new(channel)
    }

    fn parsed(entry: u64) -> RingEntry {
        RingEntry::parse(entry.to_le_bytes())
    }

    fn entry_at(list: &ListChannel, index: usize) -> RingEntry {
        let words = list.channel.area().load_words(RING + index * ENTRY_SIZE, 2);
        let bytes = u64::from(words[0]) | u64::from(words[1]) << 32;
        RingEntry::parse(bytes.to_le_bytes())
    }

    #[test]
    fn the_prelude_comes_first() {
        let list = list();
        let invalidate = parsed(list.channel.l2_invalidate_entry());
        assert_eq!(
            entry_at(&list, 0).length,
            2 * BLACKWELL.graphics_classes.len() as u32 + invalidate.length
        );
    }

    #[test]
    fn a_list_is_followed_by_its_fence_release() {
        let mut list = list();
        let entry = ring_entry(0x1000, 4).to_le_bytes();
        assert_eq!(list.add(&entry, FENCE, 7, 0), Ok(4));
        assert_eq!(entry_at(&list, 2), RingEntry { address: 0x1000, length: 4 });
        let release = entry_at(&list, 3);
        assert_eq!(release.length, 6);
        let words = list.channel.area().load_words((release.address - AREA_ADDRESS) as usize, 6);
        assert_eq!(&words[1..5], [0x2345_6000, 1, 7, 0]);
        assert_eq!(words[5], SEMAPHORE_RELEASE_64);
    }

    #[test]
    fn a_video_engine_releases_the_fence_itself_after_its_work() {
        let video =
            Engine::Video(crate::channel::VideoEngine { rm_engine: 0x13, class: 0xCFB0, instance: 0 });
        let words = release_words(video, FENCE, 7);
        assert_eq!(words[0], increasing(4, 0x240, 3));
        assert_eq!(words[1..4], [1, 0x2345_6000, 7]);
        assert_eq!(words[4..6], [increasing(4, 0x304, 1), 0]);
    }

    #[test]
    fn a_list_starts_by_dropping_host_memory_from_the_l2() {
        const L2_SYSMEM_INVALIDATE: u32 = 0x7000_0000;
        let mut list = list();
        list.add(&ring_entry(0x1000, 4).to_le_bytes(), FENCE, 7, 0).unwrap();
        let first = entry_at(&list, 1);
        assert_eq!(first, parsed(list.channel.l2_invalidate_entry()));
        let words =
            list.channel.area().load_words((first.address - AREA_ADDRESS) as usize, first.length as usize);
        assert_eq!(words.iter().filter(|&&word| word == L2_SYSMEM_INVALIDATE).count(), 1);
    }

    #[test]
    fn a_ring_that_has_filled_says_so_until_the_fence_moves() {
        let mut list = list();
        let long = vec![0; (RING_ENTRIES as usize - 4) * ENTRY_SIZE];
        assert!(list.add(&long, FENCE, 1, 0).is_ok());
        assert_eq!(list.add(&[0; 8], FENCE, 2, 0), Err(ListFull::ForNow));
        assert!(list.add(&[0; 8], FENCE, 2, 1).is_ok());
    }

    #[test]
    fn a_list_longer_than_the_ring_can_never_fit() {
        let mut list = list();
        let long = vec![0; RING_ENTRIES as usize * ENTRY_SIZE];
        assert_eq!(list.add(&long, FENCE, 1, 0), Err(ListFull::TooLong));
    }
}
