//! A backend that answers vendor questions from the recording.

use guesthouse_backend::{
    AdapterFacts, AllocationShape, Backend, BackendError, COPY_PIXEL_BYTES, DriverSurvey, GpuRange,
    GuestBooks, HostServices, MapRequest, MemoryId, MemoryKind, PciCard, Refusal, RegistryName,
    RegistryValue, Setup, SpaceId, StagingCopy, StagingStart, Status, SurfaceShape, UnmapRequest,
    VendorQuery, Work, WorkStart,
};
use guesthouse_backend_standin::StandIn;
use guesthouse_messages::adapter::AdapterQuery;
use guesthouse_messages::address::PAGE_SIZE;
use guesthouse_messages::allocation::{CreateAllocation, CreateAllocationAnswer};
use guesthouse_messages::queue::{CreateHardwareQueue, SubmitToQueue};
use guesthouse_messages::standard::StandardRecord;
use guesthouse_messages::wire::{align_up, u32_at, u64_at};
use guesthouse_messages::{Command, HEADER_SIZE, MessageHeader};
use std::collections::VecDeque;
use std::time::Instant;

/// Adapter queries the core answers by itself, and so are really compared.
const NEUTRAL_QUERIES: [u32; 8] = [
    AdapterQuery::SEGMENT_SIZES,
    AdapterQuery::DRIVER_MODEL,
    AdapterQuery::DRIVER_MODEL_FOR_RENDER,
    AdapterQuery::DIRECT_FLIP,
    AdapterQuery::CONSOLE_CAPS,
    AdapterQuery::PHYSICAL_ADAPTER_COUNT,
    AdapterQuery::PHYSICAL_ADAPTER_IDS,
    AdapterQuery::DESCRIPTION,
];

/// A surface's answer fields, in their order on the wire.
const SURFACE_FIELDS: usize = 7;
const STATUS_SIZE: usize = 4;
const CLOCK_ANSWER_GPU_CLOCK: usize = 12;
/// So large that any cap refuses it, for a request the recording shows was refused.
const OVER_ANY_CAP: u64 = 1 << 50;

/// Where a submission's count of ring entries is, which the vendor reads.
const PRIVATE_ENTRIES: usize = 0x14;
/// The reference daemon's limit on what a queue may have unfinished.
const RING_ROOM_LIMIT: u64 = 4000;

fn submission_private(message: &[u8]) -> &[u8] {
    SubmitToQueue::parse(message).map_or(&[], |request| request.private)
}

/// How much of a reply came from the recording and not from the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provenance {
    /// The core worked out all of it.
    Worked,
    /// Vendor fields inside it came from the recording, and the rest was worked out.
    PartlyRecorded,
    /// The whole answer came from the recording.
    Recorded,
}

/// See the module documentation. Memory and address spaces go to the stand-in, which keeps real books.
pub struct RecordedAnswers {
    inner: StandIn,
    expected: Vec<u8>,
    /// For the message at hand: where each entry's private data is in the answer, and its index.
    vendor_entries: Vec<(usize, usize)>,
    next_described: usize,
    next_finished: usize,
    work: VecDeque<bool>,
    work_entries: u32,
    work_limit: u64,
    provenance: Provenance,
}

impl RecordedAnswers {
    pub fn new(facts: AdapterFacts) -> Self {
        Self {
            inner: StandIn::with_facts(facts),
            expected: Vec::new(),
            vendor_entries: Vec::new(),
            next_described: 0,
            next_finished: 0,
            work: VecDeque::new(),
            work_entries: 0,
            work_limit: RING_ROOM_LIMIT,
            provenance: Provenance::Worked,
        }
    }

    /// Readies for a message, given the reply the recording has for it.
    pub fn expect(&mut self, message: &[u8], expected: &[u8], work: impl Iterator<Item = bool>) {
        self.expected.clear();
        self.expected.extend_from_slice(expected);
        self.next_described = 0;
        self.next_finished = 0;
        self.work.clear();
        self.work.extend(work);
        self.work_entries = u32_at(submission_private(message), PRIVATE_ENTRIES).unwrap_or(0);
        self.provenance = Provenance::Worked;
        self.vendor_entries.clear();
        let is_create =
            MessageHeader::parse(message).is_some_and(|header| header.command == Command::CreateAllocation);
        if let (true, Ok(request)) = (is_create, CreateAllocation::parse(message)) {
            let mut private_at = CreateAllocationAnswer::private_data_start(request.count);
            for (index, entry) in request.entries().enumerate() {
                if StandardRecord::parse(entry.private).is_none() {
                    self.vendor_entries.push((private_at, index));
                }
                private_at += entry.private.len();
            }
        }
    }

    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    fn note(&mut self, provenance: Provenance) {
        self.provenance = self.provenance.max(provenance);
    }

    fn was_refused(&self) -> bool {
        u32_at(&self.expected, 0).is_ok_and(|status| status != Status::SUCCESS.0)
    }
}

impl Backend for RecordedAnswers {
    fn supports(_card: &PciCard) -> bool {
        true
    }

    fn survey(_setup: &Setup) -> Result<DriverSurvey, Refusal> {
        Err(Refusal("a replay is built by hand, and has no drivers to survey".into()))
    }

    fn open(_setup: &Setup, _survey: &DriverSurvey) -> Result<Self, Refusal> {
        Err(Refusal("a replay is built by hand, and has no card to open".into()))
    }

    fn facts(&self) -> &AdapterFacts {
        self.inner.facts()
    }

    /// Whole pages, as a real driver gives. Only an allocation grown by a map gets here unrounded,
    /// and the recording does not say what the vendor's driver made of that.
    fn allocate_video_memory(
        &mut self,
        size: u64,
        kind: MemoryKind,
    ) -> Result<(MemoryId, u64), BackendError> {
        self.inner.allocate_video_memory(align_up(size, PAGE_SIZE), kind)
    }

    fn adopt_host_pages(&mut self, host_address: u64, size: u64) -> Result<MemoryId, BackendError> {
        self.inner.adopt_host_pages(host_address, size)
    }

    fn release_memory(&mut self, memory: MemoryId) {
        self.inner.release_memory(memory);
    }

    fn preserve_contents(
        &mut self,
        _host: &mut dyn HostServices,
        _own_space: SpaceId,
        _from: MemoryId,
        _to: MemoryId,
        _size: u64,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn create_address_space(&mut self, ranges: &[GpuRange]) -> Result<SpaceId, BackendError> {
        self.inner.create_address_space(ranges)
    }

    fn destroy_address_space(&mut self, space: SpaceId) {
        self.inner.destroy_address_space(space);
    }

    fn map(&mut self, request: &MapRequest) -> Result<(), BackendError> {
        self.inner.map(request)
    }

    fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError> {
        self.inner.unmap(unmap)
    }

    /// Reads the size and surface back from the recorded answer.
    fn describe_allocation(&mut self, _private: &[u8]) -> AllocationShape {
        self.note(Provenance::PartlyRecorded);
        if self.vendor_entries.is_empty() {
            return AllocationShape {
                size: 0,
                exact_size: 0,
                surface: None,
                gpu_cached: false,
                kind: MemoryKind::PLAIN,
            };
        }
        let (_, index) = self.vendor_entries[self.next_described % self.vendor_entries.len()];
        self.next_described += 1;
        if self.was_refused() {
            return AllocationShape {
                size: OVER_ANY_CAP,
                exact_size: 0,
                surface: None,
                gpu_cached: false,
                kind: MemoryKind::PLAIN,
            };
        }
        let size = CreateAllocationAnswer::size_of_entry(&self.expected, index).unwrap_or(0);
        let fields = CreateAllocationAnswer::surface_of_entry(&self.expected, index);
        let surface = fields.ok().filter(|fields| *fields != [0; SURFACE_FIELDS]).map(|fields| {
            let [width, height, format, samples, quality, refresh, refresh_base] = fields;
            SurfaceShape {
                width,
                height,
                format,
                // The answer has no pitch in it, and rows with no padding are the least a surface has.
                pitch: width.saturating_mul(COPY_PIXEL_BYTES),
                tiling: None,
                refresh: (refresh, refresh_base),
                samples: (samples, quality),
                colour: 0,
            }
        });
        AllocationShape { size, exact_size: size, surface, gpu_cached: false, kind: MemoryKind::PLAIN }
    }

    fn finish_allocation_data(&mut self, private: &mut [u8], _size: u64) {
        self.note(Provenance::PartlyRecorded);
        if self.vendor_entries.is_empty() {
            return;
        }
        let (at, _) = self.vendor_entries[self.next_finished % self.vendor_entries.len()];
        self.next_finished += 1;
        if let Some(recorded) = self.expected.get(at..at + private.len()) {
            private.copy_from_slice(recorded);
        }
    }

    fn finish_queue_data(
        &mut self,
        _queue: u32,
        private: &mut [u8],
        _serial: u32,
    ) -> Result<(), BackendError> {
        self.note(Provenance::PartlyRecorded);
        let start = CreateHardwareQueue::PRIVATE;
        if let Some(recorded) = self.expected.get(start..start + private.len()) {
            private.copy_from_slice(recorded);
        }
        Ok(())
    }

    fn escape(&mut self, _process: u32, data: &mut [u8], _books: &GuestBooks<'_>) {
        self.note(Provenance::Recorded);
        let length = data.len().min(self.expected.len());
        data[..length].copy_from_slice(&self.expected[..length]);
    }

    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
        if NEUTRAL_QUERIES.contains(&query.kind) {
            return None;
        }
        self.note(Provenance::Recorded);
        let status = u32_at(&self.expected, 0).ok()?;
        out.extend_from_slice(self.expected.get(STATUS_SIZE..)?);
        Some(Status(status))
    }

    fn registry_value(&mut self, _name: &RegistryName) -> Option<RegistryValue> {
        None
    }

    fn gpu_clock(&mut self) -> Result<u64, BackendError> {
        self.note(Provenance::PartlyRecorded);
        u64_at(&self.expected, CLOCK_ANSWER_GPU_CLOCK).map_err(|_| BackendError::new("no recorded clock"))
    }

    /// What the recording says became of this submission.
    fn start_work(&mut self, _host: &mut dyn HostServices, _work: &Work<'_>) -> WorkStart {
        match self.work.pop_front() {
            Some(true) => WorkStart::Started { doorbell: 0, work: self.work_entries },
            _ => WorkStart::Finished,
        }
    }

    fn ring_doorbell(&mut self, _queue: u32, _doorbell: u64) -> Result<(), BackendError> {
        Ok(())
    }

    fn close_queue(&mut self, _queue: u32) {}

    fn start_staging_copy(&mut self, _host: &mut dyn HostServices, _copy: &StagingCopy) -> StagingStart {
        self.note(Provenance::PartlyRecorded);
        StagingStart::Finished(u32_at(&self.expected, 0).map_or(Status::UNSUCCESSFUL, Status))
    }

    fn poll_staging_copy(&mut self, _host: &mut dyn HostServices, _now: Instant) -> Option<Status> {
        Some(Status::SUCCESS)
    }

    /// A refused context answers with a handle of zero, and a recording has no other way to say so.
    fn context_allowed(&mut self, _node: u32, _client_hint: u32) -> bool {
        let refused = u32_at(&self.expected, HEADER_SIZE).is_ok_and(|handle| handle == 0);
        if refused {
            self.note(Provenance::PartlyRecorded);
        }
        !refused
    }

    fn unfinished_work_limit(&self) -> Option<u64> {
        Some(self.work_limit)
    }
}
