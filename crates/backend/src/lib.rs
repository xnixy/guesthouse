//! The line between the vendor-neutral daemon and a GPU vendor.
//!
//! A vendor backend implements [`Backend`] and sees nothing of the core: it gets plain values, and
//! asks for what it needs through [`HostServices`]. The core sees no vendor data either, as private
//! bytes pass through it unread, except the record in `guesthouse_messages::standard`, which is ours.
//!
//! Architectures and driver branches are a backend's own business. [`Backend::open`] is where it
//! picks its profiles, and nothing in this trait is per-architecture or per-card.

mod driver;
mod memory;
mod pci_address;
mod types;

pub use driver::{
    CardLabel, DriverFound, DriverSurvey, DriverVersion, Setup, StartDecision, decide_start,
    driver_version_from_inf,
};
pub use guesthouse_messages::Status;
pub use guesthouse_messages::registry::RegistryName;
pub use memory::{GuestMemory, MemoryError, PagesView, WINDOW_BASE, in_window};
pub use pci_address::PciAddress;
pub use types::*;

use std::os::fd::OwnedFd;
use std::time::Instant;

/// Everything a GPU vendor must provide.
///
/// Calls come from one thread, and the methods after [`Backend::facts`] run on the message path, so
/// they should not block. A method that can fail returns [`BackendError`], and the guest then gets a
/// plain failure for that message.
pub trait Backend: Sized {
    /// Whether this backend drives a card, judged by its architecture. There is no list of cards
    /// here: the name, memory and engines of a card are read from it in [`Backend::open`].
    fn supports(card: &PciCard) -> bool;

    /// Looks at what is installed, without sending any request to the host's driver. Reading a
    /// version from sysfs is fine, and opening the device is not. [`decide_start`] then judges the
    /// answer, so every vendor refuses for the same reasons and in the same words.
    fn survey(setup: &Setup) -> Result<DriverSurvey, Refusal>;

    /// Opens the card. Runs only after [`decide_start`] allowed it, and picks the architecture and
    /// driver branch profiles from `survey`. A backend may still refuse here, with a plain reason.
    fn open(setup: &Setup, survey: &DriverSurvey) -> Result<Self, Refusal>;

    /// What the card and its driver turned out to be.
    fn facts(&self) -> &AdapterFacts;

    /// Makes video memory of at least `size` bytes, laid out as `kind` says. Returns it and the
    /// size it really has.
    fn allocate_video_memory(&mut self, size: u64, kind: MemoryKind)
    -> Result<(MemoryId, u64), BackendError>;

    /// Lets the GPU reach host memory that is already mapped in this process.
    fn adopt_host_pages(&mut self, host_address: u64, size: u64) -> Result<MemoryId, BackendError>;

    fn release_memory(&mut self, memory: MemoryId);

    /// Copies what the GPU may have written from one block to another, when an allocation moves.
    /// This one may wait a moment, as the guest can't go on until it is done, and nothing else
    /// needing the copy engine is running.
    fn preserve_contents(
        &mut self,
        host: &mut dyn HostServices,
        own_space: SpaceId,
        from: MemoryId,
        to: MemoryId,
        size: u64,
    ) -> Result<(), BackendError>;

    /// Reads the first bytes of memory the GPU may have written, for telling one buffer from
    /// another. It may wait a moment, like [`Backend::preserve_contents`], and nothing else needing
    /// the copy engine is running. At most one small page.
    fn read_memory_start(
        &mut self,
        _host: &mut dyn HostServices,
        _own_space: SpaceId,
        _memory: MemoryId,
        _out: &mut [u8],
    ) -> Result<(), BackendError> {
        Err(BackendError::new("this backend can't read video memory"))
    }

    /// Makes a GPU address space covering `ranges`.
    fn create_address_space(&mut self, ranges: &[GpuRange]) -> Result<SpaceId, BackendError>;

    fn destroy_address_space(&mut self, space: SpaceId);

    /// Maps memory. Must fail if the addresses are taken: the core then clears them and tries again.
    fn map(&mut self, request: &MapRequest) -> Result<(), BackendError>;

    fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError>;

    /// Reads a vendor driver's private data for a new allocation.
    fn describe_allocation(&mut self, private: &[u8]) -> AllocationShape;

    /// Whether the vendor's kernel driver would take an allocation's private data. If any allocation
    /// in a call wouldn't be taken, the whole call fails, as nothing has been made yet.
    fn allocation_data_valid(&mut self, _private: &[u8]) -> bool {
        true
    }

    /// Fills in what the vendor's kernel driver would write back into an allocation's private data.
    /// `size` is what [`AllocationShape::size`] said.
    fn finish_allocation_data(&mut self, private: &mut [u8], size: u64);

    /// Fills in the vendor's part of a new hardware queue. `serial` counts queues from one.
    fn finish_queue_data(&mut self, queue: u32, private: &mut [u8], serial: u32) -> Result<(), BackendError>;

    /// Answers a driver escape in place. The buffer is the guest's data and goes back to it.
    /// GPU addresses the host must never choose, in every address space, because the vendor's
    /// driver has told the guest they are its own to use. The guest can still map there.
    fn kept_gpu_ranges(&self) -> &'static [GpuRange] {
        &[]
    }

    /// The guest has restarted, with its bus reset. Most of what a backend holds goes with its
    /// messages, so this is only for what has to follow the guest's own start.
    fn guest_restarted(&mut self) {}

    /// `memory` adds up the guest's allocations and `processes` sorts them by owner, so both are
    /// only for the escapes that ask.
    fn escape(&mut self, process: u32, data: &mut [u8], books: &GuestBooks<'_>);

    /// Answers an adapter query the vendor owns by appending the answer to `out`. Returns `None` for
    /// a query it leaves to the core.
    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status>;

    /// A value from the adapter's registry key, or one of the driver paths.
    fn registry_value(&mut self, name: &RegistryName) -> Option<RegistryValue>;

    /// The GPU's clock, in nanoseconds.
    fn gpu_clock(&mut self) -> Result<u64, BackendError>;

    /// Hands out memory as a file another process can use where it lies, for frames that stay in
    /// video memory. `size` is what [`Backend::allocate_video_memory`] said it really has.
    fn export_memory(&mut self, _memory: MemoryId, _size: u64) -> Result<OwnedFd, BackendError> {
        Err(BackendError::new("this backend can't export memory yet"))
    }

    /// Whether a context on this engine node may be made. `client_hint` is what the guest says the
    /// context is for. Work for a node with no channel has nowhere to run, and sent to the wrong
    /// engine it would be a fault on a card the host is using.
    fn context_allowed(&mut self, node: u32, client_hint: u32) -> bool;

    /// A context has been made for this process.
    fn context_made(&mut self, _process: u32, _node: u32) {}

    /// A context has gone.
    fn context_gone(&mut self, _process: u32, _node: u32) {}

    /// A guest process has ended, however it ended. What the backend holds for it must go.
    fn process_gone(&mut self, _process: u32) {}

    /// The guest has restarted, and its old processes will never say they ended.
    fn all_processes_gone(&mut self) {}

    /// Whether the vendor's kernel driver would take a new context's private data from this process.
    /// A context it wouldn't take fails the same way as one on a node with no channel.
    fn context_data_valid(&mut self, _process: u32, _node: u32, _private: &[u8]) -> bool {
        true
    }

    /// A last look at a new context's private data, which goes back to the guest. Most need nothing.
    fn finish_context_data(&mut self, _node: u32, _private: &mut [u8]) {}

    /// How much of a queue may be unfinished before the guest's next submission is held back. In the
    /// vendor's own units, the ones [`WorkStart::Started`] counts in. `None` means no limit.
    fn unfinished_work_limit(&self) -> Option<u64> {
        None
    }

    /// Prepares work without letting it run. [`WorkStart::Finished`] says nothing reached the GPU,
    /// and the backend has done what the guest would otherwise wait for, except the queue's fence.
    fn start_work(&mut self, host: &mut dyn HostServices, work: &Work<'_>) -> WorkStart;

    /// Lets prepared work run. The core delays this while a queue waits on a fence. A backend whose
    /// submission is a call that can fail says so here: nothing then runs, the core stands in for the
    /// GPU at the queue's fence, and the queue stops as if [`WorkStart::QueueFailed`] had come back.
    fn ring_doorbell(&mut self, queue: u32, doorbell: u64) -> Result<(), BackendError>;

    /// A hardware queue has gone, so whatever backs it can go too.
    fn close_queue(&mut self, queue: u32);

    /// Starts copying a finished frame into a staging surface. This must not wait for the copy,
    /// as the message thread serves every other guest thread meanwhile.
    fn start_staging_copy(&mut self, host: &mut dyn HostServices, copy: &StagingCopy) -> StagingStart;

    /// Looks at a copy that [`StagingStart::Running`] said was under way. `None` means not yet.
    fn poll_staging_copy(&mut self, host: &mut dyn HostServices, now: Instant) -> Option<Status>;

    /// Starts copying a whole picture into a slot of the frame ring. It runs when no other copy is
    /// under way, and a backend that can't show this picture says so with a failure.
    fn start_slot_copy(&mut self, _host: &mut dyn HostServices, _copy: &SlotCopy) -> StagingStart {
        StagingStart::Finished(Status::UNSUCCESSFUL)
    }

    /// How many bytes a pixel of this surface takes in a copy. The check of a copy and the copy itself
    /// both use this, so that they can't disagree. A surface this backend can't place counts as the
    /// usual size: the check still holds, as it measures the same bytes the copy moves.
    fn copy_pixel_bytes(&self, _surface: &SurfaceShape) -> u32 {
        COPY_PIXEL_BYTES
    }

    /// The bytes a tiled surface fills, as this backend's copy engine lays it out, for a copy that
    /// starts at byte `x` of row `y`. `None` if the engine can't be given these numbers at all.
    fn tiled_bytes(&self, _surface: &SurfaceShape, _x: u64, _y: u64) -> Option<u64> {
        None
    }
}

/// What a backend may ask of the host while it works.
pub trait HostServices {
    /// Takes GPU addresses from the pool guest allocations use, as both share every address space.
    fn take_gpu_addresses(&mut self, size: u64, align: u64) -> Result<u64, BackendError>;

    /// Reads guest-visible memory by GPU address, in one address space, as two processes can use the
    /// same address for different memory. Returns how many bytes it could, as this stops at
    /// the end of the window, or `None` if the address is not in shared pages.
    fn read_at_gpu_address(&self, space: SpaceId, address: u64, out: &mut [u8]) -> Option<usize>;

    /// Where a GPU address in this space is in shared pages.
    fn shared_span(&self, space: SpaceId, address: u64) -> Option<SharedSpan>;

    /// Where a GPU address in this space is in any memory the guest can reach: shared pages, or
    /// its own pages lent to an allocation.
    fn guest_span(&self, space: SpaceId, address: u64) -> Option<GuestSpan>;

    /// Reads such memory by GPU address, up to the end of the mapping. Returns how many bytes.
    fn read_guest_span(&self, space: SpaceId, address: u64, out: &mut [u8]) -> Option<usize>;

    /// Writes a 32-bit word to such memory, where the GPU may be reading. Says if it found any.
    fn write_guest_span(&mut self, space: SpaceId, address: u64, value: u32) -> bool;

    /// Writes a 32-bit word by guest physical address, where the GPU may be reading.
    fn write_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryError>;

    /// Writes a 64-bit word by guest physical address, where the GPU may be reading.
    fn write_u64(&mut self, address: u64, value: u64) -> Result<(), MemoryError>;

    /// Reads by guest physical address.
    fn read_guest(&self, address: u64, out: &mut [u8]) -> usize;

    /// A fence's value, by its guest physical address.
    fn fence_value(&self, fence: u64) -> Result<u64, MemoryError>;
}
