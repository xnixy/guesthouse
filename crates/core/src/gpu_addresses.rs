//! Handing out GPU addresses, and taking them back.

use guesthouse_backend::{BackendError, GpuRange};
use guesthouse_messages::wire::align_up;
use std::collections::HashMap;

/// The ranges every address space covers. There is a hole around 4 GiB, where real drivers keep
/// something of their own, and the ranges are big because one process can reserve gigabytes. They
/// stop at 1 TiB because a ring entry can't reach past it.
pub const GPU_RANGES: [GpuRange; 2] = [
    GpuRange { start: 0x400_0000, end: 0x1_0000_0000 },
    GpuRange { start: 0x1_4000_0000, end: 0x100_0000_0000 },
];

/// Past the last address any mapping can have, which also makes it more than any size can be.
pub const ADDRESSES_END: u64 = GPU_RANGES[1].end;

/// The alignment of ordinary mappings.
pub const ALLOCATION_ALIGN: u64 = 64 << 10;

/// The alignment of mappings big enough for 2 MiB pages, which must start on a 2 MiB boundary.
pub const LARGE_ALIGN: u64 = 2 << 20;

/// What a range of addresses is for. A range only ever goes back to the same use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressUse {
    /// The host's own, for channels and copies. Once in use, these are never freed.
    Host,
    /// A slot of the frame ring, which comes and goes with viewers and screen modes.
    FrameRing,
    /// A whole allocation in video memory, mapped where the host chose.
    Mapped,
    /// The same in system memory: the guest's own pages, or video memory a first lock moved there.
    MappedSystem,
    /// Set aside by the guest, for mappings it places itself.
    Reserved,
}

impl AddressUse {
    /// The use for an allocation mapped where the host chose, by the memory behind it.
    pub fn mapped(system_memory: bool) -> Self {
        if system_memory { Self::MappedSystem } else { Self::Mapped }
    }

    /// Where a range can come from once no new addresses are left: a range that held the other kind
    /// of memory, which is better than refusing the mapping.
    fn last_resort(self) -> Option<Self> {
        match self {
            Self::Mapped => Some(Self::MappedSystem),
            Self::MappedSystem => Some(Self::Mapped),
            Self::Host | Self::FrameRing | Self::Reserved => None,
        }
    }
}

/// A bump allocator with a free list per shape of range. Addresses stay unique across every address
/// space at any one time, so an address always names one allocation.
///
/// A program that makes and frees memory all day would run out of new addresses in the end. Freed
/// ranges are not split or joined, and go back only to the same use, size and alignment, because
/// handing one to a different use has made the vendor's driver refuse mappings.
///
/// A mapped range also goes back only to the kind of memory it held last. Video memory mapped where
/// system memory was has made the vendor's compute launches fault with an invalid address space,
/// however long the range rested and whatever caches were dropped first. Ranges that held video
/// memory go back to video memory without trouble.
#[derive(Debug)]
pub struct GpuAddresses {
    next: u64,
    free: HashMap<(AddressUse, u64, u64), Vec<u64>>,
    /// Where new addresses come from: the ranges, less what a backend keeps for the guest.
    hand_out: Vec<GpuRange>,
    /// (start, end) of each range a guest placed, which hand-outs stay clear of in every space. The
    /// same range can be here twice, as two processes can each hold it.
    placed: Vec<(u64, u64)>,
    /// (start, end) of what the host took for itself.
    host: Vec<(u64, u64)>,
}

/// Where a request will accept addresses: from `from`, with everything ending by `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: u64,
    pub to: u64,
}

impl Window {
    pub const ANYWHERE: Self = Self { from: 0, to: u64::MAX };

    fn holds(&self, base: u64, size: u64) -> bool {
        base >= self.from && size <= self.to.saturating_sub(base)
    }

    fn limit(&self, range: &GpuRange) -> Option<GpuRange> {
        let limited = GpuRange { start: range.start.max(self.from), end: range.end.min(self.to) };
        (limited.start < limited.end).then_some(limited)
    }
}

fn overlaps((start, end): (u64, u64), base: u64, size: u64) -> bool {
    start < base.saturating_add(size) && base < end
}

/// The first place in `range` from `from` on, stepping over what's blocked.
fn first_fit(range: &GpuRange, from: u64, size: u64, align: u64, blocked: &[(u64, u64)]) -> Option<u64> {
    let mut base = align_up(from.max(range.start), align);
    while size <= range.end.saturating_sub(base) {
        match blocked.iter().find(|&&block| overlaps(block, base, size)) {
            Some(&(_, end)) => base = align_up(end, align),
            None => return Some(base),
        }
    }
    None
}

/// `ranges` without `kept`, in order.
fn without(ranges: &[GpuRange], kept: &GpuRange) -> Vec<GpuRange> {
    let mut left = Vec::with_capacity(ranges.len() + 1);
    for range in ranges {
        let before = GpuRange { start: range.start, end: range.end.min(kept.start) };
        let after = GpuRange { start: range.start.max(kept.end), end: range.end };
        left.extend([before, after].into_iter().filter(|part| part.start < part.end));
    }
    left
}

impl GpuAddresses {
    pub fn new() -> Self {
        Self::keeping_clear(&[])
    }

    /// Never hands out anything in `kept`. Those addresses still work, as the guest may map there.
    pub fn keeping_clear(kept: &[GpuRange]) -> Self {
        let hand_out = kept.iter().fold(GPU_RANGES.to_vec(), |left, range| without(&left, range));
        Self {
            next: GPU_RANGES[0].start,
            free: HashMap::new(),
            hand_out,
            placed: Vec::new(),
            host: Vec::new(),
        }
    }

    /// Takes `size` bytes: what was given back for this use, or new addresses, moving to the next
    /// range if this one is too full.
    pub fn take(&mut self, size: u64, align: u64, usage: AddressUse) -> Result<u64, BackendError> {
        self.take_within(size, align, usage, Window::ANYWHERE)
            .ok_or_else(|| BackendError::new("no room left in the GPU address ranges"))
    }

    /// As [`GpuAddresses::take`], but only if what comes next lies in `window`. Nothing changes if not.
    pub fn take_within(&mut self, size: u64, align: u64, usage: AddressUse, window: Window) -> Option<u64> {
        if let Some(spare) = self.spare(size, align, usage, window) {
            return Some(spare);
        }
        let Some(base) = self.next_clear(size, align).filter(|&base| window.holds(base, size)) else {
            let other = usage.last_resort()?;
            return self.spare(size, align, other, window);
        };
        self.next = base + size;
        if usage == AddressUse::Host {
            self.host.push((base, base + size));
        }
        Some(base)
    }

    /// The newest range given back for this use that hasn't been placed over since.
    fn spare(&mut self, size: u64, align: u64, usage: AddressUse, window: Window) -> Option<u64> {
        let spares = self.free.get_mut(&(usage, size, align))?;
        let usable = |base: &u64| {
            window.holds(*base, size) && !self.placed.iter().any(|&range| overlaps(range, *base, size))
        };
        let found = spares.iter().rposition(usable)?;
        Some(spares.remove(found))
    }

    /// The first new address with room, past whatever the guest has placed.
    fn next_clear(&self, size: u64, align: u64) -> Option<u64> {
        self.hand_out.iter().find_map(|range| first_fit(range, self.next, size, align, &self.placed))
    }

    /// Room in `window`, whatever has been handed out, clear of `taken` and of the host's own ranges.
    /// The caller goes on to [`GpuAddresses::place`] it.
    pub fn find_gap(&self, window: Window, size: u64, align: u64, taken: &[(u64, u64)]) -> Option<u64> {
        let blocked = [taken, &self.host].concat();
        let in_window = self.hand_out.iter().filter_map(|range| window.limit(range));
        in_window.into_iter().find_map(|range| first_fit(&range, range.start, size, align, &blocked))
    }

    /// Whether any addresses this hands out lie in `window`.
    pub fn reaches(&self, window: Window) -> bool {
        self.hand_out.iter().any(|range| window.limit(range).is_some())
    }

    /// Notes a range the guest holds in one of its spaces, which nothing may be handed from.
    pub fn place(&mut self, base: u64, size: u64) {
        self.placed.push((base, base + size));
    }

    /// The opposite of [`GpuAddresses::place`]. Other spaces can hold the same range.
    pub fn unplace(&mut self, base: u64, size: u64) {
        if let Some(index) = self.placed.iter().position(|&range| range == (base, base + size)) {
            self.placed.swap_remove(index);
        }
    }

    /// The host's own ranges. Each is in use in some space, or in all of them.
    pub fn host_ranges(&self) -> &[(u64, u64)] {
        &self.host
    }

    /// Makes a range free for the same use again. Nothing may be mapped there now.
    pub fn give_back(&mut self, base: u64, size: u64, align: u64, usage: AddressUse) {
        self.free.entry((usage, size, align)).or_default().push(base);
    }

    pub fn in_a_range(address: u64) -> bool {
        GPU_RANGES.iter().any(|range| range.contains(address))
    }
}

impl Default for GpuAddresses {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: AddressUse = AddressUse::Host;

    #[test]
    fn addresses_follow_one_another() {
        let mut addresses = GpuAddresses::new();
        assert_eq!(addresses.take(0x1000, ALLOCATION_ALIGN, HOST), Ok(0x400_0000));
        assert_eq!(addresses.take(0x1000, ALLOCATION_ALIGN, HOST), Ok(0x401_0000));
    }

    #[test]
    fn alignment_can_leave_a_gap() {
        let mut addresses = GpuAddresses::new();
        addresses.take(0x1000, ALLOCATION_ALIGN, HOST).unwrap();
        assert_eq!(addresses.take(0x1000, LARGE_ALIGN, HOST), Ok(0x420_0000));
    }

    #[test]
    fn what_does_not_fit_moves_to_the_next_range() {
        let mut addresses = GpuAddresses::new();
        assert_eq!(addresses.take(0x1_0000_0000, ALLOCATION_ALIGN, HOST), Ok(GPU_RANGES[1].start));
    }

    #[test]
    fn the_hole_is_not_in_a_range() {
        assert!(!GpuAddresses::in_a_range(0x1_0000_0000));
        assert!(GpuAddresses::in_a_range(0x1_4000_0000));
    }

    #[test]
    fn a_kept_range_is_jumped_over_and_still_mappable() {
        let kept = GpuRange { start: 0x800_0000, end: GPU_RANGES[0].end };
        let mut addresses = GpuAddresses::keeping_clear(&[kept]);
        assert_eq!(addresses.take(0x400_0000, ALLOCATION_ALIGN, HOST), Ok(0x400_0000));
        assert_eq!(addresses.take(0x1000, ALLOCATION_ALIGN, HOST), Ok(GPU_RANGES[1].start));
        assert!(GpuAddresses::in_a_range(kept.start));
    }

    #[test]
    fn a_kept_range_in_the_middle_leaves_both_sides() {
        let kept = GpuRange { start: 0x500_0000, end: 0x600_0000 };
        let mut addresses = GpuAddresses::keeping_clear(&[kept]);
        assert_eq!(addresses.take(0x100_0000, ALLOCATION_ALIGN, HOST), Ok(0x400_0000));
        assert_eq!(addresses.take(0x1000, ALLOCATION_ALIGN, HOST), Ok(0x600_0000));
    }

    const PLACED_AT: u64 = 0x401_0000;

    #[test]
    fn new_addresses_step_over_what_a_guest_placed() {
        let mut addresses = GpuAddresses::new();
        addresses.place(PLACED_AT, 0x2_0000);
        assert_eq!(addresses.take(0x1_0000, ALLOCATION_ALIGN, HOST), Ok(0x400_0000));
        assert_eq!(addresses.take(0x1_0000, ALLOCATION_ALIGN, HOST), Ok(PLACED_AT + 0x2_0000));
    }

    #[test]
    fn a_spare_range_that_was_placed_over_stays_on_the_shelf() {
        let usage = AddressUse::Mapped;
        let mut addresses = GpuAddresses::new();
        addresses.give_back(PLACED_AT, 0x1_0000, ALLOCATION_ALIGN, usage);
        addresses.place(PLACED_AT, 0x1_0000);
        assert_ne!(addresses.take(0x1_0000, ALLOCATION_ALIGN, usage), Ok(PLACED_AT));
        addresses.unplace(PLACED_AT, 0x1_0000);
        assert_eq!(addresses.take(0x1_0000, ALLOCATION_ALIGN, usage), Ok(PLACED_AT));
    }

    #[test]
    fn a_range_two_spaces_placed_stays_clear_until_both_let_go() {
        let mut addresses = GpuAddresses::new();
        addresses.place(0x400_0000, 0x1_0000);
        addresses.place(0x400_0000, 0x1_0000);
        addresses.unplace(0x400_0000, 0x1_0000);
        assert_eq!(addresses.take(0x1000, ALLOCATION_ALIGN, HOST), Ok(PLACED_AT));
    }

    #[test]
    fn taking_within_a_window_changes_nothing_if_the_next_address_is_outside() {
        let mut addresses = GpuAddresses::new();
        let window = Window { from: 0x500_0000, to: 0x600_0000 };
        assert_eq!(addresses.take_within(0x1000, ALLOCATION_ALIGN, HOST, window), None);
        assert_eq!(addresses.take(0x1000, ALLOCATION_ALIGN, HOST), Ok(0x400_0000));
    }

    #[test]
    fn a_gap_is_found_behind_the_next_address_and_clear_of_the_host() {
        let mut addresses = GpuAddresses::new();
        addresses.take(0x1_0000, ALLOCATION_ALIGN, HOST).unwrap();
        addresses.take(0x1000_0000, ALLOCATION_ALIGN, AddressUse::Mapped).unwrap();
        let window = Window { from: 0, to: 0x800_0000 };
        let gap = addresses.find_gap(window, 0x1_0000, ALLOCATION_ALIGN, &[(PLACED_AT, 0x402_0000)]);
        assert_eq!(gap, Some(0x402_0000));
    }

    #[test]
    fn a_window_past_every_range_is_out_of_reach() {
        let addresses = GpuAddresses::new();
        let far = Window { from: 1 << 40, to: 2 << 40 };
        assert!(!addresses.reaches(far));
        assert_eq!(addresses.find_gap(far, 0x1000, ALLOCATION_ALIGN, &[]), None);
        assert!(addresses.reaches(Window::ANYWHERE));
    }

    #[test]
    fn running_out_is_an_error() {
        assert!(GpuAddresses::new().take(1 << 50, ALLOCATION_ALIGN, HOST).is_err());
        assert!(GpuAddresses::new().take(u64::MAX, ALLOCATION_ALIGN, HOST).is_err());
    }

    #[test]
    fn a_freed_range_comes_back_for_the_same_use_only() {
        let mut addresses = GpuAddresses::new();
        let usage = AddressUse::Mapped;
        let first = addresses.take(0x10000, ALLOCATION_ALIGN, usage).unwrap();
        addresses.give_back(first, 0x10000, ALLOCATION_ALIGN, usage);
        assert_ne!(addresses.take(0x10000, ALLOCATION_ALIGN, AddressUse::Reserved), Ok(first));
        assert_ne!(addresses.take(0x20000, ALLOCATION_ALIGN, usage), Ok(first));
        assert_eq!(addresses.take(0x10000, ALLOCATION_ALIGN, usage), Ok(first));
    }

    #[test]
    fn a_range_that_held_system_memory_goes_back_to_system_memory() {
        let mut addresses = GpuAddresses::new();
        let (video, system) = (AddressUse::mapped(false), AddressUse::mapped(true));
        let first = addresses.take(0x10000, ALLOCATION_ALIGN, video).unwrap();
        addresses.give_back(first, 0x10000, ALLOCATION_ALIGN, system);
        assert_ne!(addresses.take(0x10000, ALLOCATION_ALIGN, video), Ok(first));
        assert_eq!(addresses.take(0x10000, ALLOCATION_ALIGN, system), Ok(first));
    }

    #[test]
    fn a_range_that_held_video_memory_goes_back_to_video_memory() {
        let mut addresses = GpuAddresses::new();
        let (video, system) = (AddressUse::mapped(false), AddressUse::mapped(true));
        let first = addresses.take(0x10000, ALLOCATION_ALIGN, system).unwrap();
        addresses.give_back(first, 0x10000, ALLOCATION_ALIGN, video);
        assert_ne!(addresses.take(0x10000, ALLOCATION_ALIGN, system), Ok(first));
        assert_eq!(addresses.take(0x10000, ALLOCATION_ALIGN, video), Ok(first));
    }

    #[test]
    fn with_no_new_addresses_left_a_range_of_the_other_memory_is_used() {
        let last = GPU_RANGES[1].end - 0x10000;
        let mut addresses = GpuAddresses::keeping_clear(&[GpuRange { start: 0, end: last }]);
        let (video, system) = (AddressUse::mapped(false), AddressUse::mapped(true));
        let only = addresses.take(0x10000, ALLOCATION_ALIGN, system).unwrap();
        addresses.give_back(only, 0x10000, ALLOCATION_ALIGN, system);
        assert_eq!(addresses.take(0x10000, ALLOCATION_ALIGN, video), Ok(only));
        assert!(addresses.take(0x10000, ALLOCATION_ALIGN, video).is_err());
    }

    #[test]
    fn the_last_range_given_back_goes_out_first() {
        let mut addresses = GpuAddresses::new();
        let usage = AddressUse::Reserved;
        addresses.give_back(0x1000, 1, 1, usage);
        addresses.give_back(0x2000, 1, 1, usage);
        assert_eq!(addresses.take(1, 1, usage), Ok(0x2000));
    }
}
