//! The session: the tables for one guest, and the dispatch from messages to handlers.

mod adapter;
mod allocation;
mod context;
mod copies;
mod copy_bounds;
mod frame_ring;
mod indirect;
mod mapping;
mod reserving;
mod sync;
#[cfg(test)]
mod tests;
mod waiting;

use crate::config::Config;
use crate::deferred::Deferred;
use crate::fault_rate::FaultRate;
use crate::fences::FencePool;
use crate::frames::ScreenFrames;
use crate::gpu_addresses::{AddressUse, GpuAddresses};
use crate::guest_ranges::GuestRanges;
use crate::handles::HandleCounter;
use crate::io_space::IoSpace;
use crate::pending::{Gate, Pending, SharedWait};
use crate::sync_table::SyncTable;
use crate::tables::{Allocation, Mapping, Queue, ReservedRange};
use crate::timing::Timings;
use copies::{CopyJob, Parked};
use guesthouse_backend::{
    AllocationShape, Backend, BackendError, GuestMemory, GuestSpan, HostServices, MemoryError, PagesView,
    SharedSpan, SpaceId,
};
use guesthouse_messages::notice::Notice;
use guesthouse_messages::wire::{Malformed, put_u32};
use guesthouse_messages::{Command, MessageHeader, Status};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

/// Where a reply goes, which only the caller can read. The core just hands it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplyTicket {
    pub channel: u8,
    pub transaction: u64,
}

/// What became of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Answer {
    /// The reply is in the buffer.
    Now,
    /// The reply will come from [`Session::take_replies`]. The guest thread that sent this waits,
    /// and every other message goes on being served.
    Held,
}

/// A reply that was held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldReply {
    pub ticket: ReplyTicket,
    pub bytes: Vec<u8>,
}

/// Why a handler gave up. The guest gets a bare failure status either way.
#[derive(Debug)]
pub(crate) enum Failure {
    Malformed,
    Backend(BackendError),
}

impl From<Malformed> for Failure {
    fn from(_: Malformed) -> Self {
        Self::Malformed
    }
}

impl From<MemoryError> for Failure {
    fn from(error: MemoryError) -> Self {
        Self::Backend(error.into())
    }
}

impl From<BackendError> for Failure {
    fn from(error: BackendError) -> Self {
        Self::Backend(error)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => Malformed.fmt(f),
            Self::Backend(error) => error.fmt(f),
        }
    }
}

pub(crate) type Handled = Result<(), Failure>;

/// See [`Session::table_sizes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSizes {
    pub allocations: usize,
    pub resources: usize,
    pub resource_users: usize,
    pub shared_objects: usize,
    pub deferred: usize,
}

/// Everything the host knows about one guest. See the crate documentation.
pub struct Session<B, M> {
    pub(crate) backend: B,
    pub(crate) memory: M,
    pub(crate) config: Config,
    pub(crate) started: Instant,

    pub(crate) handles: HandleCounter,
    pub(crate) gpu_addresses: GpuAddresses,
    pub(crate) guest_ranges: GuestRanges,
    pub(crate) fences: FencePool,
    /// Where the fence pages are, once the guest has said and the place has passed.
    pub(crate) io_space: Option<IoSpace>,

    pub(crate) processes_32_bit: HashSet<u32>,
    /// Each process's PID in the guest, by the handle this side gave it.
    pub(crate) guest_pids: HashMap<u32, u32>,
    pub(crate) spaces: HashMap<u32, SpaceId>,
    /// The daemon's own address space, apart from `spaces` so that no message can name it.
    own_space: Option<SpaceId>,
    pub(crate) closed_spaces: HashSet<SpaceId>,

    pub(crate) allocations: HashMap<u32, Allocation>,
    /// The vendor's private data given for the guest kernel's own surfaces, newest last, so that
    /// a creation that hands it back is known for one of them.
    pub(crate) standard_data: VecDeque<(Vec<u8>, AllocationShape)>,
    pub(crate) resources: HashMap<u32, Vec<u32>>,
    /// Users of a resource, when more than the process that made it.
    pub(crate) resource_users: HashMap<u32, u32>,
    /// Shared handle to the object behind it.
    pub(crate) shared_objects: HashMap<u32, u32>,

    pub(crate) sync_objects: SyncTable,
    pub(crate) paging_queues: HashMap<u32, u64>,
    /// A queue's progress sync object to its fence.
    pub(crate) queue_progress: HashMap<u32, u64>,

    pub(crate) queues: HashMap<u32, Queue>,
    pub(crate) context_queues: HashMap<u32, Vec<u32>>,
    pub(crate) context_nodes: HashMap<u32, u32>,
    pub(crate) context_hints: HashMap<u32, u32>,
    /// The process each context was made for, as the one that destroys it may be another.
    pub(crate) context_processes: HashMap<u32, u32>,
    /// Each device's limit on queued presents, once it has set one.
    pub(crate) present_limits: HashMap<u32, u32>,
    /// What each process has asked to keep of each memory group, by (process, group).
    pub(crate) reservations: HashMap<(u32, u32), u64>,
    pub(crate) gates: HashMap<u32, Gate>,
    /// The number of the next wait to join a line.
    pub(crate) next_wait: u64,
    /// Waits that hold more than one queue, by number.
    pub(crate) shared_waits: HashMap<u64, SharedWait>,
    /// Allocations whose first lock has waited for the GPU once, and goes ahead when it comes back.
    pub(crate) first_locks_waited: HashSet<u32>,
    /// The display driver has registered a ring since the guest last started, so its process holds
    /// the screen and no allocation is a primary from here on.
    pub(crate) display_driver_seen: bool,
    pub(crate) queues_made: u32,
    /// Statistics have been refused once, which is worth saying once.
    pub(crate) statistics_refused: bool,
    /// Command numbers that were answered as not supported, so that each is worth one warning.
    unknown_commands: HashSet<u32>,
    /// Copies refused so far, as only the first few are worth a warning.
    pub(crate) copies_refused: u64,
    /// Submissions that reached the GPU, which may have written any memory mapped for it since.
    pub(crate) work_started: u64,
    /// How many queues of each address space have stopped answering since it last came off the GPU.
    pub(crate) failed_queues: HashMap<SpaceId, u32>,
    pub(crate) faults: FaultRate,

    /// Where a mapping the host placed sits: address to size and alignment.
    pub(crate) mapped_addresses: HashMap<u64, (u64, u64)>,
    /// Mappings the host placed in a gap of one space because the guest's window lay below the
    /// next new address: their start and size, to unplace when they go.
    pub(crate) placed_mappings: HashMap<u64, u64>,
    pub(crate) mapped_so_far: u64,
    /// In the order the guest asked, which decides who gets a freed range next.
    pub(crate) reserved_addresses: Vec<ReservedRange>,

    pub(crate) copies: VecDeque<CopyJob>,
    pub(crate) next_copy: u64,
    pub(crate) parked: VecDeque<Parked>,
    pub(crate) frames: ScreenFrames,

    pub(crate) ticket: Option<ReplyTicket>,
    pub(crate) held: bool,
    pub(crate) replies: Vec<HeldReply>,
    pub(crate) timings: Timings,

    pub(crate) deferred: Vec<Deferred>,
    pub(crate) pending: Vec<Pending>,
    pub(crate) pending_spare: Vec<Pending>,
    pub(crate) notices: Vec<Notice>,
}

impl<B: Backend, M: GuestMemory> Session<B, M> {
    pub fn new(backend: B, memory: M, config: Config) -> Self {
        let gpu_addresses = GpuAddresses::keeping_clear(backend.kept_gpu_ranges());
        Self {
            backend,
            memory,
            config,
            started: Instant::now(),
            handles: HandleCounter::new(),
            gpu_addresses,
            guest_ranges: GuestRanges::new(),
            fences: FencePool::new(),
            io_space: None,
            processes_32_bit: HashSet::new(),
            guest_pids: HashMap::new(),
            spaces: HashMap::new(),
            own_space: None,
            closed_spaces: HashSet::new(),
            allocations: HashMap::new(),
            standard_data: VecDeque::new(),
            resources: HashMap::new(),
            resource_users: HashMap::new(),
            shared_objects: HashMap::new(),
            sync_objects: SyncTable::new(),
            paging_queues: HashMap::new(),
            queue_progress: HashMap::new(),
            queues: HashMap::new(),
            context_queues: HashMap::new(),
            context_nodes: HashMap::new(),
            context_hints: HashMap::new(),
            context_processes: HashMap::new(),
            present_limits: HashMap::new(),
            reservations: HashMap::new(),
            next_wait: 0,
            shared_waits: HashMap::new(),
            first_locks_waited: HashSet::new(),
            display_driver_seen: false,
            gates: HashMap::new(),
            queues_made: 0,
            statistics_refused: false,
            unknown_commands: HashSet::new(),
            copies_refused: 0,
            work_started: 0,
            failed_queues: HashMap::new(),
            faults: FaultRate::new(),
            mapped_addresses: HashMap::new(),
            placed_mappings: HashMap::new(),
            mapped_so_far: 0,
            reserved_addresses: Vec::new(),
            copies: VecDeque::new(),
            next_copy: 0,
            parked: VecDeque::new(),
            frames: ScreenFrames::default(),
            ticket: None,
            held: false,
            replies: Vec::new(),
            timings: Timings::new(false),
            deferred: Vec::new(),
            pending: Vec::new(),
            pending_spare: Vec::new(),
            notices: Vec::new(),
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn memory(&self) -> &M {
        &self.memory
    }

    pub fn memory_mut(&mut self) -> &mut M {
        &mut self.memory
    }

    /// Takes GPU addresses as a backend would, for a replay standing in for one.
    pub fn take_gpu_addresses(&mut self, size: u64, align: u64) -> Result<u64, BackendError> {
        self.gpu_addresses.take(size, align, AddressUse::Host)
    }

    /// Chooses how copies to the staging surface wait.
    pub fn set_copy_order(&mut self, order: crate::CopyOrder) {
        self.config.copy_order = order;
    }

    /// How much is being kept, to show that it comes back down when processes finish.
    pub fn table_sizes(&self) -> TableSizes {
        TableSizes {
            allocations: self.allocations.len(),
            resources: self.resources.len(),
            resource_users: self.resource_users.len(),
            shared_objects: self.shared_objects.len(),
            deferred: self.deferred.len(),
        }
    }

    pub fn timings(&self) -> &Timings {
        &self.timings
    }

    pub fn timings_mut(&mut self) -> &mut Timings {
        &mut self.timings
    }

    /// Answers one message. `ticket` is `None` if the guest wants no reply.
    ///
    /// The reply replaces what is in `out`, which the caller should reuse. A handler that fails
    /// answers with a bare failure status, as the guest must never be left waiting.
    pub fn handle_message(
        &mut self,
        message: &[u8],
        ticket: Option<ReplyTicket>,
        out: &mut Vec<u8>,
    ) -> Answer {
        out.clear();
        self.ticket = ticket;
        self.held = false;
        let command = MessageHeader::parse(message).map(|header| header.command);
        if let Err(failure) = self.dispatch(command, message, out) {
            log::warn!("{command:?} failed: {failure}");
            out.clear();
            put_u32(out, Status::UNSUCCESSFUL.0);
            return Answer::Now;
        }
        // With GUESTHOUSE_VERBOSE, every message gets a line, as some only the core sees, and a
        // status that isn't success shows in the reply's first bytes for most of them.
        log::debug!(
            "message {command:?} process={:#x} length={} reply_length={} held={} reply_start={}",
            MessageHeader::process_of(message).unwrap_or(0),
            message.len(),
            out.len(),
            self.held,
            ReplyStart(out)
        );
        if self.held { Answer::Held } else { Answer::Now }
    }

    /// Messages the host starts by itself, waiting to go out on the global channel.
    pub fn take_notices(&mut self) -> impl Iterator<Item = Notice> + '_ {
        self.notices.drain(..)
    }

    /// Replies that were held and now can go.
    pub fn take_replies(&mut self) -> impl Iterator<Item = HeldReply> + '_ {
        self.replies.drain(..)
    }

    /// The reply to the message at hand will come later.
    pub(crate) fn hold_reply(&mut self) {
        self.held = true;
    }

    /// The guest has restarted, which only a vendor's backend may care about.
    pub fn guest_restarted(&mut self) {
        self.backend.guest_restarted();
    }

    fn dispatch(&mut self, command: Option<Command>, message: &[u8], out: &mut Vec<u8>) -> Handled {
        use Command as C;
        match command {
            Some(C::OpenAdapter) => self.open_adapter(message, out)?,
            Some(C::GetAdapterIdentity) => self.adapter_identity(out),
            Some(C::SetIoSpace) => self.set_io_space(message, out)?,
            Some(C::DriverEscape) => self.driver_escape(message, out)?,
            Some(C::QueryAdapter) => self.query_adapter(message, out)?,
            Some(C::QueryDriverCaps) => self.query_driver_caps(message, out)?,
            Some(C::DescribeNode) => self.describe_node(message, out)?,
            Some(C::QueryVideoMemory) => self.query_video_memory(message, out)?,
            Some(C::ChangeVideoMemoryReservation) => self.change_video_memory_reservation(message, out)?,
            Some(C::QueryClockCalibration) => self.query_clock(out)?,
            Some(C::GetRegistryKeys) => answer_no_registry_keys(out),
            Some(C::QueryStatistics) => self.refuse_statistics(out),
            Some(C::MarkDeviceFailed) => self.mark_device_failed(message, out)?,

            Some(C::CreateProcess) => self.create_process(message, out)?,
            Some(C::DestroyProcess) => self.destroy_process(message, out)?,
            Some(C::CreateDevice) => {
                let handle = self.handles.take();
                put_u32(out, handle);
            }

            Some(C::CreateAllocation) => self.create_allocation(message, out)?,
            Some(C::DestroyAllocation) => self.destroy_allocation(message, out)?,
            Some(C::DescribeStandardAllocation) => self.describe_standard_allocation(message, out)?,
            Some(C::OpenResource) => self.open_resource(message, out)?,
            Some(C::LockForCpu) => self.lock_for_cpu(message, out)?,
            Some(C::SetAllocationPriority) => self.set_allocation_priority(message, out)?,
            Some(C::GetAllocationPriority) => self.get_allocation_priority(message, out)?,
            Some(C::SetGuestPages) => self.set_guest_pages(message, out)?,

            Some(C::MapGpuAddresses) => self.map_gpu_addresses(message, out)?,
            Some(C::ReserveGpuAddresses) => self.reserve_gpu_addresses(message, out)?,
            Some(C::UpdateGpuAddresses) => self.update_gpu_addresses(message, out)?,
            Some(C::MakeResident | C::Evict) => {
                guesthouse_messages::address::two_values_and_status(0, 0, Status::SUCCESS, out)
            }

            Some(C::CreateSyncObject) => self.create_sync_object(message, out)?,
            Some(C::OpenSyncObject) => self.open_sync_object(message, out)?,
            Some(C::DestroySyncObject) => self.destroy_sync_object(message, out)?,
            Some(C::CreateSharedObject) => self.create_shared_object(message, out)?,
            Some(C::DestroySharedObject) => self.destroy_shared_object(message, out)?,
            Some(C::CreatePagingQueue) => self.create_paging_queue(out)?,
            Some(C::DestroyPagingQueue) => self.destroy_paging_queue(message, out)?,
            Some(C::SignalSyncObject) => self.signal_sync_object(message, out)?,
            Some(C::SignalSharedFence) => self.signal_shared_fence(message, out)?,
            Some(C::SignalFence) => self.signal_fence(message, out)?,
            Some(C::WaitOnCpu) => self.wait_on_cpu(message, out)?,
            Some(C::WaitOnGpu) => self.wait_on_gpu(message, out)?,

            Some(C::CreateContext) => self.create_context(message, out)?,
            Some(C::DestroyContext) => self.destroy_context(message, out)?,
            Some(C::CreateHardwareQueue) => self.create_hardware_queue(message, out)?,
            Some(C::DestroyHardwareQueue) => self.destroy_hardware_queue(message, out)?,
            Some(C::SubmitToHardwareQueue) => self.submit_to_hardware_queue(message, out)?,
            Some(C::SubmitToContext) => self.submit_to_context(message, out)?,
            Some(C::SetQueuedLimit) => self.queued_limit(message, out)?,
            Some(C::PresentToken) => self.present_token(message, out)?,
            Some(C::CopyToStaging) => self.copy_to_staging(message, out)?,

            Some(C::FreeGpuAddresses) => self.free_gpu_addresses(message, out)?,

            Some(C::DestroyDevice) => self.destroy_device(message, out),
            Some(
                C::CloseAdapter
                | C::StopAdapter
                | C::FlushAdapter
                | C::FlushDevice
                | C::FlushHeapTransitions
                | C::UnlockForCpu
                | C::SetContextPriority
                | C::SetFlipFenceValue
                | C::IsSyncObjectSignaled,
            ) => answer_status(Status::SUCCESS, out),

            Some(C::Unknown(number)) => {
                if self.unknown_commands.insert(number) {
                    log::warn!("the guest sent command {number}, which isn't supported");
                }
                answer_status(Status::NOT_SUPPORTED, out);
            }
            None => answer_status(Status::NOT_SUPPORTED, out),
        }
        Ok(())
    }

    /// The address space of `process`, made the first time it is needed.
    pub(crate) fn space_for(&mut self, process: u32) -> Result<SpaceId, BackendError> {
        if let Some(&space) = self.spaces.get(&process) {
            return Ok(space);
        }
        let space = self.new_space()?;
        self.spaces.insert(process, space);
        Ok(space)
    }

    /// An address space with the fences in it, as every space has.
    fn new_space(&mut self) -> Result<SpaceId, BackendError> {
        let space = self.backend.create_address_space(&crate::GPU_RANGES)?;
        let block = self.fence_block()?;
        self.backend.map(&guesthouse_backend::MapRequest {
            space,
            memory: block.memory,
            offset: 0,
            length: crate::FENCE_AREA,
            address: block.gpu_address,
            small_pages: true,
            gpu_cached: false,
            readable: true,
            writable: true,
            executable: false,
            memory_type: guesthouse_backend::MapMemoryType::Default,
        })?;
        Ok(space)
    }

    /// The daemon's own address space, for its copies. Whatever is mapped here got here by
    /// `surface_address`, so a surface in this space is always there whole.
    pub(crate) fn own_space(&mut self) -> Result<SpaceId, BackendError> {
        if let Some(space) = self.own_space {
            return Ok(space);
        }
        let space = self.new_space()?;
        self.own_space = Some(space);
        Ok(space)
    }

    pub(crate) fn fence_value(&self, fence: u64) -> u64 {
        read_fence(&self.memory, fence)
    }

    /// Sets a fence that's already in use. Those are all in the window, so this can only fail if
    /// the window has gone, and then the guest has nothing left to wait with.
    pub(crate) fn store_fence(&mut self, fence: u64, value: u64) {
        if let Err(error) = self.memory.write_u64(fence, value) {
            log::error!("setting a fence: {error}");
        }
    }
}

/// A fence's value. One that can't be read counts as not reached, as work that's thought to be
/// finished has its memory taken away.
fn read_fence<M: GuestMemory>(memory: &M, fence: u64) -> u64 {
    memory.read_u64(fence).unwrap_or_else(|error| {
        log::error!("reading a fence: {error}");
        0
    })
}

/// The handler bodies borrow these fields apart, so a backend call can use the rest of the session.
pub(crate) struct Services<'a, M> {
    gpu_addresses: &'a mut GpuAddresses,
    allocations: &'a HashMap<u32, Allocation>,
    memory: &'a mut M,
}

impl<M: GuestMemory> Services<'_, M> {
    /// The allocation in shared pages whose mapping holds this address, and the mapping.
    fn shared_mapping(&self, space: SpaceId, address: u64) -> Option<(u64, &Mapping)> {
        self.allocations.values().find_map(|allocation| {
            let base = allocation.guest_address?;
            let mut mappings = allocation.mappings.iter();
            Some((base, mappings.find(|mapping| mapping.space == space && mapping.contains(address))?))
        })
    }
}

/// What backs an allocation the guest can reach.
#[derive(Clone, Copy)]
enum GuestBacking {
    /// In the shared window, from this address.
    Shared(u64),
    /// The guest's own pages.
    Pages(PagesView),
}

impl<M: GuestMemory> Services<'_, M> {
    /// The guest memory behind a GPU address, with the mapping that holds it.
    fn guest_mapping(&self, space: SpaceId, address: u64) -> Option<(GuestBacking, &Mapping)> {
        self.allocations.values().find_map(|allocation| {
            let backing = match (allocation.guest_address, allocation.view) {
                (Some(base), _) => GuestBacking::Shared(base),
                (None, Some(view)) => GuestBacking::Pages(view),
                (None, None) => return None,
            };
            let mut mappings = allocation.mappings.iter();
            Some((backing, mappings.find(|mapping| mapping.space == space && mapping.contains(address))?))
        })
    }
}

impl<M: GuestMemory> HostServices for Services<'_, M> {
    fn take_gpu_addresses(&mut self, size: u64, align: u64) -> Result<u64, BackendError> {
        self.gpu_addresses.take(size, align, AddressUse::Host)
    }

    fn read_at_gpu_address(&self, space: SpaceId, address: u64, out: &mut [u8]) -> Option<usize> {
        let span = self.shared_span(space, address)?;
        Some(self.memory.read_bytes(span.guest_address, out))
    }

    fn shared_span(&self, space: SpaceId, address: u64) -> Option<SharedSpan> {
        let (base, mapping) = self.shared_mapping(space, address)?;
        let offset_in_mapping = address - mapping.address;
        Some(SharedSpan {
            guest_address: base.checked_add(mapping.first)?.checked_add(offset_in_mapping)?,
            offset_in_mapping,
            mapping_length: mapping.length,
        })
    }

    fn guest_span(&self, space: SpaceId, address: u64) -> Option<GuestSpan> {
        let (_, mapping) = self.guest_mapping(space, address)?;
        Some(GuestSpan { offset_in_mapping: address - mapping.address, mapping_length: mapping.length })
    }

    fn read_guest_span(&self, space: SpaceId, address: u64, out: &mut [u8]) -> Option<usize> {
        let (backing, mapping) = self.guest_mapping(space, address)?;
        let offset = address - mapping.address;
        let length = (mapping.length - offset).min(out.len() as u64) as usize;
        let at = mapping.first + offset;
        Some(match backing {
            GuestBacking::Shared(base) => self.memory.read_bytes(base + at, &mut out[..length]),
            GuestBacking::Pages(view) => self.memory.read_view(view, at, &mut out[..length]),
        })
    }

    fn write_guest_span(&mut self, space: SpaceId, address: u64, value: u32) -> bool {
        let Some((backing, mapping)) = self.guest_mapping(space, address) else { return false };
        let at = mapping.first + address - mapping.address;
        match backing {
            GuestBacking::Shared(base) => self.memory.write_u32(base + at, value).is_ok(),
            GuestBacking::Pages(view) => {
                self.memory.write_view_u32(view, at, value);
                true
            }
        }
    }

    fn write_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryError> {
        self.memory.write_u32(address, value)
    }

    fn write_u64(&mut self, address: u64, value: u64) -> Result<(), MemoryError> {
        self.memory.write_u64(address, value)
    }

    fn read_guest(&self, address: u64, out: &mut [u8]) -> usize {
        self.memory.read_bytes(address, out)
    }

    fn fence_value(&self, fence: u64) -> Result<u64, MemoryError> {
        self.memory.read_u64(fence)
    }
}

macro_rules! services {
    ($session:expr) => {
        $crate::session::Services::new(
            &mut $session.gpu_addresses,
            &$session.allocations,
            &mut $session.memory,
        )
    };
}
pub(crate) use services;

impl<'a, M> Services<'a, M> {
    pub(crate) fn new(
        gpu_addresses: &'a mut GpuAddresses,
        allocations: &'a HashMap<u32, Allocation>,
        memory: &'a mut M,
    ) -> Self {
        Self { gpu_addresses, allocations, memory }
    }
}

pub(crate) fn answer_status(status: Status, out: &mut Vec<u8>) {
    put_u32(out, status.0);
}

/// An entry count, then a status. A bare status here would be read as the count.
fn answer_no_registry_keys(out: &mut Vec<u8>) {
    put_u32(out, 0);
    put_u32(out, Status::SUCCESS.0);
}

/// The first bytes of a reply in hex, for the log, written without making a string.
struct ReplyStart<'a>(&'a [u8]);

impl std::fmt::Display for ReplyStart<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const SHOWN: usize = 16;
        self.0.iter().take(SHOWN).try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}
