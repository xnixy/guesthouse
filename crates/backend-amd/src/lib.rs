//! The AMD backend: the host's open amdgpu driver, and the private data AMD's Windows driver passes
//! to it.
//!
//! The host-execution side turns a guest's GPU work into real work on the host's AMD card through
//! amdgpu's DRM ioctls. It opens the render node, reads the card's facts, makes and maps buffers,
//! and submits a command buffer that completes on an eventfd.
//!
//! The guest-protocol side, in [`protocol`], is the private data in the WDDM calls the guest's
//! driver makes: it answers the adapter block, checks allocations, contexts, queues, submissions and
//! escapes the way the kernel driver would, and tiles a surface's host buffer the way the guest laid
//! it out. It is plain byte work, tested without a guest.
//!
//! Where the backend trait is shaped for NVIDIA and does not fit amdgpu, the mismatch is called out
//! at the method. The chief one is submission: amdgpu's `DRM_AMDGPU_CS` is a blocking ioctl whose
//! completion arrives on an event source, not a doorbell the trait rings. The real submission path
//! lives in [`submit`] and is proven by the `amd-probe` binary, not by [`Backend::start_work`],
//! which only checks the guest's private data for now.

mod channel_order;
mod copy_ib;
mod device;
mod fault;
mod info;
mod ioctl;
mod memory;
mod moves;
mod nodes;
pub mod protocol;
mod run_wait;
mod screen_copy;
mod sdma_copy;
mod space_vm;
mod submit;

pub use device::{Device, Mapping};
pub use info::{
    CardDetails, DeviceInfo, EngineSet, Firmware, HW_IP_DMA, HW_IP_GFX, read_device_info, read_gpu_clock,
};
pub use ioctl::{AMD_VENDOR, amd_render_node};
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

use channel_order::WrittenRows;
use copy_ib::CopyIb;
use guesthouse_backend::{
    AdapterFacts, AllocationShape, Backend, BackendError, CardLabel, DriverFound, DriverSurvey, GpuRange,
    GuestBooks, HostServices, MapRequest, MemoryId, MemoryKind, PciCard, Refusal, RegistryName,
    RegistryValue, Setup, SlotCopy, SpaceId, StagingCopy, StagingStart, StandardSurface, Status,
    SurfaceShape, UnmapRequest, VendorQuery, Work, WorkStart,
};
use nodes::nodes;
use run_wait::Patience;
use screen_copy::{CopyPlan, PIXEL_BYTES};
use sdma_copy::{LinearCopy, TiledCopy};
use space_vm::{Origin, Ran, SpaceVm};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::OwnedFd;
use std::time::Instant;
use submit::Ib;

/// How this backend names the host driver's branch. There is no Windows-style branch here: the host
/// side is the Linux kernel's amdgpu.
const HOST_BRANCH: &str = "amdgpu";
/// The CPU maps whole pages.
const PAGE_BYTES: usize = 4096;

/// One buffer object the backend is holding, on its first open. Where it is mapped is in each
/// space's [`SpaceVm`].
#[derive(Debug, Clone, Copy)]
struct Bo {
    handle: GemHandle,
    origin: Origin,
}

/// A GPU address space, with a VM of its own on amdgpu. The ranges and mappings are kept here too,
/// to refuse what the core shouldn't ask for before the kernel is asked.
#[derive(Debug)]
struct Space {
    ranges: Vec<GpuRange>,
    mappings: BTreeMap<u64, u64>,
    vm: SpaceVm,
}

/// See the crate documentation.
pub struct AmdBackend {
    device: Device,
    facts: AdapterFacts,
    next_id: u64,
    memory: HashMap<MemoryId, Bo>,
    spaces: HashMap<SpaceId, Space>,
    /// CPU mappings of buffers made on the card, kept while the buffer lives, for the screen copy.
    cpu_views: HashMap<GemHandle, Mapping>,
    /// The DMA engine's commands for copies of tiled surfaces, made at the first.
    copy_ib: Option<CopyIb>,
    /// How long the next run is waited for, which is less after one that hung.
    patience: Patience,
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
        Ok(Self {
            device,
            facts,
            next_id: 1,
            memory: HashMap::new(),
            spaces: HashMap::new(),
            cpu_views: HashMap::new(),
            copy_ib: None,
            patience: Patience::RUN,
        })
    }

    /// The full device readings, richer than [`AdapterFacts`], for the harness and the report.
    pub fn device_info(&self) -> &DeviceInfo {
        self.device.info()
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Copies a frame with the CPU. The work that drew it has retired, as a submission is waited for.
    fn copy_with_cpu(&mut self, copy: &StagingCopy) -> Result<(), String> {
        let plan = CopyPlan::of(copy).ok_or("a tiled surface, or a rectangle past a surface")?;
        let source = self.cpu_address(copy.own_space, copy.source_address, plan.source_end())?;
        let target = self.cpu_address(copy.own_space, copy.target_address, plan.target_end())?;
        // SAFETY: each address is good for the bytes asked of `cpu_address`, which are the plan's
        // ends, and the two surfaces are different allocations.
        unsafe { plan.run(source, target) };
        Ok(())
    }

    /// Copies a tiled frame with the DMA engine, which reads tiles as the CPU can't, and waits for it.
    fn copy_with_dma(&mut self, space: SpaceId, plan: &TiledCopy) -> Result<(), String> {
        let target_start =
            (u64::from(plan.to.1) * u64::from(plan.linear_pitch) + u64::from(plan.to.0)) * PIXEL_BYTES;
        let target_bytes =
            (u64::from(plan.height - 1) * u64::from(plan.linear_pitch) + u64::from(plan.width)) * PIXEL_BYTES;
        let reaches = [(plan.tiled_address, PIXEL_BYTES), (plan.linear_address + target_start, target_bytes)];
        self.run_dma(space, &reaches, &plan.commands())
    }

    /// Whether the DMA engine's commands here are written for this card.
    fn dma_known(&self) -> bool {
        matches!(self.device.info().details.gfx_version.0, 10 | 11)
    }

    /// Runs commands on the DMA engine in the daemon's space, and waits for them. `reaches` are the
    /// ranges they touch, each of which must be mapped there.
    fn run_dma(&mut self, space: SpaceId, reaches: &[(u64, u64)], commands: &[u32]) -> Result<(), String> {
        if !self.dma_known() {
            return Err("the DMA copy is written for GFX10 and GFX11".into());
        }
        let alignment = self.device.info().va_alignment;
        if self.copy_ib.is_none() {
            let made = CopyIb::make(&self.device, alignment)
                .map_err(|error| format!("its commands' buffer: {error}"))?;
            self.copy_ib = Some(made);
        }
        let ib = self.copy_ib.as_mut().expect("made above");
        let Some(space_vm) = self.spaces.get_mut(&space).map(|space| &mut space.vm) else {
            return Err("no such space".into());
        };
        if !reaches.iter().all(|&(address, bytes)| space_vm.holds(address, bytes)) {
            return Err("a surface isn't mapped in the daemon's space".into());
        }
        if ib.mapped_in != Some(space) {
            let flags = MapFlags {
                readable: true,
                writable: false,
                executable: true,
                memory_type: MemoryType::Uncached,
            };
            space_vm
                .map(&self.device, ib.handle(), Origin::Made, copy_ib::ADDRESS, 0, ib.size(), flags)
                .map_err(|error| format!("mapping its commands: {error}"))?;
            ib.mapped_in = Some(space);
        }
        let bytes = ib.write(commands).map_err(|error| error.to_string())?;
        let ran = space_vm.run(HW_IP_DMA, &[Ib { address: copy_ib::ADDRESS, bytes }], self.patience);
        self.note_how_it_ran(&ran);
        match ran {
            Ok(Ran::Retired) => Ok(()),
            Ok(Ran::Refused) => Err("amdgpu kept saying try again".into()),
            Ok(Ran::Hung) => Err("it never finished".into()),
            Ok(Ran::Faulted(fault)) => Err(format!(
                "the daemon's own work on the card reached {:#x}, where nothing is mapped (fault status \
                 {:#x}), so it sends the card no more",
                fault.address, fault.status
            )),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Keeps the patience for the next run in step with how this one ended.
    fn note_how_it_ran(&mut self, ran: &io::Result<Ran>) {
        match ran {
            Ok(Ran::Retired) => self.patience = Patience::RUN,
            Ok(Ran::Hung) => self.patience = Patience::AFTER_A_HANG,
            Ok(Ran::Refused | Ran::Faulted(_)) | Err(_) => {}
        }
    }

    /// Copies a frame into a viewer's slot. The guest's surface is in video memory, which the CPU
    /// reads slowly, so a run of rows goes by the DMA engine, and anything else the way the screen's
    /// copies go.
    fn copy_to_slot(&mut self, staging: &StagingCopy) -> Result<(), String> {
        let linear = CopyPlan::of(staging)
            .and_then(|plan| LinearCopy::of(&plan, staging.source_address, staging.target_address));
        let Some(linear) = linear else { return self.copy_staging(staging) };
        let reaches = [(linear.source_address, linear.bytes), (linear.target_address, linear.bytes)];
        self.run_dma(staging.own_space, &reaches, &linear.commands())
    }

    /// Copies what `from` holds into `to`, a window at a time, by the DMA engine. The work that wrote
    /// it has retired, as every submission is waited for.
    fn move_with_dma(&mut self, space: SpaceId, from: Bo, to: Bo, size: u64) -> Result<(), String> {
        let flags =
            MapFlags { readable: true, writable: true, executable: false, memory_type: MemoryType::Uncached };
        for piece in moves::pieces(size) {
            let space_vm = &mut self.spaces.get_mut(&space).ok_or("no such space")?.vm;
            let (source, target) = (moves::SOURCE_WINDOW, moves::TARGET_WINDOW);
            space_vm
                .map(&self.device, from.handle, from.origin, source, piece.offset, piece.mapped, flags)
                .map_err(|error| format!("mapping what moves: {error}"))?;
            let copied = space_vm
                .map(&self.device, to.handle, to.origin, target, piece.offset, piece.mapped, flags)
                .map_err(|error| format!("mapping where it goes: {error}"))
                .and_then(|()| {
                    let copy =
                        LinearCopy { source_address: source, target_address: target, bytes: piece.bytes };
                    let reaches = [(source, piece.bytes), (target, piece.bytes)];
                    self.run_dma(space, &reaches, &copy.commands())
                });
            // The windows are one place each, so a mapping left in one would stop every later move.
            let space_vm = &mut self.spaces.get_mut(&space).ok_or("no such space")?.vm;
            for window in [source, target] {
                if let Err(error) = space_vm.unmap(window) {
                    log::warn!("GEM_VA unmap at {window:#x}: {error}");
                }
            }
            copied?;
        }
        Ok(())
    }

    /// Copies what `from` holds into `to` with the CPU, for a card whose DMA commands aren't known.
    fn move_with_cpu(&mut self, from: Bo, to: Bo, size: u64) -> Result<(), String> {
        let source = self.cpu_reach(from.handle, from.origin, 0, size)?;
        let target = self.cpu_reach(to.handle, to.origin, 0, size)?;
        // SAFETY: `cpu_reach` gave addresses good for `size` bytes, in two different buffers.
        unsafe { std::ptr::copy_nonoverlapping(source.cast_const(), target, size as usize) };
        Ok(())
    }

    /// A copy to the screen or a slot: tiled surfaces by the DMA engine, rows by the CPU. Both move
    /// bytes as they are, so a target whose red and blue lie the other way round is put right after.
    fn copy_staging(&mut self, copy: &StagingCopy) -> Result<(), String> {
        match TiledCopy::of(copy, self.device.info().details.gfx_version) {
            Some(plan) => self.copy_with_dma(copy.own_space, &plan)?,
            None => self.copy_with_cpu(copy)?,
        }
        if channel_order::differs(&copy.source, &copy.target) {
            // The picture is there either way, so a target the CPU can't reach isn't a failed copy.
            if let Err(reason) = self.swap_red_and_blue(copy) {
                log::warn!("a copy's red and blue were left changed over: {reason}");
            }
        }
        Ok(())
    }

    /// Changes red and blue over in what a copy wrote. The copy has finished, as each is waited for.
    fn swap_red_and_blue(&mut self, copy: &StagingCopy) -> Result<(), String> {
        let rows = WrittenRows::of(copy).ok_or("a tiled target")?;
        let target = self.cpu_address(copy.own_space, copy.target_address, rows.end())?;
        // SAFETY: `cpu_address` gave an address good for the rows' end.
        unsafe { rows.swap_red_and_blue(target) };
        Ok(())
    }

    /// Where the CPU reaches `bytes` bytes at a GPU address of a space.
    fn cpu_address(&mut self, space: SpaceId, address: u64, bytes: u64) -> Result<*mut u8, String> {
        let space = self.spaces.get(&space).ok_or("no such space")?;
        let (buffer, offset) =
            space.vm.locate(address).ok_or_else(|| format!("nothing mapped at {address:#x}"))?;
        let bo = self.memory.values().find(|bo| bo.handle == buffer).ok_or("the buffer has gone")?;
        let (handle, origin) = (bo.handle, bo.origin);
        self.cpu_reach(handle, origin, offset, bytes)
    }

    /// Where the CPU reaches `bytes` bytes at `offset` into a buffer, mapping it on first use.
    fn cpu_reach(
        &mut self,
        buffer: GemHandle,
        origin: Origin,
        offset: u64,
        bytes: u64,
    ) -> Result<*mut u8, String> {
        let end = offset.checked_add(bytes).ok_or("too long")?;
        if let Origin::HostPages { address: host, size } = origin {
            if end > size {
                return Err(format!("{bytes:#x} bytes at {offset:#x} run past the pages"));
            }
            return Ok((host + offset) as *mut u8);
        }
        let length = usize::try_from(end).map_err(|_| "too long")?.next_multiple_of(PAGE_BYTES);
        if self.cpu_views.get(&buffer).is_none_or(|view| view.size() < length) {
            let view = self
                .device
                .map_cpu(buffer, length)
                .map_err(|error| format!("mapping {buffer:?}: {error}"))?;
            self.cpu_views.insert(buffer, view);
        }
        // SAFETY: the view covers `length` bytes, which is past `offset`.
        Ok(unsafe { self.cpu_views[&buffer].as_ptr().add(offset as usize) })
    }

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Whether every range lies inside the card's usable addresses, `start..end`.
fn ranges_fit(ranges: &[GpuRange], start: u64, end: u64) -> bool {
    ranges.iter().all(|range| range.start >= start && range.end <= end && range.start <= range.end)
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
        // The host is the Linux kernel's amdgpu and the guest is one Adrenalin release. Those are
        // different drivers on purpose, so the pair is allowed through even though the branch names
        // differ. Any other guest release has no profile and is refused before the card is opened.
        let version = setup.guest_driver_version.to_string();
        let matches = protocol::guest_driver::GUEST_RELEASES.contains(&version.as_str());
        let host = DriverFound { version: "kernel".into(), branch: HOST_BRANCH.into() };
        let guest = DriverFound { version, branch: setup.guest_branch.clone() };
        Ok(DriverSurvey {
            host,
            guest,
            host_branch_known: true,
            guest_branch_known: matches,
            host_version_tested: matches,
            mixed_branches_tested: matches,
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
        self.memory.insert(id, Bo { handle, origin: Origin::Made });
        Ok((id, size))
    }

    fn adopt_host_pages(&mut self, host_address: u64, size: u64) -> Result<MemoryId, BackendError> {
        let handle = memory::adopt_userptr(&self.device, host_address, size, false)
            .map_err(|error| BackendError::new(format!("GEM_USERPTR: {error}")))?;
        let id = MemoryId(self.new_id());
        self.memory.insert(id, Bo { handle, origin: Origin::HostPages { address: host_address, size } });
        Ok(id)
    }

    fn release_memory(&mut self, memory: MemoryId) {
        if let Some(bo) = self.memory.remove(&memory) {
            self.cpu_views.remove(&bo.handle);
            for space in self.spaces.values_mut() {
                for address in space.vm.forget(bo.handle) {
                    log::debug!("unmapped {address:#x} as its memory was freed");
                    space.mappings.remove(&address);
                }
            }
            memory::close(&self.device, bo.handle);
        }
    }

    fn preserve_contents(
        &mut self,
        _host: &mut dyn HostServices,
        own_space: SpaceId,
        from: MemoryId,
        to: MemoryId,
        size: u64,
    ) -> Result<(), BackendError> {
        let buffer = |id| self.memory.get(&id).copied().ok_or_else(|| BackendError::new("no such memory"));
        let (from, to) = (buffer(from)?, buffer(to)?);
        // Once the daemon's own space has faulted nothing more is run there, so the CPU moves it.
        let dma_stopped = self.spaces.get(&own_space).is_some_and(|space| space.vm.faulted().is_some());
        let moved = if self.dma_known() && !dma_stopped {
            // A lock that fails is worse for the guest than a slow one, so the CPU has a go when the
            // engine refuses.
            self.move_with_dma(own_space, from, to, size).or_else(|reason| {
                log::warn!("the DMA engine didn't move {size:#x} bytes ({reason}), so the CPU does");
                self.move_with_cpu(from, to, size)
            })
        } else {
            self.move_with_cpu(from, to, size)
        };
        moved.map_err(|reason| BackendError::new(format!("moving {size:#x} bytes: {reason}")))
    }

    /// Linear buffers are made with CPU access, so the CPU reads them, on a discrete card too. A tiled
    /// one starts with a tile rather than its first bytes, which no caller wants.
    fn read_memory_start(
        &mut self,
        _host: &mut dyn HostServices,
        _own_space: SpaceId,
        memory: MemoryId,
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        if out.len() > PAGE_BYTES {
            return Err(BackendError::new("more than a page asked for"));
        }
        let bo = self.memory.get(&memory).ok_or_else(|| BackendError::new("no such memory"))?;
        let (handle, origin) = (bo.handle, bo.origin);
        let start = self.cpu_reach(handle, origin, 0, out.len() as u64).map_err(BackendError::new)?;
        // SAFETY: `cpu_reach` gave an address good for `out.len()` bytes, and the GPU work that wrote
        // them has retired, as the caller waited for its queues.
        unsafe { std::ptr::copy_nonoverlapping(start.cast_const(), out.as_mut_ptr(), out.len()) };
        Ok(())
    }

    /// The core's ranges are its own, not the card's. amdgpu keeps the bottom and top of its address
    /// range, so a space reaching into either is refused here rather than failing at the first map.
    fn create_address_space(&mut self, ranges: &[GpuRange]) -> Result<SpaceId, BackendError> {
        let info = self.device.info();
        if !ranges_fit(ranges, info.va_start, info.va_end) {
            return Err(BackendError::new(format!(
                "these GPU ranges reach past what the card offers, {:#x}..{:#x}",
                info.va_start, info.va_end
            )));
        }
        let space = SpaceId(self.new_id());
        let vm = SpaceVm::open(&self.device)
            .map_err(|error| BackendError::new(format!("opening the card for an address space: {error}")))?;
        self.spaces.insert(space, Space { ranges: ranges.to_vec(), mappings: BTreeMap::new(), vm });
        log::debug!("address space {space:?} opened");
        Ok(space)
    }

    fn destroy_address_space(&mut self, space: SpaceId) {
        // Closing the space's open takes its VM, and all that was mapped in it, with it.
        self.spaces.remove(&space);
        log::debug!("address space {space:?} closed");
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
            // The command processor fetches command buffers through the guest's own mappings, and
            // amdgpu won't let it without this. The guest never asks for it, so whatever it can
            // read, the card may run.
            executable: request.executable || request.readable,
            memory_type,
        };
        let (address, offset, length) = (request.address, request.offset, request.length);
        space
            .vm
            .map(&self.device, bo.handle, bo.origin, address, offset, length, flags)
            .map_err(|error| BackendError::new(format!("GEM_VA map: {error}")))?;
        space.mappings.insert(address, length);
        log::debug!("mapped {address:#x} for {length:#x} in space {:?}", request.space);
        Ok(())
    }

    fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError> {
        if !self.memory.contains_key(&unmap.memory) {
            return Err(BackendError::new("unmapping memory that does not exist"));
        }
        let Some(space) = self.spaces.get_mut(&unmap.space) else {
            return Ok(());
        };
        space.mappings.remove(&unmap.address);
        log::debug!("unmap {:#x} in space {:?}", unmap.address, unmap.space);
        space.vm.unmap(unmap.address).map_err(|error| BackendError::new(format!("GEM_VA unmap: {error}")))
    }

    fn describe_allocation(&mut self, private: &[u8]) -> AllocationShape {
        protocol::allocation::shape(private)
    }

    /// AMD's user-mode driver reads a shared primary's data in its own layout when a fullscreen
    /// program opens it, so these surfaces get the kernel driver's data, not the daemon's record.
    fn standard_allocation_data(&mut self, surface: &StandardSurface) -> Option<Vec<u8>> {
        Some(protocol::standard::data(surface))
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

    /// The answer is in the escape's own header. The trait has no status to return beside it.
    fn escape(&mut self, process: u32, data: &mut [u8], _books: &GuestBooks<'_>) {
        match protocol::escape::answer(data) {
            Ok(answered) if answered.done => log::debug!(
                "escape from {process}: service {} subfunction {:#x}, answered as done",
                answered.code.service,
                answered.code.subfunction
            ),
            Ok(answered) if answered.code.needed_by_the_guest() => log::warn!(
                "escape from {process}: service {} subfunction {:#x}, not shaped as the guest's driver \
                 sends it, answered as failed; the guest can't go on without it",
                answered.code.service,
                answered.code.subfunction
            ),
            Ok(answered) => log::debug!(
                "escape from {process}: service {} subfunction {:#x}, answered as failed",
                answered.code.service,
                answered.code.subfunction
            ),
            Err(refused) => log::info!(
                "an escape from {process} is {} ({} bytes, starting {})",
                refused.0,
                data.len(),
                protocol::bytes::first_dwords(data, 4)
            ),
        }
    }

    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
        if query.kind == guesthouse_backend::guest_text::QUERY_USER_DRIVER_NAME {
            protocol::guest_driver::write_name(query, out);
            return Some(Status::SUCCESS);
        }
        if query.kind == protocol::api_drivers::QUERY_OPENGL {
            protocol::api_drivers::write_opengl_info(query, out);
            return Some(Status::SUCCESS);
        }
        if let Some(status) = protocol::adapter_registry::answer(query, out) {
            return Some(status);
        }
        if let Some(status) = protocol::wddm_caps::answer(query, out) {
            return Some(status);
        }
        if let Some(status) = protocol::gpu_mmu_caps::answer(query, out) {
            return Some(status);
        }
        protocol::adapter_info::answer(query, self.device.info(), out)
    }

    fn registry_value(&mut self, name: &RegistryName) -> Option<RegistryValue> {
        protocol::guest_driver::registry(name)
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
        let Some(node) = nodes(&self.device.info().engines).get(work.node as usize).copied() else {
            log::warn!("work for node {}, which this card hasn't got", work.node);
            return WorkStart::QueueFailed;
        };
        let ibs: Vec<Ib> = protocol::command_data::command_buffers(work.private)
            .into_iter()
            .map(|buffer| Ib { address: buffer.address, bytes: buffer.bytes })
            .collect();
        if ibs.is_empty() || !work.may_run || work.queue_failed {
            return WorkStart::Finished;
        }
        let Some(space) = self.spaces.get_mut(&work.space) else {
            log::warn!("work for a space that isn't here");
            return WorkStart::QueueFailed;
        };
        if let Some(ib) = space.vm.first_unmapped(&ibs) {
            log::warn!(
                "work on engine family {} in space {:?} reads {:#x} for {:#x} bytes, which isn't mapped \
                 there, so it isn't run",
                node.ip_type,
                work.space,
                ib.address,
                ib.bytes
            );
            return WorkStart::QueueFailed;
        }
        log::debug!("run on engine family {} in space {:?}: {:x?}", node.ip_type, work.space, ibs);
        // The run is waited for here, so the core then completes the fence itself. A
        // completion that arrives on its own, as amdgpu's eventfd allows, would free this thread.
        let ran = space.vm.run(node.ip_type, &ibs, self.patience);
        self.note_how_it_ran(&ran);
        match ran {
            Ok(Ran::Retired) => WorkStart::Ran,
            // Nothing ran, so the guest's submission waits and comes again, and the guest never
            // hears of it.
            Ok(Ran::Refused) => WorkStart::Busy,
            Ok(Ran::Hung) => {
                log::error!(
                    "work on engine family {} never finished, and the kernel didn't reset it",
                    node.ip_type
                );
                WorkStart::QueueFailed
            }
            // The guest's driver hears its queue failed, as it would after a fault on its own card.
            Ok(Ran::Faulted(fault)) => {
                log::error!(
                    "work in space {:?} reached {:#x} on the card, where nothing is mapped (fault status \
                     {:#x}). No more work from that space is run, on engine family {} or any other",
                    work.space,
                    fault.address,
                    fault.status,
                    node.ip_type
                );
                WorkStart::QueueFailed
            }
            Err(error) => {
                log::warn!("work on engine family {} not run: {error}", node.ip_type);
                WorkStart::QueueFailed
            }
        }
    }

    /// Where the command submission will go: amdgpu's CS call is the doorbell, and it can fail.
    fn ring_doorbell(&mut self, _queue: u32, _doorbell: u64) -> Result<(), BackendError> {
        Ok(())
    }

    fn close_queue(&mut self, _queue: u32) {}

    fn start_staging_copy(&mut self, _host: &mut dyn HostServices, copy: &StagingCopy) -> StagingStart {
        StagingStart::Finished(match self.copy_staging(copy) {
            Ok(()) => Status::SUCCESS,
            Err(reason) => {
                log::warn!("a copy to the screen failed: {reason}");
                Status::UNSUCCESSFUL
            }
        })
    }

    /// Finished when it returns.
    fn start_slot_copy(&mut self, _host: &mut dyn HostServices, copy: &SlotCopy) -> StagingStart {
        let Some(staging) = screen_copy::slot_as_staging(copy) else {
            return StagingStart::Finished(Status::UNSUCCESSFUL);
        };
        StagingStart::Finished(match self.copy_to_slot(&staging) {
            Ok(()) => Status::SUCCESS,
            Err(reason) => {
                log::warn!("a copy to a viewer's slot failed: {reason}");
                Status::UNSUCCESSFUL
            }
        })
    }

    /// A tiled surface is copied by the DMA engine, which takes any start inside it.
    fn tiled_bytes(&self, surface: &SurfaceShape, _x: u64, _y: u64) -> Option<u64> {
        if !matches!(self.device.info().details.gfx_version.0, 10 | 11) {
            return None;
        }
        let layout = protocol::tiling::Layout::of(surface.tiling?);
        sdma_copy::surface_bytes(surface.pitch, surface.height, layout.swizzle_mode)
    }

    fn poll_staging_copy(&mut self, _host: &mut dyn HostServices, _now: Instant) -> Option<Status> {
        Some(Status::UNSUCCESSFUL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::{CardLabel, DriverVersion, StartDecision, decide_start};

    fn setup_for(version: &str) -> Setup {
        Setup {
            guest_driver_version: DriverVersion::parse(version).unwrap(),
            guest_branch: version.into(),
            driver_folder: Default::default(),
            card: None,
            card_address: None,
        }
    }

    #[test]
    fn these_guest_drivers_may_start_and_another_is_refused() {
        for release in protocol::guest_driver::GUEST_RELEASES {
            let survey = AmdBackend::survey(&setup_for(release)).unwrap();
            assert_eq!(decide_start(&survey), StartDecision::Run);
            assert_eq!(survey.card, CardLabel::Experimental);
        }
        let survey = AmdBackend::survey(&setup_for("1.0.0.0")).unwrap();
        assert!(matches!(decide_start(&survey), StartDecision::Refuse(_)));
    }

    #[test]
    fn address_spaces_stay_inside_the_cards_range() {
        let ranges = [GpuRange { start: 0x400_0000, end: 0x1_0000_0000 }];
        assert!(ranges_fit(&ranges, 0x1_0000, 0x8000_0000_0000));
        assert!(!ranges_fit(&ranges, 0x800_0000, 0x8000_0000_0000));
        assert!(!ranges_fit(&ranges, 0x1_0000, 0x8000_0000));
        assert!(ranges_fit(&[], 0, 0));
    }
}
