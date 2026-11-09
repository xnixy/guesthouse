//! The AMD backend's host-execution side.
//!
//! This is the half of AMD support that is open and testable today: turning a guest's GPU work into
//! real work on the host's AMD card through the open `amdgpu` kernel driver's DRM ioctls. It opens
//! the render node, reads the card's facts, makes and maps buffers, and submits a command buffer
//! that completes on an eventfd. None of it needs a Windows guest or any closed AMD code.
//!
//! The other half, the guest-protocol side, is the private WDDM blobs the guest's driver sends and
//! expects back. That is not here. The trait methods that serve it ([`Backend::describe_allocation`],
//! [`Backend::finish_allocation_data`], [`Backend::escape`], the staging copies and so on) are
//! placeholders, marked below, waiting on the protocol layouts.
//!
//! Where the backend trait is shaped for NVIDIA and does not fit amdgpu, the mismatch is called out
//! at the method. The chief one is submission: amdgpu's `DRM_AMDGPU_CS` is a blocking ioctl whose
//! completion arrives on an event source, not a doorbell the trait rings. The real submission path
//! lives in [`submit`] and is proven by the `amd-probe` binary, not by [`Backend::start_work`].

mod device;
mod info;
mod ioctl;
mod memory;
pub mod protocol;
mod submit;

pub use device::{Device, Mapping};
pub use info::{
    CardDetails, DeviceInfo, EngineSet, Firmware, HW_IP_DMA, HW_IP_GFX, read_device_info, read_gpu_clock,
};
pub use ioctl::AMD_VENDOR;
pub use memory::{
    CpuAccess, DOMAIN_GTT, DOMAIN_VRAM, GemHandle, MapFlags, MemoryType, set_tiling as set_buffer_tiling,
    tiling as buffer_tiling,
};
pub use submit::{Context, Syncobj, sdma_write_ib, submit, wait_eventfd};

// The buffer calls under harness-facing names, so a proof binary can drive the card without the
// trait. They are the same functions the trait methods use.
pub use memory::{
    adopt_userptr as adopt_buffer, close as close_buffer, create as create_buffer, export as export_buffer,
    map as map_buffer, unmap as unmap_buffer,
};

use guesthouse_backend::{
    AdapterFacts, AllocationShape, Backend, BackendError, CardLabel, DriverFound, DriverSurvey, GpuRange,
    GuestBooks, HostServices, MapRequest, MemoryId, MemoryKind, PciCard, Refusal, RegistryName,
    RegistryValue, Setup, SpaceId, StagingCopy, StagingStart, Status, UnmapRequest, VendorQuery, Work,
    WorkStart,
};
use std::collections::{BTreeMap, HashMap};
use std::os::fd::OwnedFd;
use std::time::Instant;

/// How this backend names the host driver's branch. There is no Windows-style branch here: the host
/// side is the Linux kernel's amdgpu.
const HOST_BRANCH: &str = "amdgpu";

/// One buffer object the backend is holding, and where it sits in the GPU's address space.
#[derive(Debug)]
struct Bo {
    handle: GemHandle,
    /// The address it was last mapped at, so release can take it back. One mapping is tracked, which
    /// is all the allocate-then-map path needs.
    mapped: Option<(SpaceId, u64, u64)>,
}

/// A GPU address space. On amdgpu every space is backed by the one open render node's VM, so the
/// ranges and mappings are tracked here in software to catch collisions the way the core expects,
/// while the hardware sees a single VM. True per-guest isolation needs one open file per space and a
/// buffer import on map; that is noted for the protocol work.
#[derive(Debug, Default)]
struct Space {
    ranges: Vec<GpuRange>,
    mappings: BTreeMap<u64, u64>,
}

/// See the crate documentation.
pub struct AmdBackend {
    device: Device,
    facts: AdapterFacts,
    next_id: u64,
    memory: HashMap<MemoryId, Bo>,
    spaces: HashMap<SpaceId, Space>,
}

impl std::fmt::Debug for AmdBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmdBackend").field("facts", &self.facts).finish_non_exhaustive()
    }
}

impl AmdBackend {
    /// Opens the card directly, for the standalone harness. `open` is the trait's door; this is the
    /// same work without a survey.
    pub fn open_card(address: Option<&str>) -> Result<Self, Refusal> {
        let device =
            Device::open(address).map_err(|error| Refusal(format!("opening the AMD card: {error}")))?;
        let facts = facts_from(device.info());
        Ok(Self { device, facts, next_id: 1, memory: HashMap::new(), spaces: HashMap::new() })
    }

    /// The full device readings, richer than [`AdapterFacts`], for the harness and the report.
    pub fn device_info(&self) -> &DeviceInfo {
        self.device.info()
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Builds the facts the guest is told from the card's own readings.
fn facts_from(info: &DeviceInfo) -> AdapterFacts {
    AdapterFacts {
        card: PciCard { vendor: AMD_VENDOR, device: info.device_id, revision: info.pci_revision },
        name: format!("AMD GPU {:#06x}", info.device_id),
        video_memory: info.vram_total,
        gtt_memory: info.gtt_total,
        bar_size: info.visible_vram_total,
        node_count: info.engines.count(),
        engines: guesthouse_backend::AdapterEngines {
            graphics: info.engines.graphics,
            compute: info.engines.compute,
            copy: info.engines.dma,
            video: false,
        },
        guest_driver_version: 0,
    }
}

impl Backend for AmdBackend {
    fn supports(card: &PciCard) -> bool {
        card.vendor == AMD_VENDOR
    }

    fn survey(setup: &Setup) -> Result<DriverSurvey, Refusal> {
        // The host side is the Linux amdgpu driver; the guest side will be AMD's Adrenalin. Their
        // branches can never match the way two NVIDIA drivers do, so `decide_start`'s branch rule
        // does not fit AMD and will need a tested-pair table (noted in the plan). For the skeleton,
        // the survey reports what it can without opening the card.
        let host = DriverFound { version: "kernel".into(), branch: HOST_BRANCH.into() };
        let guest = DriverFound {
            version: setup.guest_driver_version.to_string(),
            branch: setup.guest_branch.clone(),
        };
        Ok(DriverSurvey {
            host,
            guest,
            host_branch_known: true,
            guest_branch_known: false,
            host_version_tested: false,
            mixed_branches_tested: false,
            card: CardLabel::Experimental,
        })
    }

    fn open(setup: &Setup, _survey: &DriverSurvey) -> Result<Self, Refusal> {
        let address = setup.card_address.as_ref().map(|address| address.to_string());
        let mut backend = Self::open_card(address.as_deref())?;
        backend.facts.guest_driver_version = setup.guest_driver_version.packed();
        Ok(backend)
    }

    fn facts(&self) -> &AdapterFacts {
        &self.facts
    }

    /// The kind is the swizzle mode the guest's surface descriptor gave, so a tiled surface gets a
    /// buffer that carries the same tiling for whoever imports it.
    fn allocate_video_memory(
        &mut self,
        size: u64,
        kind: MemoryKind,
    ) -> Result<(MemoryId, u64), BackendError> {
        let buffer = protocol::tiling::host_buffer(kind.0, self.device.info().details.gfx_version);
        let alignment = self.device.info().va_alignment;
        let handle = memory::create(&self.device, size, alignment, buffer.domain, buffer.cpu_access)
            .map_err(|error| BackendError::new(format!("GEM_CREATE: {error}")))?;
        if buffer.tiling_info != 0
            && let Err(error) = memory::set_tiling(&self.device, handle, buffer.tiling_info)
        {
            memory::close(&self.device, handle);
            return Err(BackendError::new(format!("GEM_METADATA: {error}")));
        }
        let id = MemoryId(self.new_id());
        self.memory.insert(id, Bo { handle, mapped: None });
        Ok((id, size))
    }

    fn adopt_host_pages(&mut self, host_address: u64, size: u64) -> Result<MemoryId, BackendError> {
        let handle = memory::adopt_userptr(&self.device, host_address, size, false)
            .map_err(|error| BackendError::new(format!("GEM_USERPTR: {error}")))?;
        let id = MemoryId(self.new_id());
        self.memory.insert(id, Bo { handle, mapped: None });
        Ok(id)
    }

    fn release_memory(&mut self, memory: MemoryId) {
        if let Some(bo) = self.memory.remove(&memory) {
            if let Some((space, address, size)) = bo.mapped {
                let _ = memory::unmap(&self.device, bo.handle, address, size);
                if let Some(space) = self.spaces.get_mut(&space) {
                    space.mappings.remove(&address);
                }
            }
            memory::close(&self.device, bo.handle);
        }
    }

    fn preserve_contents(
        &mut self,
        _host: &mut dyn HostServices,
        _own_space: SpaceId,
        _from: MemoryId,
        _to: MemoryId,
        _size: u64,
    ) -> Result<(), BackendError> {
        // A move would be an SDMA linear copy between the two buffers, waited on like a submission.
        // Not needed to prove the host path, so left for the submission work that follows.
        Err(BackendError::new("AMD backend does not move allocations yet"))
    }

    fn create_address_space(&mut self, ranges: &[GpuRange]) -> Result<SpaceId, BackendError> {
        let space = SpaceId(self.new_id());
        self.spaces.insert(space, Space { ranges: ranges.to_vec(), mappings: BTreeMap::new() });
        Ok(space)
    }

    fn destroy_address_space(&mut self, space: SpaceId) {
        self.spaces.remove(&space);
    }

    fn map(&mut self, request: &MapRequest) -> Result<(), BackendError> {
        let Some(space) = self.spaces.get_mut(&request.space) else {
            return Err(BackendError::new("this address space is closed"));
        };
        if !space.ranges.iter().any(|range| range.contains(request.address)) {
            return Err(BackendError::new("no range holds this address"));
        }
        let overlaps = {
            let before = space.mappings.range(..request.address.saturating_add(request.length)).next_back();
            before.is_some_and(|(&start, &size)| start.saturating_add(size) > request.address)
        };
        if overlaps {
            return Err(BackendError::new("these addresses are taken"));
        }
        let Some(bo) = self.memory.get_mut(&request.memory) else {
            return Err(BackendError::new("mapping memory that does not exist"));
        };
        // Honour the request's read/write/execute and memory type, which the core now carries. When
        // the memory type is left to the backend, the older gpu_cached hint still decides caching.
        let memory_type = match request.memory_type {
            guesthouse_backend::MapMemoryType::Default => {
                if request.gpu_cached {
                    MemoryType::NonCoherent
                } else {
                    MemoryType::Uncached
                }
            }
            guesthouse_backend::MapMemoryType::NonCoherent => MemoryType::NonCoherent,
            guesthouse_backend::MapMemoryType::Coherent => MemoryType::CacheCoherent,
            guesthouse_backend::MapMemoryType::Uncached => MemoryType::Uncached,
        };
        let flags = MapFlags {
            readable: request.readable,
            writable: request.writable,
            executable: request.executable,
            memory_type,
        };
        memory::map(&self.device, bo.handle, request.address, request.offset, request.length, flags)
            .map_err(|error| BackendError::new(format!("GEM_VA map: {error}")))?;
        space.mappings.insert(request.address, request.length);
        bo.mapped = Some((request.space, request.address, request.length));
        Ok(())
    }

    fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError> {
        let Some(bo) = self.memory.get_mut(&unmap.memory) else {
            return Err(BackendError::new("unmapping memory that does not exist"));
        };
        memory::unmap(&self.device, bo.handle, unmap.address, unmap.length)
            .map_err(|error| BackendError::new(format!("GEM_VA unmap: {error}")))?;
        if let Some(space) = self.spaces.get_mut(&unmap.space) {
            space.mappings.remove(&unmap.address);
        }
        bo.mapped = None;
        Ok(())
    }

    fn describe_allocation(&mut self, private: &[u8]) -> AllocationShape {
        protocol::allocation::shape(private)
    }

    fn allocation_data_valid(&mut self, private: &[u8]) -> bool {
        match protocol::allocation::parse(private) {
            Ok(_) => true,
            Err(refused) => {
                log::info!("refused an allocation: {}", refused.0);
                false
            }
        }
    }

    /// The kernel driver writes nothing back into private data the guest built.
    fn finish_allocation_data(&mut self, _private: &mut [u8], _size: u64) {}

    fn finish_queue_data(
        &mut self,
        _queue: u32,
        private: &mut [u8],
        _serial: u32,
    ) -> Result<(), BackendError> {
        protocol::hw_queue::check(private).map_err(|refused| BackendError::new(refused.0))
    }

    fn escape(&mut self, _process: u32, _data: &mut [u8], _books: &GuestBooks<'_>) {
        // AMD's escapes carry a subfunction (ATI_LHESCAPE_HDR) and expect a status; the trait's
        // escape hook has neither yet. This is guest-protocol work.
    }

    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
        protocol::adapter_info::answer(query, self.device.info(), out)
    }

    fn registry_value(&mut self, _name: &RegistryName) -> Option<RegistryValue> {
        None
    }

    fn gpu_clock(&mut self) -> Result<u64, BackendError> {
        self.device.gpu_clock().map_err(|error| BackendError::new(format!("timestamp: {error}")))
    }

    fn export_memory(&mut self, memory: MemoryId, _size: u64) -> Result<OwnedFd, BackendError> {
        let bo = self
            .memory
            .get(&memory)
            .ok_or_else(|| BackendError::new("exporting memory that does not exist"))?;
        memory::export(&self.device, bo.handle)
            .map_err(|error| BackendError::new(format!("PRIME export: {error}")))
    }

    fn context_allowed(&mut self, node: u32, _client_hint: u32) -> bool {
        node < self.facts.node_count
    }

    /// The kernel driver writes nothing back into a context's private data, so it is only checked.
    fn context_data_valid(&mut self, _process: u32, node: u32, private: &[u8]) -> bool {
        match protocol::context::check(private) {
            Ok(_) => true,
            Err(refused) => {
                log::info!("refused a context on node {node}: {}", refused.0);
                false
            }
        }
    }

    fn start_work(&mut self, _host: &mut dyn HostServices, work: &Work<'_>) -> WorkStart {
        // The kernel driver fails a submission whose private data is wrong, and the guest then
        // treats the device as lost, so the queue stops here the same way.
        if let Err(refused) = protocol::command_data::check(work.private) {
            log::warn!("refused a submission on queue {:#x}: {}", work.queue, refused.0);
            return WorkStart::QueueFailed;
        }
        // amdgpu submission is a blocking ioctl that signals a sync object, not a doorbell the core
        // rings, and completion arrives on an eventfd, not in fence memory. The trait has no event
        // source, so the real path is `submit::submit` plus `Syncobj::arm_eventfd`, proven by the
        // `amd-probe` binary. Part B adds the completion-event variant this would return.
        WorkStart::Finished
    }

    fn ring_doorbell(&mut self, _queue: u32, _doorbell: u64) {}

    fn close_queue(&mut self, _queue: u32) {}

    fn start_staging_copy(&mut self, _host: &mut dyn HostServices, _copy: &StagingCopy) -> StagingStart {
        // The copy-to-screen path wants the tiling hooks and a BAR-size-aware route; not part of the
        // host-execution proof.
        StagingStart::Finished(Status::UNSUCCESSFUL)
    }

    fn poll_staging_copy(&mut self, _host: &mut dyn HostServices, _now: Instant) -> Option<Status> {
        Some(Status::UNSUCCESSFUL)
    }
}
