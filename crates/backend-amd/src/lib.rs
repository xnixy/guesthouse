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
//! at the method. The chief one is submission: amdgpu has no doorbell, only the `DRM_AMDGPU_CS`
//! call. So [`Backend::start_work`] gets a run ready, with a command buffer of the daemon's last
//! that has the card write the queue's fence, and [`Backend::ring_doorbell`] makes the call. The
//! guest's fence then moves when the card is through, as the core expects of a GPU.

mod card_name;
mod channel_order;
mod copy_ib;
mod device;
mod fault;
mod fence_ib;
mod guest_dma;
mod ib_dump;
mod info;
mod ioctl;
mod memory;
mod moves;
mod nodes;
pub mod protocol;
mod reset;
mod run_wait;
mod screen_copy;
mod sdma_copy;
mod space_vm;
mod submit;
mod traffic;
mod udmabuf;
mod video;
mod video_close;
mod video_ib;
mod watch;

pub use device::{Device, Mapping};
pub use fence_ib::{Writer as FenceWriter, words as fence_words, writer_for as fence_writer_for};
pub use ib_dump::VARIABLE as IB_DUMP_VARIABLE;
pub use info::{
    CardDetails, DeviceInfo, EngineSet, Firmware, HW_IP_DMA, HW_IP_GFX, read_device_info, read_gpu_clock,
};
pub use ioctl::{AMD_VENDOR, amd_render_node};
pub use memory::{
    CpuAccess, DOMAIN_GTT, DOMAIN_VRAM, GemHandle, MapFlags, MemoryType, set_tiling as set_buffer_tiling,
    tiling as buffer_tiling,
};
pub use submit::{Context, Ib, Syncobj, sdma_write_ib, submit, submit_all, wait_eventfd};
pub use udmabuf::Udmabuf;
pub use video::{VARIABLE as VIDEO_VARIABLE, switch_is_on as video_switch_is_on};
pub use video_close::{CLOSE_VARIABLE as VIDEO_CLOSE_VARIABLE, REST_VARIABLE as VIDEO_REST_VARIABLE};

// The buffer calls under harness-facing names, so a proof binary can drive the card without the
// trait. They are the same functions the trait methods use.
pub use memory::{
    adopt_userptr as adopt_buffer, close as close_buffer, create as create_buffer, export as export_buffer,
    import as import_buffer, map as map_buffer, unmap as unmap_buffer,
};

use channel_order::WrittenRows;
use copy_ib::CopyIb;
use guest_dma::Reach;
use guesthouse_backend::{
    AdapterFacts, AllocationShape, Backend, BackendError, CardLabel, Doorbell, DriverFound, DriverSurvey,
    FilePages, GpuRange, GuestBooks, HostServices, MapRequest, MemoryId, MemoryKind, PciCard, Refusal,
    RegistryName, RegistryValue, Setup, SlotCopy, SpaceId, StagingCopy, StagingStart, StandardSurface,
    Status, SurfaceShape, UnmapRequest, VendorQuery, Work, WorkStart,
};
use nodes::{Node, nodes};
use run_wait::Patience;
use screen_copy::{CopyPlan, PIXEL_BYTES};
use sdma_copy::{EngineForm, LinearCopy, TiledCopy};
use space_vm::{GuestFence, Origin, Ran, SpaceVm};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

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
    /// Runs the core has had prepared and not yet let go, by the doorbell that sends each.
    prepared: HashMap<u64, PreparedRun>,
    /// When the runs on the card were last looked at.
    looked_at_runs: Option<Instant>,
    /// Makes dma-bufs of guest memory, where this user may.
    udmabuf: Udmabuf,
    /// Copies of the last command buffers, when the daemon was asked for them.
    ib_dump: Option<ib_dump::Keeper>,
    /// The copy of the video commands being run, kept for its room. See [`video_ib`].
    video_commands: video_ib::Copied,
    /// The books of the guest's video sessions, when it is offered the card's video engine.
    video: Option<video::Sessions>,
    /// What is kept of the firmware's encode sessions, when the daemon was asked to close them or
    /// to rest the engine between them. See [`video_close`].
    video_close: Option<video_close::Watch>,
}

/// A run ready to send: its command buffers, the fence writer in `slot` last.
#[derive(Debug)]
struct PreparedRun {
    space: SpaceId,
    ip_type: u32,
    ibs: Vec<Ib>,
    slot: usize,
    fence: GuestFence,
}

/// The bytes of a fence the card writes.
const FENCE_BYTES: u64 = size_of::<u64>() as u64;

/// Sends a prepared run, if its command buffers are still mapped.
fn send(vm: &mut SpaceVm, run: &PreparedRun) -> io::Result<()> {
    let (_, guests) = run.ibs.split_last().expect("the fence writer is always there");
    if let Some(ib) = vm.first_unmapped(guests) {
        return Err(io::Error::other(format!(
            "{:#x} for {:#x} bytes is no longer mapped",
            ib.address, ib.bytes
        )));
    }
    vm.send(run.ip_type, &run.ibs, run.slot, run.fence)
}

/// How often runs on the card are looked at for ones the kernel ended early.
const LOOK_AT_RUNS_EVERY: Duration = Duration::from_millis(50);
/// What a copy by the DMA engine fails with on a card whose packets aren't written here.
const DMA_NOT_WRITTEN: &str = "the DMA copy is written for GFX10 to GFX12";

/// Stands in at the fence of each run the kernel retired before the card wrote it: one it reset,
/// or cancelled after a reset. A run that went through wrote its fence before the kernel saw it end.
/// Returns the fences it stood in at.
fn stand_in_for_lost_runs(host: &mut dyn HostServices, retired: Vec<GuestFence>) -> Vec<GuestFence> {
    let mut lost = Vec::new();
    for fence in retired {
        let Ok(reached) = host.fence_value(fence.address) else { continue };
        if reached >= fence.value {
            continue;
        }
        lost.push(fence);
        log::warn!(
            "queue {:#x}: the kernel ended a run before it wrote fence value {:#x}, so the host writes it",
            fence.queue,
            fence.value
        );
        if let Err(error) = host.write_u64(fence.address, fence.value) {
            log::warn!("standing in for a lost run: {error}");
        }
    }
    lost
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
            prepared: HashMap::new(),
            looked_at_runs: None,
            udmabuf: Udmabuf::open(),
            ib_dump: None,
            video_commands: video_ib::Copied::default(),
            video: None,
            video_close: None,
        })
    }

    /// Copies the guest's graphics and compute command buffers as they are sent, and writes them to
    /// `folder` now and then and when a run hangs. `started` is when the log's clock started.
    pub fn keep_command_buffers(&mut self, folder: &std::path::Path, started: Instant) {
        let info = self.device.info();
        let (major, minor) = info.details.gfx_version;
        let card = format!(
            "card device={:#06x} revision={:#04x} family={:#x} external_revision={:#x} gfx={major}.{minor}",
            info.device_id, info.pci_revision, info.family, info.details.external_revision
        );
        self.ib_dump = Some(ib_dump::Keeper::new(folder, started, card));
    }

    /// Offers the guest the card's video engine, where it can be: see [`video`]. The daemon calls
    /// this when it was started with [`VIDEO_VARIABLE`] on, before the guest is told anything about
    /// the card. On a card it can't be offered on, a warning says why, and the guest is told what
    /// it is told without the switch. Returns whether it is offered.
    pub fn offer_video(&mut self) -> bool {
        let most = match video::most_throughput(self.device.info()) {
            Ok(most) => most,
            Err(why) => {
                log::warn!("{VIDEO_VARIABLE} is on, but {why}, so the guest is offered no video");
                return false;
            }
        };
        self.device.offer_video();
        self.facts.node_count = self.device.info().engines.count();
        self.facts.engines.video = true;
        self.video = Some(video::Sessions::new(most));
        log::warn!(
            "{VIDEO_VARIABLE} is on, which is an experiment: the guest is offered the card's video engine. \
             Video work of the guest's that the engine doesn't take can have amdgpu reset it, and a reset \
             reaches every program on this host that is using the engine"
        );
        true
    }

    /// Has the daemon close the encode sessions a guest leaves open in the firmware, and rest the
    /// video engine before a new one for the milliseconds `rest` says: see [`video_close`]. The
    /// daemon calls this after [`Self::offer_video`], with whether it was started with
    /// [`VIDEO_CLOSE_VARIABLE`] on and the value of [`VIDEO_REST_VARIABLE`]. Neither does anything
    /// for a guest that isn't offered video.
    pub fn close_and_rest_video(&mut self, closes: bool, rest: Option<&str>) {
        if self.video.is_none() {
            if closes || rest.is_some() {
                log::warn!(
                    "{VIDEO_CLOSE_VARIABLE} and {VIDEO_REST_VARIABLE} do nothing for a guest that isn't \
                     offered video"
                );
            }
            return;
        }
        let rest = video_close::rest_from(rest);
        if closes {
            log::warn!(
                "{VIDEO_CLOSE_VARIABLE} is on, which is an experiment: an encode session the guest leaves \
                 open is closed on the card when the guest says it is over, or when its queue goes"
            );
        }
        if let Some(rest) = rest {
            log::warn!(
                "{VIDEO_REST_VARIABLE} is set, which is an experiment: the video engine rests {rest:?} \
                 before a new session's first work, and nothing else of the guest's is served meanwhile"
            );
        }
        self.video_close = video_close::Watch::new(closes, rest);
    }

    /// Copies a run's command buffers, if that was asked for.
    fn note_run(&mut self, work: &Work<'_>, engine: u32, listed: &[Ib]) {
        if self.ib_dump.is_none() || !matches!(engine, HW_IP_GFX | info::HW_IP_COMPUTE) {
            return;
        }
        let facts =
            ib_dump::RunFacts { queue: work.queue, space: work.space.0, engine, fence: work.fence_value };
        let mut keeper = self.ib_dump.take().expect("just checked");
        let mut read = |ib: &Ib| {
            let at = self.cpu_address(work.space, ib.address, u64::from(ib.bytes)).ok()?;
            let mut bytes = vec![0u8; ib.bytes as usize];
            // SAFETY: `cpu_address` gave an address good for the buffer's bytes. The guest can write
            // them meanwhile, which a copy takes as it finds them.
            unsafe { std::ptr::copy_nonoverlapping(at, bytes.as_mut_ptr(), bytes.len()) };
            Some(bytes)
        };
        keeper.note_run(facts, listed, Instant::now(), &mut read);
        self.ib_dump = Some(keeper);
    }

    /// The full device readings, richer than [`AdapterFacts`], for the harness and the report.
    pub fn device_info(&self) -> &DeviceInfo {
        self.device.info()
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Copies a frame with the CPU. The core starts a copy once the queue that drew the frame has
    /// reached its fence, so the work has retired.
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
        if self.dma_form().is_none() {
            return Err(DMA_NOT_WRITTEN.into());
        }
        self.run_dma(space, &reaches, &plan.commands())
    }

    /// The form of the DMA engine's packets this card takes, or `None` for a card they aren't
    /// written for here.
    fn dma_form(&self) -> Option<EngineForm> {
        EngineForm::of(self.device.info().details.gfx_version)
    }

    /// Runs a plain copy on the DMA engine, and waits for it.
    fn run_linear_dma(&mut self, space: SpaceId, copy: &LinearCopy) -> Result<(), String> {
        let form = self.dma_form().ok_or(DMA_NOT_WRITTEN)?;
        let reaches = [(copy.source_address, copy.bytes), (copy.target_address, copy.bytes)];
        self.run_dma(space, &reaches, &copy.commands(form))
    }

    /// Runs commands on the DMA engine in the daemon's space, and waits for them. `reaches` are the
    /// ranges they touch, each of which must be mapped there.
    fn run_dma(&mut self, space: SpaceId, reaches: &[(u64, u64)], commands: &[u32]) -> Result<(), String> {
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
        let ran = space_vm.run(HW_IP_DMA, None, &[Ib { address: copy_ib::ADDRESS, bytes }], self.patience);
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

    /// Runs a guest's video work on the card's video ring and waits for it. The ring is never given
    /// the guest's own command buffers, only a copy of them in a buffer of the daemon's: see
    /// [`video_ib`] for why.
    fn run_video(&mut self, work: &Work<'_>, ibs: &[Ib]) -> io::Result<Ran> {
        let card = self.device.info();
        let (start_alignment, alignment) = (card.ib_alignment.video.start, card.va_alignment);
        let Some(space) = self.spaces.get_mut(&work.space) else {
            return Err(io::Error::other("its space isn't here"));
        };
        let copied = &mut self.video_commands;
        let read = |ib: &Ib, room: &mut [u32]| read_guest_commands(&self.memory, &space.vm, ib, room);
        copy_video_commands(ibs, start_alignment, copied, read)?;
        traffic::video_commands(work, copied);
        let mut seen = None;
        if let Some(watch) = &self.video_close {
            let rest;
            (seen, rest) = watch.before(work.queue, work.space, copied, Instant::now());
            if !rest.is_zero() {
                log::info!("amd video: resting {rest:?} before the session of queue {:#x}", work.queue);
                std::thread::sleep(rest);
            }
        }
        let started = Instant::now();
        let ran = space.vm.run_video(alignment, work.queue, copied, self.patience);
        traffic::video_ran(work, copied, started.elapsed(), &ran);
        if let Some(watch) = &mut self.video_close
            && reached_the_engine(&ran)
        {
            watch.reached(seen, Instant::now());
        }
        ran
    }

    /// Closes the encode session a queue has left open in the firmware, if the daemon was asked to:
    /// see [`video_close`]. The firmware reads the session's buffer to close it, so one that is no
    /// longer mapped is left alone: a fault there would cost every program on the card a reset.
    fn close_video_session(&mut self, queue: u32) {
        let Some(watch) = &mut self.video_close else { return };
        let Some((space_id, session)) = watch.take(queue) else { return };
        if !watch.closes() {
            return;
        }
        let Some(space) = self.spaces.get_mut(&space_id) else { return };
        if !space.vm.covers(session.address(), video_close::SESSION_BYTES) {
            log::warn!(
                "amd video: queue {queue:#x} left a session open whose buffer at {:#x} is no longer mapped, so \
                 it isn't closed",
                session.address()
            );
            return;
        }
        if space.vm.video_stopped() {
            // The buffer the closing commands would go in may still be read by an earlier run.
            log::debug!(
                "amd video: the session of queue {queue:#x} isn't closed, as no video work is run there"
            );
            return;
        }
        let card = self.device.info();
        let commands = video_close::close_commands(&session);
        let copied = &mut self.video_commands;
        copied.clear();
        let Ok(room) = copied.place(size_of_val(&commands) as u32, card.ib_alignment.video.start) else {
            return;
        };
        room.copy_from_slice(&commands);
        let started = Instant::now();
        let ran = space.vm.run_video(card.va_alignment, queue, copied, self.patience);
        log::info!(
            "amd video close queue={queue:#x} session={:#x} task={} took_us={} {}",
            session.address(),
            session.task.wrapping_add(1),
            started.elapsed().as_micros(),
            traffic::RanText(&ran)
        );
        if reached_the_engine(&ran) {
            watch.reached(None, Instant::now());
        }
        self.note_how_it_ran(&ran);
    }

    /// Takes as ended every session whose context buffer the guest has freed, and closes it if the
    /// daemon was asked to. Called when a guest's driver says a video session is over, which it
    /// says after freeing that buffer and without saying which of the firmware's sessions it was.
    fn end_video_sessions_without_context(&mut self) {
        let Some(watch) = &self.video_close else { return };
        let spaces = &self.spaces;
        let mapped =
            |space: SpaceId, context: u64| spaces.get(&space).is_some_and(|space| space.vm.holds(context, 1));
        for queue in watch.ended(mapped) {
            log::debug!("amd video: the session of queue {queue:#x} has ended");
            self.close_video_session(queue);
        }
    }

    /// What the core is told of work that was waited for, by how it ran.
    fn waited_for(&mut self, work: &Work<'_>, ip_type: u32, ran: io::Result<Ran>) -> WorkStart {
        self.note_how_it_ran(&ran);
        match ran {
            Ok(Ran::Retired) => WorkStart::Ran,
            // Nothing ran, so the guest's submission waits and comes again, and the guest never
            // hears of it.
            Ok(Ran::Refused) => WorkStart::Busy,
            Ok(Ran::Hung) => {
                log::error!("work on engine family {ip_type} never finished, and the kernel didn't reset it");
                WorkStart::QueueFailed
            }
            // The guest's driver hears its queue failed, as it would after a fault on its own card.
            Ok(Ran::Faulted(fault)) => {
                log::error!(
                    "work in space {:?} reached {:#x} on the card, where nothing is mapped (fault status \
                     {:#x}). No more work from that space is run, on engine family {ip_type} or any other",
                    work.space,
                    fault.address,
                    fault.status
                );
                WorkStart::QueueFailed
            }
            Err(error) => {
                log::warn!("work on engine family {ip_type} not run: {error}");
                WorkStart::QueueFailed
            }
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
        self.run_linear_dma(staging.own_space, &linear)
    }

    /// Copies what `from` holds into `to`, a window at a time, by the DMA engine. The work that wrote
    /// it has retired, as the core waits for the queues that could before it moves an allocation.
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
                    self.run_linear_dma(space, &copy)
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
        if let Origin::HostPages { address: host, size, .. } = origin {
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

    /// A buffer on the first open that is these pages of a file, when the kernel makes a dma-buf of
    /// them and amdgpu takes it in.
    fn file_pages_buffer(&mut self, size: u64, pages: &FilePages<'_>) -> Option<GemHandle> {
        if pages.size() != size {
            return None;
        }
        let shared = self.udmabuf.wrap(pages)?;
        memory::import(&self.device, &shared)
            .inspect_err(|error| log::debug!("amdgpu did not take in a dma-buf of guest memory: {error}"))
            .ok()
    }

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Whether a video run got as far as the card's video engine, which then has had work, however it
/// ended there.
fn reached_the_engine(ran: &io::Result<Ran>) -> bool {
    matches!(ran, Ok(Ran::Retired | Ran::Hung | Ran::Faulted(_)))
}

/// Whether every range lies inside the card's usable addresses, `start..end`.
fn ranges_fit(ranges: &[GpuRange], start: u64, end: u64) -> bool {
    ranges.iter().all(|range| range.start >= start && range.end <= end && range.start <= range.end)
}

/// Where in this process a command buffer lies, if it lies in guest pages, which the CPU reads
/// where they are. A guest writes its commands with its CPU, so that is where they are.
fn guest_command_bytes(memory: &HashMap<MemoryId, Bo>, space: &SpaceVm, ib: &Ib) -> Option<*const u8> {
    let (buffer, offset) = space.locate(ib.address)?;
    let bo = memory.values().find(|bo| bo.handle == buffer)?;
    let Origin::HostPages { address, size, .. } = bo.origin else {
        return None;
    };
    if offset.checked_add(u64::from(ib.bytes))? > size {
        return None;
    }
    Some((address + offset) as *const u8)
}

/// A command buffer's dwords, if it lies in guest pages.
fn guest_commands(memory: &HashMap<MemoryId, Bo>, space: &SpaceVm, ib: &Ib) -> Option<Vec<u32>> {
    let at = guest_command_bytes(memory, space, ib)?;
    let mut commands = vec![0u32; ib.bytes as usize / size_of::<u32>()];
    // SAFETY: the pages are mapped in this process for the buffer's bytes while it lives, and the
    // read ends inside them. The guest can write them meanwhile, which a copy of bytes takes as it
    // finds them.
    unsafe {
        std::ptr::copy_nonoverlapping(
            at,
            commands.as_mut_ptr().cast::<u8>(),
            size_of_val(commands.as_slice()),
        )
    };
    Some(commands)
}

/// Reads a command buffer's dwords into `commands`, which holds no more of them than the buffer
/// has. `None`, with nothing read, for a buffer that doesn't lie in guest pages.
fn read_guest_commands(
    memory: &HashMap<MemoryId, Bo>,
    space: &SpaceVm,
    ib: &Ib,
    commands: &mut [u32],
) -> Option<()> {
    let at = guest_command_bytes(memory, space, ib)?;
    let bytes = size_of_val(commands);
    if bytes > ib.bytes as usize {
        return None;
    }
    // SAFETY: as in `guest_commands`, and no more is read than the buffer's bytes.
    unsafe { std::ptr::copy_nonoverlapping(at, commands.as_mut_ptr().cast::<u8>(), bytes) };
    Some(())
}

/// Copies a submission's video command buffers into `copied`, laid out for the daemon's buffer.
/// `read` fills in one buffer's dwords from the guest's pages, or says it can't.
fn copy_video_commands(
    ibs: &[Ib],
    start_alignment: u32,
    copied: &mut video_ib::Copied,
    mut read: impl FnMut(&Ib, &mut [u32]) -> Option<()>,
) -> io::Result<()> {
    copied.clear();
    for ib in ibs {
        let refused = |why: &str| {
            io::Error::other(format!("its commands at {:#x} for {:#x} bytes are {why}", ib.address, ib.bytes))
        };
        let room = copied.place(ib.bytes, start_alignment).map_err(|why| refused(why.0))?;
        read(ib, room).ok_or_else(|| refused("not in guest pages the daemon can read"))?;
    }
    Ok(())
}

/// The first run of bytes a guest's DMA commands name that isn't mapped in its space. The card
/// would fault there.
fn unmapped_dma_reach(memory: &HashMap<MemoryId, Bo>, space: &SpaceVm, ibs: &[Ib]) -> Option<Reach> {
    let commands = ibs.iter().filter_map(|ib| guest_commands(memory, space, ib));
    let mut named = commands.flat_map(|commands| guest_dma::reaches(&commands));
    named.find(|reach| !space.covers(reach.address, reach.bytes))
}

/// Answers an escape where it lies, and logs how. Every escape has an `amd escape` line at debug,
/// which says the same and more of the ones that get no line of their own here. `video` is the
/// books of the guest's video sessions, if it is offered video, which answer a session's escapes.
fn answer_escape(process: u32, data: &mut [u8], video: Option<&mut video::Sessions>) {
    // The answer is written into the buffer the escape came in, so the log takes its copy first.
    let asked = traffic::escape_asked(process, data);
    let answered = match video {
        Some(video) => {
            protocol::escape::answer_with(data, |code, payload| video.answer(process, code, payload))
        }
        None => protocol::escape::answer(data),
    };
    match &answered {
        Ok(answered) if !answered.done && answered.code.needed_by_the_guest() => log::warn!(
            "escape from {process}: service {} subfunction {:#x}, not shaped as the guest's driver \
             sends it, answered as failed; the guest can't go on without it",
            answered.code.service,
            answered.code.subfunction
        ),
        Ok(_) => {}
        Err(refused) => log::info!(
            "an escape from {process} is {} ({} bytes, starting {})",
            refused.0,
            data.len(),
            protocol::bytes::first_dwords(data, 4)
        ),
    }
    traffic::escape_answered(process, asked, &answered);
}

/// The answer to an adapter query this backend owns, appended to `out`, or `None` for one it leaves
/// to the core.
fn own_answer(info: &DeviceInfo, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
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
    if let Some(status) = protocol::node_metadata::answer(query, &info.engines, out) {
        return Some(status);
    }
    protocol::adapter_info::answer(query, info, out)
}

/// Answers an adapter query as [`own_answer`] does, and logs that it was asked.
fn answer_query(info: &DeviceInfo, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
    let answer = own_answer(info, query, out);
    traffic::query(query, answer);
    answer
}

/// The registry value this backend has under a name, logged when it has one. A name it has no value
/// for is the core's to log.
fn registry_answer(name: &RegistryName) -> Option<RegistryValue> {
    let value = protocol::guest_driver::registry(name);
    if let Some(value) = &value {
        traffic::registry(name, value);
    }
    value
}

/// Reads a submission as far as the card's readings go, and logs what its private data holds: the
/// engine behind its node and the command buffers it names, or `None` for one its queue fails on.
fn submission_read(info: &DeviceInfo, work: &Work<'_>) -> Option<(Node, Vec<Ib>)> {
    // The kernel driver fails a submission whose private data is wrong, and the guest then
    // treats the device as lost, so the queue stops here the same way.
    let submission = match protocol::command_data::check(work.private) {
        Ok(submission) => submission,
        Err(refused) => {
            log::warn!("refused a submission on queue {:#x}: {}", work.queue, refused.0);
            traffic::submission_refused(work, refused.0);
            return None;
        }
    };
    let Some(node) = nodes(&info.engines).get(work.node as usize).copied() else {
        log::warn!("work for node {}, which this card hasn't got", work.node);
        return None;
    };
    let ibs: Vec<Ib> = protocol::command_data::command_buffers(work.private)
        .into_iter()
        .map(|buffer| Ib { address: buffer.address, bytes: buffer.bytes })
        .collect();
    traffic::submission(work, node.ip_type, &submission, &ibs);
    Some((node, ibs))
}

/// Whether the kernel driver would take a new context's private data, and logs what it holds.
fn context_taken(process: u32, node: u32, private: &[u8]) -> bool {
    let checked = protocol::context::check(private);
    if let Err(refused) = &checked {
        log::info!("refused a context on node {node}: {}", refused.0);
    }
    traffic::context(process, node, private, &checked);
    checked.is_ok()
}

/// Checks a new hardware queue's private data, which nothing is written back into, and logs it.
fn queue_checked(queue: u32, serial: u32, private: &[u8]) -> Result<(), BackendError> {
    let checked = protocol::hw_queue::check(private);
    traffic::queue(queue, serial, private, checked);
    checked.map_err(|refused| BackendError::new(refused.0))
}

/// Builds the facts the guest is told from the card's own readings.
fn facts_from(info: &DeviceInfo) -> AdapterFacts {
    AdapterFacts {
        card: PciCard { vendor: AMD_VENDOR, device: info.device_id, revision: info.pci_revision },
        name: card_name::card_name(info.device_id, info.pci_revision)
            .unwrap_or_else(|| format!("AMD GPU {:#06x}", info.device_id)),
        video_memory: info.vram_total,
        gtt_memory: info.gtt_total,
        bar_size: info.visible_vram_total,
        node_count: info.engines.count(),
        engines: guesthouse_backend::AdapterEngines {
            graphics: info.engines.graphics,
            compute: info.engines.compute,
            copy: info.engines.dma,
            video: info.engines.video,
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
        self.memory.insert(
            id,
            Bo { handle, origin: Origin::HostPages { address: host_address, size, as_dma_buf: false } },
        );
        Ok(id)
    }

    fn adopt_file_pages(
        &mut self,
        host_address: u64,
        size: u64,
        pages: &FilePages<'_>,
    ) -> Result<MemoryId, BackendError> {
        let Some(handle) = self.file_pages_buffer(size, pages) else {
            return self.adopt_host_pages(host_address, size);
        };
        let id = MemoryId(self.new_id());
        let origin = Origin::HostPages { address: host_address, size, as_dma_buf: true };
        self.memory.insert(id, Bo { handle, origin });
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
        let moved = if self.dma_form().is_some() && !dma_stopped {
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
        self.prepared.retain(|_, run| run.space != space);
        if let Some(watch) = &mut self.video_close {
            watch.space_gone(space);
        }
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

    fn finish_queue_data(&mut self, queue: u32, private: &mut [u8], serial: u32) -> Result<(), BackendError> {
        queue_checked(queue, serial, private)
    }

    /// The answer is in the escape's own header. The trait has no status to return beside it.
    fn escape(&mut self, process: u32, data: &mut [u8], _books: &GuestBooks<'_>) {
        // Read before the answer is written, as that goes into the same buffer.
        let session_over = self.video_close.is_some() && video_close::says_a_session_is_over(data);
        answer_escape(process, data, self.video.as_mut());
        if session_over {
            self.end_video_sessions_without_context();
        }
    }

    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
        answer_query(self.device.info(), query, out)
    }

    fn registry_value(&mut self, name: &RegistryName) -> Option<RegistryValue> {
        registry_answer(name)
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
    fn context_data_valid(&mut self, process: u32, node: u32, private: &[u8]) -> bool {
        context_taken(process, node, private)
    }

    fn process_gone(&mut self, process: u32) {
        if let Some(video) = &mut self.video {
            video.process_gone(process);
        }
    }

    fn all_processes_gone(&mut self) {
        if let Some(video) = &mut self.video {
            video.all_gone();
        }
        // The core closes no queue and no space of a guest that restarted.
        if let Some(watch) = &mut self.video_close {
            watch.all_gone();
        }
    }

    fn start_work(&mut self, _host: &mut dyn HostServices, work: &Work<'_>) -> WorkStart {
        let Some((node, mut ibs)) = submission_read(self.device.info(), work) else {
            return WorkStart::QueueFailed;
        };
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
        if node.ip_type == HW_IP_DMA
            && let Some(reach) = unmapped_dma_reach(&self.memory, &space.vm, &ibs)
        {
            log::warn!(
                "work on the DMA engine in space {:?} names {:#x} for {:#x} bytes, which isn't all mapped \
                 there, so it isn't run",
                work.space,
                reach.address,
                reach.bytes
            );
            return WorkStart::QueueFailed;
        }
        if node.ip_type == info::HW_IP_VCN_ENC {
            // Before anything below can send the guest's own command buffers: see `video_ib`.
            if space.vm.video_stopped() {
                if space.vm.first_video_refusal() {
                    log::warn!(
                        "no more video work is run in space {:?}: an earlier video run there was sent and \
                         never seen to end, and the buffer its commands are in can't be written again \
                         while the card may still read it",
                        work.space
                    );
                }
                return WorkStart::QueueFailed;
            }
            let ran = self.run_video(work, &ibs);
            return self.waited_for(work, node.ip_type, ran);
        }
        let card = self.device.info();
        let writer = fence_ib::writer_for(node.ip_type, card.details.gfx_version.0)
            .filter(|_| space.vm.holds(work.fence_gpu_address, FENCE_BYTES));
        if let Some(writer) = writer {
            // The card writes the fence itself when the run is through, so nothing waits here.
            let written =
                space.vm.fence_writer(card.va_alignment, writer, work.fence_gpu_address, work.fence_value);
            return match written {
                Ok(Some((slot, last))) => {
                    self.note_run(work, node.ip_type, &ibs);
                    ibs.push(last);
                    let doorbell = self.new_id();
                    let fence =
                        GuestFence { queue: work.queue, address: work.fence, value: work.fence_value };
                    let (space, ip_type) = (work.space, node.ip_type);
                    self.prepared.insert(doorbell, PreparedRun { space, ip_type, ibs, slot, fence });
                    WorkStart::Started { doorbell, work: 1 }
                }
                Ok(None) => WorkStart::Busy,
                Err(error) => {
                    log::warn!("work on engine family {} not run: {error}", node.ip_type);
                    WorkStart::QueueFailed
                }
            };
        }
        // An engine with no fence writer is waited for here, and the core then completes the fence.
        self.note_run(work, node.ip_type, &ibs);
        let Some(space) = self.spaces.get_mut(&work.space) else {
            return WorkStart::QueueFailed;
        };
        let ran = space.vm.run(node.ip_type, Some(work.queue), &ibs, self.patience);
        self.waited_for(work, node.ip_type, ran)
    }

    /// amdgpu's CS call is the doorbell, and it can fail.
    fn ring_doorbell(&mut self, _queue: u32, doorbell: u64) -> Result<Doorbell, BackendError> {
        let Some(run) = self.prepared.remove(&doorbell) else {
            return Err(BackendError::new("no run was prepared for this doorbell"));
        };
        let Some(space) = self.spaces.get_mut(&run.space) else {
            return Err(BackendError::new("the run's address space has closed"));
        };
        match send(&mut space.vm, &run) {
            Ok(()) => Ok(Doorbell::Rung),
            // amdgpu refuses a submission while pages it lists are moving, and takes it later.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                self.prepared.insert(doorbell, run);
                Ok(Doorbell::NotYet)
            }
            Err(error) => {
                space.vm.drop_fence_writer(run.slot);
                if reset::os_error(&error) == Some(libc::ETIME)
                    && let Some(keeper) = &mut self.ib_dump
                {
                    keeper.keep_for(run.fence.queue, run.fence.value, "ETIME", Instant::now());
                }
                Err(BackendError::new(format!("engine family {}: {error}", run.ip_type)))
            }
        }
    }

    fn close_queue(&mut self, queue: u32) {
        self.close_video_session(queue);
        for space in self.spaces.values_mut() {
            space.vm.forget_queue(queue);
        }
        let unsent: Vec<u64> = self
            .prepared
            .iter()
            .filter(|(_, run)| run.fence.queue == queue)
            .map(|(&doorbell, _)| doorbell)
            .collect();
        for doorbell in unsent {
            if let Some(run) = self.prepared.remove(&doorbell)
                && let Some(space) = self.spaces.get_mut(&run.space)
            {
                space.vm.drop_fence_writer(run.slot);
            }
        }
    }

    fn poll_work(&mut self, host: &mut dyn HostServices, now: Instant) {
        if self.looked_at_runs.is_some_and(|last| now.saturating_duration_since(last) < LOOK_AT_RUNS_EVERY) {
            return;
        }
        self.looked_at_runs = Some(now);
        for space in self.spaces.values_mut() {
            let lost = stand_in_for_lost_runs(host, space.vm.take_retired());
            if let (Some(keeper), Some(fence)) = (&mut self.ib_dump, lost.first()) {
                keeper.keep_for(fence.queue, fence.value, "hung", now);
            }
        }
        if let Some(keeper) = &mut self.ib_dump {
            keeper.now_and_then(now);
        }
    }

    /// A run can take seconds on a small card, and several can be waiting on it.
    fn work_patience(&self) -> Option<std::time::Duration> {
        Some(Patience::RUN.hung)
    }

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
        self.dma_form()?;
        let layout = protocol::tiling::Layout::of(surface.tiling?);
        let gfx = self.device.info().details.gfx_version;
        sdma_copy::surface_bytes(surface.pitch, surface.height, layout.swizzle_mode, gfx)
    }

    fn poll_staging_copy(&mut self, _host: &mut dyn HostServices, _now: Instant) -> Option<Status> {
        Some(Status::UNSUCCESSFUL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::{
        CardLabel, DriverVersion, GuestSpan, MemoryError, SharedSpan, StartDecision, decide_start,
    };

    /// Guest memory that holds only fences, by guest physical address.
    #[derive(Default)]
    struct Fences(HashMap<u64, u64>);

    impl HostServices for Fences {
        fn take_gpu_addresses(&mut self, _size: u64, _align: u64) -> Result<u64, BackendError> {
            unreachable!()
        }
        fn read_at_gpu_address(&self, _space: SpaceId, _address: u64, _out: &mut [u8]) -> Option<usize> {
            unreachable!()
        }
        fn shared_span(&self, _space: SpaceId, _address: u64) -> Option<SharedSpan> {
            unreachable!()
        }
        fn guest_span(&self, _space: SpaceId, _address: u64) -> Option<GuestSpan> {
            unreachable!()
        }
        fn read_guest_span(&self, _space: SpaceId, _address: u64, _out: &mut [u8]) -> Option<usize> {
            unreachable!()
        }
        fn write_guest_span(&mut self, _space: SpaceId, _address: u64, _value: u32) -> bool {
            unreachable!()
        }
        fn write_u32(&mut self, _address: u64, _value: u32) -> Result<(), MemoryError> {
            unreachable!()
        }
        fn write_u64(&mut self, address: u64, value: u64) -> Result<(), MemoryError> {
            self.0.insert(address, value);
            Ok(())
        }
        fn read_guest(&self, _address: u64, _out: &mut [u8]) -> usize {
            unreachable!()
        }
        fn fence_value(&self, fence: u64) -> Result<u64, MemoryError> {
            Ok(self.0.get(&fence).copied().unwrap_or(0))
        }
    }

    #[test]
    fn a_run_that_ended_before_writing_its_fence_gets_it_written_by_the_host() {
        const WRITTEN: u64 = 0x1000;
        const LOST: u64 = 0x2000;
        let mut host = Fences::default();
        host.0.insert(WRITTEN, 7);
        host.0.insert(LOST, 3);
        let retired = vec![
            GuestFence { queue: 1, address: WRITTEN, value: 7 },
            GuestFence { queue: 2, address: LOST, value: 4 },
        ];
        // Only the lost run is named, as that is the one whose command buffers are kept.
        let lost = stand_in_for_lost_runs(&mut host, retired);
        assert_eq!(lost, vec![GuestFence { queue: 2, address: LOST, value: 4 }]);
        assert_eq!(host.0[&WRITTEN], 7);
        assert_eq!(host.0[&LOST], 4);
    }

    #[test]
    fn a_fence_already_past_a_lost_run_is_left_alone() {
        const FENCE: u64 = 0x1000;
        let mut host = Fences::default();
        host.0.insert(FENCE, 9);
        let lost = stand_in_for_lost_runs(&mut host, vec![GuestFence { queue: 1, address: FENCE, value: 8 }]);
        assert!(lost.is_empty());
        assert_eq!(host.0[&FENCE], 9);
    }

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

    /// Guest pages that hold command buffers, by GPU address: each dword is its own address.
    fn read_from_guest_pages(readable: std::ops::Range<u64>) -> impl FnMut(&Ib, &mut [u32]) -> Option<()> {
        move |ib, room| {
            let end = ib.address + u64::from(ib.bytes);
            (readable.start <= ib.address && end <= readable.end).then(|| {
                for (index, word) in room.iter_mut().enumerate() {
                    *word = (ib.address + 4 * index as u64) as u32;
                }
            })
        }
    }

    #[test]
    fn video_commands_are_copied_out_of_guest_pages_and_run_from_the_daemons_buffer() {
        const START: u32 = 256;
        let guests = [Ib { address: 0x4_2000_1000, bytes: 0xC }, Ib { address: 0x4_2000_3000, bytes: 0x8 }];
        let mut copied = video_ib::Copied::default();
        copy_video_commands(&guests, START, &mut copied, read_from_guest_pages(0..u64::MAX)).unwrap();
        // No run is at an address the guest gave, and each holds what the guest's buffer held.
        let runs = [
            Ib { address: video_ib::ADDRESS, bytes: 0xC },
            Ib { address: video_ib::ADDRESS + 256, bytes: 0x8 },
        ];
        assert_eq!(copied.runs(), runs);
        let held: Vec<&[u32]> = copied.buffers().collect();
        assert_eq!(held, [&[0x2000_1000, 0x2000_1004, 0x2000_1008][..], &[0x2000_3000, 0x2000_3004][..]]);

        // What an earlier submission left is gone before the next is copied.
        copy_video_commands(&guests[1..], START, &mut copied, read_from_guest_pages(0..u64::MAX)).unwrap();
        assert_eq!(copied.runs(), [Ib { address: video_ib::ADDRESS, bytes: 0x8 }]);
    }

    #[test]
    fn video_commands_the_daemon_cannot_copy_are_not_run() {
        const START: u32 = 256;
        let mut copied = video_ib::Copied::default();
        let failed = |guests: &[Ib], copied: &mut video_ib::Copied| {
            let read = read_from_guest_pages(0x1000..0x2000);
            copy_video_commands(guests, START, copied, read).unwrap_err().to_string()
        };
        // In video memory, say, where the CPU doesn't read them.
        assert_eq!(
            failed(&[Ib { address: 0x1000, bytes: 8 }, Ib { address: 0x8000, bytes: 8 }], &mut copied),
            "its commands at 0x8000 for 0x8 bytes are not in guest pages the daemon can read"
        );
        assert_eq!(
            failed(&[Ib { address: 0x1000, bytes: 6 }], &mut copied),
            "its commands at 0x1000 for 0x6 bytes are not a whole number of dwords"
        );
        let long = video_ib::BYTES as u32 + 4;
        assert_eq!(
            failed(&[Ib { address: 0x1000, bytes: long }], &mut copied),
            "its commands at 0x1000 for 0x10004 bytes are more than the daemon's buffer holds"
        );
    }

    #[test]
    fn only_a_video_run_that_was_sent_counts_as_work_the_engine_had() {
        let fault = fault::Fault { address: 0x3000, status: 1 };
        for sent in [Ok(Ran::Retired), Ok(Ran::Hung), Ok(Ran::Faulted(fault))] {
            assert!(reached_the_engine(&sent), "{sent:?}");
        }
        // amdgpu kept saying try again, or the commands couldn't be copied or submitted.
        assert!(!reached_the_engine(&Ok(Ran::Refused)));
        assert!(!reached_the_engine(&Err(io::Error::other("submitting"))));
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
