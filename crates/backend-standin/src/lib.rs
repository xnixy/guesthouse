//! A pretend GPU, for tests only.
//!
//! It keeps the books a real driver would and refuses what a real driver refuses, so that a mistake
//! in the core shows up as a failed call. Its private data format exists only here:
//!
//! | Bytes | Meaning |
//! |-------|---------|
//! | 0..8  | allocation size, or 4096 if the data is shorter |
//! | 8..12 | surface width, or no surface if zero |
//! | 12..16 | surface height |
//! | 16    | nonzero asks for the GPU's cache |

mod memory;
mod random_messages;

use guesthouse_backend::{
    AdapterFacts, AllocationShape, Backend, BackendError, CardLabel, DriverFound, DriverSurvey, GpuRange,
    GuestBooks, HostServices, MapRequest, MemoryId, MemoryKind, PciCard, Refusal, RegistryName,
    RegistryValue, Setup, SlotCopy, SpaceId, StagingCopy, StagingStart, Status, SurfaceShape, UnmapRequest,
    VendorQuery, Work, WorkStart,
};
pub use memory::SparseMemory;
pub use random_messages::RandomMessages;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::time::Instant;

const STAND_IN_VENDOR: u16 = 0xFFFE;
const DEFAULT_SIZE: u64 = 4096;
/// A query only the stand-in's "vendor" answers, to test that the vendor is asked first.
pub const VENDOR_ONLY_QUERY: u32 = 0;
pub const VENDOR_ANSWER: &[u8] = b"stand-in answer";
pub const REGISTRY_NAME: &str = "standinvalue";
pub const DRIVER_STORE: &str = "C:\\stand-in";
const NODE_COUNT: u32 = 8;
/// A node the stand-in has no channel for, to test that a context there is refused.
pub const NODE_WITHOUT_CHANNEL: u32 = 7;
const COLOUR_8_BIT: u32 = 7;
const FORMAT_32_BIT: u32 = 21;
const EXPORTED_FILE: &str = "/dev/null";
const BYTES_PER_PIXEL: u32 = 4;
/// The stand-in's word for a surface of wide pixels, for testing what depends on them.
pub const WIDE_COLOUR: u32 = 0x29;
const WIDE_PIXEL_BYTES: u32 = 8;

/// See the crate documentation.
#[derive(Debug)]
pub struct StandIn {
    facts: AdapterFacts,
    next_id: u64,
    live_memory: HashSet<MemoryId>,
    spaces: HashMap<SpaceId, Space>,
    /// Set to make the next video memory allocation fail.
    pub fail_next_allocation: bool,
    /// Set to make the next mapping fail.
    pub fail_next_map: bool,
    /// Whether submissions go to the pretend GPU. If not, they finish at once.
    pub run_work: bool,
    pub doorbells: Vec<(u32, u64)>,
    pub closed_queues: Vec<u32>,
    pub preserved: Vec<(MemoryId, MemoryId, u64)>,
    pub copies: Vec<StagingCopy>,
    pub slot_copies: Vec<SlotCopy>,
    /// Copies that would have gone past what's mapped at their addresses, as an engine would run them.
    pub copies_past_their_memory: Vec<String>,
    /// Slot copies end in this. A test sets it to see a copy fail.
    pub slot_copy_status: Status,
    /// What each block of memory starts with, for a test to fill in.
    pub memory_starts: HashMap<MemoryId, Vec<u8>>,
    /// Set to make every export fail.
    pub refuse_exports: bool,
    /// Set to refuse every allocation's, context's and queue's private data, as a vendor's driver
    /// would refuse data it can't read.
    pub refuse_private_data: bool,
    /// Set to make every doorbell fail, as a submission call can.
    pub fail_rings: bool,
    /// Addresses taken through [`HostServices`] by the first submission.
    pub channel_address: Option<u64>,
    /// Copies stay under way until a test sets `copy_done`.
    pub slow_copies: bool,
    pub copy_done: bool,
    pub work_per_submission: u32,
    pub unfinished_limit: Option<u64>,
}

#[derive(Debug, Default)]
struct Space {
    ranges: Vec<GpuRange>,
    /// Address to length.
    mappings: BTreeMap<u64, u64>,
}

impl Space {
    fn overlaps(&self, address: u64, length: u64) -> bool {
        let before = self.mappings.range(..address.saturating_add(length)).next_back();
        before.is_some_and(|(&start, &size)| start.saturating_add(size) > address)
    }
}

/// What a copy touches on one side, in bytes across and rows down.
#[derive(Debug, Clone, Copy)]
struct Touched {
    left: i128,
    top: i128,
    width: i128,
    count: i128,
}

impl StandIn {
    pub fn new() -> Self {
        let card = PciCard { vendor: STAND_IN_VENDOR, device: 1, revision: 1 };
        let facts = AdapterFacts {
            card,
            name: "Stand-in GPU".into(),
            video_memory: 8 << 30,
            gtt_memory: 0,
            bar_size: 8 << 30,
            node_count: NODE_COUNT,
            engines: guesthouse_backend::AdapterEngines::all(),
            guest_driver_version: 0,
        };
        Self::with_facts(facts)
    }

    pub fn with_facts(facts: AdapterFacts) -> Self {
        Self {
            facts,
            next_id: 1,
            live_memory: HashSet::new(),
            spaces: HashMap::new(),
            fail_next_allocation: false,
            fail_next_map: false,
            run_work: false,
            doorbells: Vec::new(),
            closed_queues: Vec::new(),
            preserved: Vec::new(),
            copies: Vec::new(),
            slot_copies: Vec::new(),
            copies_past_their_memory: Vec::new(),
            slot_copy_status: Status::SUCCESS,
            memory_starts: HashMap::new(),
            refuse_exports: false,
            refuse_private_data: false,
            fail_rings: false,
            channel_address: None,
            slow_copies: false,
            copy_done: false,
            work_per_submission: 1,
            unfinished_limit: None,
        }
    }

    pub fn live_memory_blocks(&self) -> usize {
        self.live_memory.len()
    }

    pub fn live_address_spaces(&self) -> usize {
        self.spaces.len()
    }

    /// Mappings in every address space.
    pub fn mapping_count(&self) -> usize {
        self.spaces.values().map(|space| space.mappings.len()).sum()
    }

    /// Whether rows of a surface at an address stay inside the mapping there.
    fn rows_are_mapped(&self, space: SpaceId, address: u64, surface: &SurfaceShape, rows: &Touched) -> bool {
        let Some(&mapped) = self.spaces.get(&space).and_then(|space| space.mappings.get(&address)) else {
            return false;
        };
        let pitch = i128::from(surface.pitch);
        let end = (rows.top + rows.count.max(1) - 1) * pitch + rows.left + rows.width;
        rows.left + rows.width <= pitch && end <= i128::from(mapped)
    }

    /// What's mapped at an address, for a test to compare a copy with.
    pub fn mapped_length(&self, space: SpaceId, address: u64) -> Option<u64> {
        self.spaces.get(&space)?.mappings.get(&address).copied()
    }

    /// A tiled surface's sums are left to the test, so that they aren't checked against themselves.
    fn source_is_mapped(&self, space: SpaceId, address: u64, surface: &SurfaceShape, rows: &Touched) -> bool {
        surface.tiling.is_some() || self.rows_are_mapped(space, address, surface, rows)
    }

    /// The same goes for a tiled target.
    fn target_is_mapped(&self, space: SpaceId, address: u64, surface: &SurfaceShape, rows: &Touched) -> bool {
        surface.tiling.is_some() || self.rows_are_mapped(space, address, surface, rows)
    }

    fn note_staging_copy(&mut self, copy: &StagingCopy) {
        let pixel = i128::from(self.copy_pixel_bytes(&copy.source));
        let width = (i128::from(copy.right) - i128::from(copy.left)) * pixel;
        let count = i128::from(copy.bottom) - i128::from(copy.top);
        let read = Touched { left: i128::from(copy.left) * pixel, top: i128::from(copy.top), width, count };
        let written =
            Touched { left: i128::from(copy.target_left) * pixel, top: i128::from(copy.target_top), ..read };
        if !self.source_is_mapped(copy.own_space, copy.source_address, &copy.source, &read)
            || !self.target_is_mapped(copy.own_space, copy.target_address, &copy.target, &written)
        {
            self.copies_past_their_memory.push(format!("{copy:?}"));
        }
    }

    fn note_slot_copy(&mut self, copy: &SlotCopy) {
        let width = i128::from(copy.width) * i128::from(BYTES_PER_PIXEL);
        let read = Touched { left: 0, top: 0, width, count: i128::from(copy.height) };
        let slot = SurfaceShape { pitch: copy.slot_pitch, tiling: None, ..copy.source };
        if !self.source_is_mapped(copy.own_space, copy.source_address, &copy.source, &read)
            || !self.rows_are_mapped(copy.own_space, copy.slot_address, &slot, &read)
        {
            self.copies_past_their_memory.push(format!("{copy:?}"));
        }
    }

    fn new_memory(&mut self) -> MemoryId {
        let memory = MemoryId(self.next_id);
        self.next_id += 1;
        self.live_memory.insert(memory);
        memory
    }
}

impl Default for StandIn {
    fn default() -> Self {
        Self::new()
    }
}

/// Where private data can state a pitch. Zero means rows with no padding.
const PITCH_FIELD: usize = 20;
/// Where private data can ask for tiling, as one more than the log2 of the block height. Zero means
/// rows in order.
const TILING_FIELD: usize = 24;
/// Where private data can state a colour word. Zero means the usual one.
const COLOUR_FIELD: usize = 28;
/// The narrowest and shortest block of a pretend tiled surface, and the tallest log2 there is.
const TILE_WIDTH: u64 = 64;
const TILE_ROWS: u64 = 8;
const MOST_TILING: u32 = 5;
const TILED_CORNER_LIMIT: u64 = 1 << 16;

fn field(private: &[u8], at: usize) -> u32 {
    private.get(at..at + 4).map_or(0, |bytes| u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

impl Backend for StandIn {
    fn supports(card: &PciCard) -> bool {
        card.vendor == STAND_IN_VENDOR
    }

    fn survey(setup: &Setup) -> Result<DriverSurvey, Refusal> {
        let found = |version: String, branch: &str| DriverFound { version, branch: branch.into() };
        Ok(DriverSurvey {
            host: found("1.0".into(), &setup.guest_branch),
            guest: found(setup.guest_driver_version.to_string(), &setup.guest_branch),
            host_branch_known: true,
            guest_branch_known: true,
            host_version_tested: true,
            mixed_branches_tested: false,
            card: CardLabel::Tested,
        })
    }

    fn open(setup: &Setup, _survey: &DriverSurvey) -> Result<Self, Refusal> {
        let mut stand_in = Self::new();
        stand_in.facts.guest_driver_version = setup.guest_driver_version.packed();
        Ok(stand_in)
    }

    fn facts(&self) -> &AdapterFacts {
        &self.facts
    }

    fn allocate_video_memory(
        &mut self,
        size: u64,
        _kind: MemoryKind,
    ) -> Result<(MemoryId, u64), BackendError> {
        if std::mem::take(&mut self.fail_next_allocation) {
            return Err(BackendError::new("the stand-in was told to fail"));
        }
        Ok((self.new_memory(), size))
    }

    fn adopt_host_pages(&mut self, _host_address: u64, _size: u64) -> Result<MemoryId, BackendError> {
        Ok(self.new_memory())
    }

    fn release_memory(&mut self, memory: MemoryId) {
        assert!(self.live_memory.remove(&memory), "{memory:?} was freed twice or never made");
    }

    fn preserve_contents(
        &mut self,
        _host: &mut dyn HostServices,
        _own_space: SpaceId,
        from: MemoryId,
        to: MemoryId,
        size: u64,
    ) -> Result<(), BackendError> {
        self.preserved.push((from, to, size));
        Ok(())
    }

    fn create_address_space(&mut self, ranges: &[GpuRange]) -> Result<SpaceId, BackendError> {
        let space = SpaceId(self.next_id);
        self.next_id += 1;
        self.spaces.insert(space, Space { ranges: ranges.to_vec(), mappings: BTreeMap::new() });
        Ok(space)
    }

    fn destroy_address_space(&mut self, space: SpaceId) {
        assert!(self.spaces.remove(&space).is_some(), "{space:?} was destroyed twice");
    }

    fn map(&mut self, request: &MapRequest) -> Result<(), BackendError> {
        assert!(self.live_memory.contains(&request.memory), "mapping memory that does not exist");
        if std::mem::take(&mut self.fail_next_map) {
            return Err(BackendError::new("told to fail"));
        }
        let Some(space) = self.spaces.get_mut(&request.space) else {
            return Err(BackendError::new("this address space is closed"));
        };
        if !space.ranges.iter().any(|range| range.contains(request.address)) {
            return Err(BackendError::new("no range holds this address"));
        }
        if space.overlaps(request.address, request.length) {
            return Err(BackendError::new("these addresses are taken"));
        }
        space.mappings.insert(request.address, request.length);
        Ok(())
    }

    fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError> {
        let Some(space) = self.spaces.get_mut(&unmap.space) else {
            return Err(BackendError::new("this address space is closed"));
        };
        match space.mappings.remove(&unmap.address) {
            Some(length) if length == unmap.length => Ok(()),
            _ => Err(BackendError::new("no mapping like this one")),
        }
    }

    fn describe_allocation(&mut self, private: &[u8]) -> AllocationShape {
        let size = private
            .first_chunk::<8>()
            .map_or(DEFAULT_SIZE, |bytes| u64::from_le_bytes(*bytes))
            .max(DEFAULT_SIZE);
        let (width, height) = (field(private, 8), field(private, 12));
        let pitch = match field(private, PITCH_FIELD) {
            0 => width.saturating_mul(BYTES_PER_PIXEL),
            pitch => pitch,
        };
        let surface = (pitch != 0).then_some(SurfaceShape {
            width,
            height,
            format: FORMAT_32_BIT,
            pitch,
            tiling: field(private, TILING_FIELD).checked_sub(1),
            refresh: (60, 1),
            samples: (1, 0),
            colour: match field(private, COLOUR_FIELD) {
                0 => COLOUR_8_BIT,
                colour => colour,
            },
        });
        AllocationShape {
            size,
            exact_size: size,
            surface,
            gpu_cached: private.get(16).is_some_and(|&byte| byte != 0),
            kind: MemoryKind::PLAIN,
        }
    }

    fn finish_allocation_data(&mut self, private: &mut [u8], size: u64) {
        if let Some(first) = private.first_mut() {
            *first = size as u8 ^ 0xFF;
        }
    }

    fn finish_queue_data(
        &mut self,
        _queue: u32,
        private: &mut [u8],
        serial: u32,
    ) -> Result<(), BackendError> {
        if self.refuse_private_data {
            return Err(BackendError::new("the stand-in was told to refuse"));
        }
        if let Some(first) = private.first_mut() {
            *first = serial as u8;
        }
        Ok(())
    }

    fn escape(&mut self, _process: u32, data: &mut [u8], _books: &GuestBooks<'_>) {
        data.iter_mut().for_each(|byte| *byte ^= 0xFF);
    }

    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
        (query.kind == VENDOR_ONLY_QUERY).then(|| {
            out.extend_from_slice(VENDOR_ANSWER);
            Status::SUCCESS
        })
    }

    fn registry_value(&mut self, name: &RegistryName) -> Option<RegistryValue> {
        let utf16 = |text: &str| text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        match name {
            RegistryName::DriverStorePath | RegistryName::DriverImagePath => {
                Some(RegistryValue { value_type: None, bytes: utf16(DRIVER_STORE) })
            }
            RegistryName::Value(name) if name.eq_ignore_ascii_case(REGISTRY_NAME) => {
                Some(RegistryValue { value_type: Some(1), bytes: utf16("value") })
            }
            RegistryName::Value(_) => None,
        }
    }

    fn gpu_clock(&mut self) -> Result<u64, BackendError> {
        Ok(self.next_id * 1000)
    }

    fn start_work(&mut self, host: &mut dyn HostServices, work: &Work<'_>) -> WorkStart {
        if !self.run_work || !work.may_run || work.queue_failed {
            return WorkStart::Finished;
        }
        if self.channel_address.is_none() {
            self.channel_address = host.take_gpu_addresses(0x10000, 0x10000).ok();
        }
        WorkStart::Started { doorbell: work.fence_value, work: self.work_per_submission }
    }

    fn ring_doorbell(&mut self, queue: u32, doorbell: u64) -> Result<(), BackendError> {
        if self.fail_rings {
            return Err(BackendError::new("the stand-in was told to fail its doorbells"));
        }
        self.doorbells.push((queue, doorbell));
        Ok(())
    }

    fn close_queue(&mut self, queue: u32) {
        self.closed_queues.push(queue);
    }

    fn start_staging_copy(&mut self, _host: &mut dyn HostServices, copy: &StagingCopy) -> StagingStart {
        self.note_staging_copy(copy);
        self.copies.push(*copy);
        if self.slow_copies { StagingStart::Running } else { StagingStart::Finished(Status::SUCCESS) }
    }

    fn poll_staging_copy(&mut self, _host: &mut dyn HostServices, _now: Instant) -> Option<Status> {
        std::mem::take(&mut self.copy_done).then_some(Status::SUCCESS)
    }

    fn start_slot_copy(&mut self, _host: &mut dyn HostServices, copy: &SlotCopy) -> StagingStart {
        self.note_slot_copy(copy);
        self.slot_copies.push(*copy);
        if self.slow_copies { StagingStart::Running } else { StagingStart::Finished(self.slot_copy_status) }
    }

    fn copy_pixel_bytes(&self, surface: &SurfaceShape) -> u32 {
        if surface.colour == WIDE_COLOUR { WIDE_PIXEL_BYTES } else { BYTES_PER_PIXEL }
    }

    /// As a copy engine lays one out: whole blocks of 64 bytes by `8 << block` rows, and a corner
    /// that has to fit 16 bits each way.
    fn tiled_bytes(&self, surface: &SurfaceShape, x: u64, y: u64) -> Option<u64> {
        let block = surface.tiling.filter(|&block| block <= MOST_TILING)?;
        if x >= TILED_CORNER_LIMIT || y >= TILED_CORNER_LIMIT {
            return None;
        }
        let rows = u64::from(surface.height).next_multiple_of(TILE_ROWS << block);
        let width = u64::from(surface.pitch).next_multiple_of(TILE_WIDTH);
        rows.checked_mul(width)
    }

    fn read_memory_start(
        &mut self,
        _host: &mut dyn HostServices,
        _own_space: SpaceId,
        memory: MemoryId,
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        out.fill(0);
        if let Some(start) = self.memory_starts.get(&memory) {
            let length = start.len().min(out.len());
            out[..length].copy_from_slice(&start[..length]);
        }
        Ok(())
    }

    /// Anything that is a file will do, as nothing here has pixels.
    fn export_memory(&mut self, _memory: MemoryId, _size: u64) -> Result<OwnedFd, BackendError> {
        if self.refuse_exports {
            return Err(BackendError::new("the stand-in was told to refuse"));
        }
        let file = File::open(EXPORTED_FILE).map_err(|error| BackendError::new(error.to_string()))?;
        Ok(file.into())
    }

    fn context_allowed(&mut self, node: u32, _client_hint: u32) -> bool {
        node != NODE_WITHOUT_CHANNEL
    }

    fn allocation_data_valid(&mut self, _private: &[u8]) -> bool {
        !self.refuse_private_data
    }

    fn context_data_valid(&mut self, _process: u32, _node: u32, _private: &[u8]) -> bool {
        !self.refuse_private_data
    }

    fn unfinished_work_limit(&self) -> Option<u64> {
        self.unfinished_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(space: SpaceId, memory: MemoryId, address: u64, length: u64) -> MapRequest {
        MapRequest {
            space,
            memory,
            offset: 0,
            length,
            address,
            small_pages: false,
            gpu_cached: false,
            readable: true,
            writable: true,
            executable: false,
            memory_type: guesthouse_backend::MapMemoryType::Default,
        }
    }

    #[test]
    fn mapping_refuses_taken_addresses() {
        let mut gpu = StandIn::new();
        let space = gpu.create_address_space(&[GpuRange { start: 0, end: 0x10000 }]).unwrap();
        let (memory, _) = gpu.allocate_video_memory(0x1000, MemoryKind::PLAIN).unwrap();
        gpu.map(&map(space, memory, 0x1000, 0x1000)).unwrap();
        assert!(gpu.map(&map(space, memory, 0x1800, 0x1000)).is_err());
        assert!(gpu.map(&map(space, memory, 0x800, 0x1000)).is_err());
        assert!(gpu.map(&map(space, memory, 0x2000, 0x1000)).is_ok());
    }

    #[test]
    fn mapping_refuses_addresses_outside_the_ranges() {
        let mut gpu = StandIn::new();
        let space = gpu.create_address_space(&[GpuRange { start: 0x1000, end: 0x2000 }]).unwrap();
        let (memory, _) = gpu.allocate_video_memory(0x1000, MemoryKind::PLAIN).unwrap();
        assert!(gpu.map(&map(space, memory, 0x2000, 0x10)).is_err());
    }

    #[test]
    fn private_data_describes_size_and_surface() {
        let mut private = vec![0; 17];
        private[..8].copy_from_slice(&0x20000u64.to_le_bytes());
        private[8] = 16;
        private[12] = 8;
        let shape = StandIn::new().describe_allocation(&private);
        assert_eq!(shape.size, 0x20000);
        assert_eq!(shape.surface.map(|surface| surface.pitch), Some(64));
        assert_eq!(StandIn::new().describe_allocation(b"short").size, DEFAULT_SIZE);
    }
}
