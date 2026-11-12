//! The NVIDIA backend.
//!
//! This talks to the host's kernel module through the [`rm::Rm`] layer, runs the guest's commands on
//! real channels, and answers what the guest's drivers ask of their own kernel driver.
//!
//! Three things vary, and each has its own place. The [`profile`] module holds a generation's classes
//! and a driver branch's layouts, and cards themselves are read live. See `ARCHITECTURE.md`.

pub mod answers;
pub mod channel;
mod context_room;
mod depth;
pub mod encode_sessions;
pub mod escape;
#[cfg(any(test, feature = "fake-rm"))]
pub mod fake_guest;
mod guest_clock;
pub mod inf;
mod list_repair;
mod local_window;
mod page_kind;
mod peek;
pub mod private_data;
pub mod profile;
pub mod pushbuffer;
pub mod queue;
pub mod record;
mod ring_position;
mod ring_request;
pub mod rm;
pub mod sensors;
pub mod staging;
mod standard;
pub mod startup;
mod tiling;

use answers::adapter::{AdapterAnswers, Inputs};
use answers::nodes::{ENGINE_VIDEO_ENCODE, Node, node_list};
use answers::perf_data;
use answers::registry::AdapterRegistry;
use channel::{ListChannel, VideoEngine};
use encode_sessions::EncodeSessions;
use guesthouse_backend::{
    AdapterFacts, AllocationShape, Backend, BackendError, DriverSurvey, GpuRange, GuestBooks, HostServices,
    MapRequest, MemoryId, MemoryKind, PciCard, Refusal, RegistryName, RegistryValue, Setup, SlotCopy,
    SpaceId, StagingCopy, StagingStart, StandardSurface, Status, SurfaceShape, UnmapRequest, VendorQuery,
    Work, WorkStart,
};
use inf::{INF_NAME, Inf, read_infs};
use profile::{
    Generation, GuestBranch, HostBranch, generation_for, guest_branch, guest_channel_classes, host_branch,
};
use queue::LiveQueue;
use record::{CardReader, HostFacts, SummaryFacts, build, read_host_facts, summary};
use ring_position::GuestRing;
use ring_request::RingMoves;
use rm::device::{AddressSpace, Device};
use rm::linux::LinuxRm;
use rm::listing::{self, CardPlace};
use rm::{Handle, Rm, RmError};
use sensors::SnapshotReader;
use staging::Copier;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Instant;

pub(crate) const CHANNEL_ADDRESS_ALIGN: u64 = 64 << 10;

const NVIDIA_VENDOR: u16 = 0x10DE;

/// How many entries a queue may have unfinished before its next submission waits. This is our own
/// measured number: a long benchmark finished at this limit, and failed at 6,144 and with no limit.
pub(crate) const UNFINISHED_ENTRIES_LIMIT: u64 = 4000;

/// What the guest's runtime says a Vulkan context is for. Vulkan's video queue families can also
/// transfer, and it uses them for plain copies, so its contexts on video nodes get copy channels.
const CLIENT_HINT_VULKAN: u32 = 4;

/// A word in a video context's private data, which we set to one.
const VIDEO_CONTEXT_WORD: usize = 0xD7C;

const RECORD_QUEUE_TOKEN: usize = 0x40;
const RECORD_QUEUE_HEAP: usize = 0x5C;
const QUEUE_HEAP_SIZE: u64 = 1 << 32;
/// A queue that brings no ring of its own asks here how many entries a list it sends may have, and
/// reads the answer from the second field. Direct3D 9 makes no device if it gets fewer than it asked.
const QUEUE_LIST_ENTRIES_ASKED: usize = 0x14;
const QUEUE_LIST_ENTRIES: usize = 0x50;

pub(crate) fn memory_error(error: RmError) -> BackendError {
    BackendError::new(error.to_string())
}

/// Gives a queue without a ring of its own the list length it asked for, as far as the ring here
/// takes one. `None` if the private data is too short to hold the answer.
fn grant_list_entries(private: &mut [u8]) -> Option<()> {
    let asked = private.get(QUEUE_LIST_ENTRIES_ASKED..QUEUE_LIST_ENTRIES_ASKED + 4)?;
    let asked = u32::from_le_bytes(asked.try_into().ok()?);
    let granted = asked.min(ListChannel::LONGEST_LIST);
    private.get_mut(QUEUE_LIST_ENTRIES..QUEUE_LIST_ENTRIES + 4)?.copy_from_slice(&granted.to_le_bytes());
    Some(())
}

fn handle_of(memory: MemoryId) -> Handle {
    memory.0 as Handle
}

/// The profile for the card's architecture, or a refusal that names it. Only reads.
pub fn card_generation<R: Rm>(card: &mut CardReader<'_, R>) -> Result<&'static Generation, Refusal> {
    let architecture = card.read(record::ARCHITECTURE, &[]);
    let architecture = architecture.map_err(|error| Refusal(format!("Reading the card failed: {error}")))?;
    let architecture = u32::from_le_bytes(architecture[..4].try_into().expect("four bytes"));
    generation_for(architecture)
        .ok_or_else(|| Refusal(format!("This build has no profile for GPU architecture {architecture:#x}.")))
}

/// The card's place for the log.
fn place_text(place: &CardPlace) -> String {
    match place.address {
        Some(address) => format!("{address}, device file nvidia{}", place.minor),
        None => format!("device file nvidia{}, guessed, as the driver's list couldn't be read", place.minor),
    }
}

/// How an RM comes to be, which tests don't use.
pub trait OpenRm: Rm + Sized {
    fn open_rm(version: &str, sizes: &'static HostBranch, place: &CardPlace) -> Result<Self, RmError>;
}

impl OpenRm for LinuxRm {
    fn open_rm(version: &str, sizes: &'static HostBranch, place: &CardPlace) -> Result<Self, RmError> {
        Self::open(version, sizes, place)
    }
}

#[cfg(any(test, feature = "fake-rm"))]
impl OpenRm for rm::fake::FakeRm {
    fn open_rm(_version: &str, _sizes: &'static HostBranch, _place: &CardPlace) -> Result<Self, RmError> {
        Err(RmError::System("a fake RM is built by hand".to_owned()))
    }
}

/// See the crate documentation.
pub struct NvidiaBackend<R> {
    pub(crate) device: Device<R>,
    pub(crate) generation: &'static Generation,
    pub(crate) host_branch: &'static HostBranch,
    guest: &'static GuestBranch,
    /// The class behind each handle the guest's driver uses.
    channel_classes: Vec<(u32, u32)>,
    nodes: Vec<Node>,
    encode_sessions: EncodeSessions,
    facts: AdapterFacts,
    answers: AdapterAnswers,
    pub(crate) spaces: HashMap<SpaceId, AddressSpace>,
    next_space: u64,
    pub(crate) queues: HashMap<u32, LiveQueue>,
    pub(crate) copier: Option<Copier>,
    pub(crate) peek_page: Option<peek::PeekPage>,
    /// Rings named in queues' private data, by queue.
    pub(crate) guest_rings: HashMap<u32, GuestRing>,
    /// Rings asked for, which queues can move to.
    pub(crate) ring_moves: RingMoves,
    /// Where shaders' local addresses sit, from the first queue that set it.
    pub(crate) local_window: local_window::LocalWindow,
    /// The memory made last, which the private data finished next belongs to.
    last_memory: Option<MemoryId>,
    /// The host's live readings, if it has any.
    sensors: Option<Box<dyn SnapshotReader>>,
    card_uuid: [u8; 16],
}

impl<R: Rm> NvidiaBackend<R> {
    /// How long a graphics channel group may hold the GPU, for channels made from now on. 0 leaves
    /// the RM's own choice.
    pub fn set_graphics_timeslice(&mut self, micros: u64) {
        self.device.graphics_timeslice_micros = micros;
    }

    /// Reads the card and works out everything the guest will be told. Nothing here changes the card.
    pub fn start(
        mut device: Device<R>,
        host_branch: &'static HostBranch,
        guest: &'static GuestBranch,
        setup: &Setup,
        host: &HostFacts,
        infs: &[Vec<u8>],
    ) -> Result<Self, Refusal> {
        let failed = |error: RmError| Refusal(format!("Reading the card failed: {error}"));
        let mut card = CardReader::new(&mut device);
        let generation = card_generation(&mut card)?;
        if !generation.run_on_guest_branches.contains(&guest.name) {
            log::warn!(
                "{} cards have not been run with guest driver branch {}. This is a try, and it may fail.",
                generation.name,
                guest.name
            );
        }
        let engines = card.engines().map_err(failed)?;
        let graphics_copy = card.graphics_copy().map_err(failed)?;
        let nodes = node_list(&engines, graphics_copy, &generation.nodes);
        let record = build(&guest.record, generation, &mut card, host, &nodes)?;
        let node_count = nodes.len() as u32;

        let name = card.text(record::NAME, 4).map_err(failed)?;
        let pci = card.read(record::PCI_INFO, &[]).map_err(failed)?;
        let word = |at: usize| u32::from_le_bytes(pci[at..at + 4].try_into().expect("four bytes"));
        let found =
            PciCard { vendor: word(0) as u16, device: (word(0) >> 16) as u16, revision: word(8) as u8 };
        let subsystem = word(4);
        if let Some(card) = setup.card.filter(|card| card.device != found.device) {
            return Err(Refusal(format!(
                "Setup chose card {:04x} but the first NVIDIA device is {:04x}. Only the first NVIDIA GPU can \
                 be used for now.",
                card.device, found.device
            )));
        }
        let pci_card = setup.card.unwrap_or(found);

        let infs: Vec<Inf> = infs.iter().map(|bytes| Inf::parse(bytes)).collect();
        let registry = AdapterRegistry::from_infs(&infs, &pci_card, subsystem)?;
        let layout = &guest.record;
        let dedicated = record[layout.dedicated..layout.dedicated + 8].try_into().expect("eight bytes");
        let guid_seed: String =
            record[layout.uuid..layout.uuid + 16].iter().map(|byte| format!("{byte:02x}")).collect();
        let clock = |at: usize| {
            let bytes = record[at..at + 4].try_into().expect("four bytes");
            u64::from(u32::from_le_bytes(bytes)) * 1000
        };
        let (graphics_hertz, memory_hertz) = (clock(layout.rated_clocks), clock(layout.rated_clocks + 4));
        let card_uuid = record[layout.uuid..layout.uuid + 16].try_into().expect("sixteen bytes");
        let inputs = Inputs {
            guid_seed,
            graphics_hertz,
            memory_hertz,
            nodes: nodes.clone(),
            registry,
            record,
            engines,
        };
        Ok(Self {
            device,
            generation,
            host_branch,
            guest,
            channel_classes: guest_channel_classes(guest, generation),
            nodes,
            encode_sessions: EncodeSessions::default(),
            facts: AdapterFacts {
                card: pci_card,
                name,
                video_memory: u64::from_le_bytes(dedicated),
                // NVIDIA's parts here report their dedicated memory as fully CPU-visible and do not
                // separate a GART pool, so these two carry no new NVIDIA behaviour.
                gtt_memory: 0,
                bar_size: u64::from_le_bytes(dedicated),
                node_count,
                engines: guesthouse_backend::AdapterEngines::all(),
                guest_driver_version: setup.guest_driver_version.packed(),
            },
            answers: AdapterAnswers::new(inputs, layout),
            spaces: HashMap::new(),
            next_space: 1,
            queues: HashMap::new(),
            copier: None,
            peek_page: None,
            guest_rings: HashMap::new(),
            ring_moves: RingMoves::default(),
            local_window: local_window::LocalWindow::default(),
            last_memory: None,
            sensors: None,
            card_uuid,
        })
    }

    /// Where live readings come from. Nothing is read from the card unless this is called.
    pub fn set_sensors(&mut self, sensors: Box<dyn SnapshotReader>) {
        self.sensors = Some(sensors);
    }

    /// The video engine this work's commands are for, if they are for one.
    pub(crate) fn video_engine(&self, work: &Work<'_>) -> Option<VideoEngine> {
        if work.client_hint == CLIENT_HINT_VULKAN {
            return None;
        }
        let node = self.nodes.get(work.node as usize)?;
        let (rm_engine, instance) = node.video?;
        let encodes = node.engine == ENGINE_VIDEO_ENCODE;
        let generation = self.generation;
        let class = if encodes { generation.video_encode_class } else { generation.video_decode_class };
        Some(VideoEngine { rm_engine, class, instance })
    }

    /// What a person trying a card can post: what the backend found and will tell the guest, with
    /// nothing in it that identifies the card or the PC.
    pub fn card_summary(&self) -> String {
        summary(&SummaryFacts {
            record: self.answers.record(),
            layout: &self.guest.record,
            generation: self.generation,
            guest_branch: self.guest.name,
            engines: self.answers.rm_engines(),
            nodes: &self.nodes,
        })
    }

    pub fn answers(&self) -> &AdapterAnswers {
        &self.answers
    }

    pub fn device(&self) -> &Device<R> {
        &self.device
    }

    pub fn device_mut(&mut self) -> &mut Device<R> {
        &mut self.device
    }

    /// The page kind a map needs, from the driver's protection value, if its generation has kinds to set.
    fn page_kind_of(&self, request: &MapRequest) -> Option<u32> {
        let kind = page_kind::page_kind(self.generation.page_kinds, request.driver_protection)?;
        let class = page_kind::depth_class(request.driver_protection);
        log::debug!(
            "a map at {:#x} is depth class {class}, so its pages get kind {kind:#x}",
            request.address
        );
        Some(kind)
    }

    fn space(spaces: &HashMap<SpaceId, AddressSpace>, space: SpaceId) -> Result<&AddressSpace, BackendError> {
        spaces.get(&space).ok_or_else(|| BackendError::new("this address space is closed"))
    }
}

impl<R: Rm + OpenRm> Backend for NvidiaBackend<R> {
    fn supports(card: &PciCard) -> bool {
        card.vendor == NVIDIA_VENDOR
    }

    fn survey(setup: &Setup) -> Result<DriverSurvey, Refusal> {
        startup::survey(setup)
    }

    fn open(setup: &Setup, survey: &DriverSurvey) -> Result<Self, Refusal> {
        let no_profile = || Refusal("The start decision passed a branch with no profile.".to_owned());
        let host_profile = host_branch(&survey.host.branch).ok_or_else(no_profile)?;
        let guest_profile = guest_branch(&setup.guest_branch).ok_or_else(no_profile)?;
        let inf_path = setup.driver_folder.join(INF_NAME);
        let infs = read_infs(&setup.driver_folder)
            .map_err(|error| Refusal(format!("{}: {error}", inf_path.display())))?;
        let place =
            listing::place_for(&listing::read_listing(), setup.card_address.as_ref()).map_err(Refusal)?;
        let host = read_host_facts(place.address.as_ref()).map_err(Refusal)?;
        let opened = |error: RmError| Refusal(format!("Opening the card failed: {error}"));
        let rm = R::open_rm(&survey.host.version, host_profile, &place).map_err(opened)?;
        let device = Device::open_place(rm, host_profile, &place).map_err(opened)?;
        log::info!("opened the card at {}, device instance {}", place_text(&place), device.instance);
        let mut backend = Self::start(device, host_profile, guest_profile, setup, &host, &infs)?;
        // Readings are a bonus, so a failure here only means going without.
        match sensors::start(&backend.card_uuid) {
            Ok(snapshot) => backend.set_sensors(Box::new(snapshot)),
            Err(error) => log::info!("no host sensors: {error}"),
        }
        Ok(backend)
    }

    fn facts(&self) -> &AdapterFacts {
        &self.facts
    }

    fn allocate_video_memory(
        &mut self,
        size: u64,
        kind: MemoryKind,
    ) -> Result<(MemoryId, u64), BackendError> {
        let (handle, given) = self.device.alloc_video_memory(size, kind).map_err(memory_error)?;
        let memory = MemoryId(u64::from(handle));
        self.last_memory = Some(memory);
        Ok((memory, given))
    }

    fn adopt_host_pages(&mut self, host_address: u64, size: u64) -> Result<MemoryId, BackendError> {
        let handle = self.device.alloc_host_pages(host_address, size).map_err(memory_error)?;
        let memory = MemoryId(u64::from(handle));
        self.last_memory = Some(memory);
        Ok(memory)
    }

    fn release_memory(&mut self, memory: MemoryId) {
        self.ring_moves.released(memory);
        if let Err(error) = self.device.free(handle_of(memory)) {
            log::warn!("freeing memory: {error}");
        }
    }

    fn preserve_contents(
        &mut self,
        host: &mut dyn HostServices,
        own_space: SpaceId,
        from: MemoryId,
        to: MemoryId,
        size: u64,
    ) -> Result<(), BackendError> {
        self.move_contents(host, own_space, handle_of(from), handle_of(to), size)
    }

    fn read_memory_start(
        &mut self,
        host: &mut dyn HostServices,
        own_space: SpaceId,
        memory: MemoryId,
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        self.peek(host, own_space, handle_of(memory), out)
    }

    fn create_address_space(&mut self, ranges: &[GpuRange]) -> Result<SpaceId, BackendError> {
        let space =
            self.device.create_address_space(&context_room::without_room(ranges)).map_err(memory_error)?;
        let id = SpaceId(self.next_space);
        self.next_space += 1;
        self.spaces.insert(id, space);
        Ok(id)
    }

    fn destroy_address_space(&mut self, space: SpaceId) {
        if let Some(space) = self.spaces.remove(&space)
            && let Err(error) = self.device.destroy_address_space(&space)
        {
            log::warn!("closing an address space: {error}");
        }
    }

    fn map(&mut self, request: &MapRequest) -> Result<(), BackendError> {
        let space = Self::space(&self.spaces, request.space)?;
        let map = rm::device::Mapping {
            memory: handle_of(request.memory),
            offset: request.offset,
            length: request.length,
            address: request.address,
            small_pages: request.small_pages,
            gpu_cached: request.gpu_cached,
            page_kind: self.page_kind_of(request),
        };
        if let Err(error) = self.device.map(space, &map) {
            if map.page_kind.is_none() {
                return Err(memory_error(error));
            }
            // A host that doesn't take the override maps the pages plain, as it did before.
            log::warn!("the RM refused a map with a page kind ({error}), so it is made without");
            self.device.map(space, &rm::device::Mapping { page_kind: None, ..map }).map_err(memory_error)?;
        }
        self.ring_moves.mapped(request.memory, request.address);
        Ok(())
    }

    fn maps_differently(&self, first: u64, second: u64) -> bool {
        page_kind::kinds_differ(self.generation.page_kinds, first, second)
    }

    fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError> {
        let space = Self::space(&self.spaces, unmap.space)?;
        self.device.unmap(space, handle_of(unmap.memory), unmap.address, unmap.length).map_err(memory_error)
    }

    fn describe_allocation(&mut self, private: &[u8]) -> AllocationShape {
        private_data::shape(&self.guest.allocation, private, self.generation.depth_kinds)
    }

    /// The guest's user-mode driver reads a shared primary's data as its kernel driver wrote it, when
    /// a fullscreen program opens the surface on the basic display.
    fn standard_allocation_data(&mut self, surface: &StandardSurface) -> Option<Vec<u8>> {
        standard::data(&self.guest.allocation, surface)
    }

    fn standard_allocation_shape(&mut self, surface: &StandardSurface) -> Option<AllocationShape> {
        standard::shape(surface)
    }

    fn finish_allocation_data(&mut self, private: &mut [u8], size: u64) {
        private_data::finish(&self.guest.allocation, private, size);
        let memory = self.last_memory.take();
        if ring_request::is_ring_request(&self.guest.allocation, private)
            && let Some(memory) = memory
        {
            let handle = self.device.new_handle();
            ring_request::answer_with_handle(private, handle);
            self.ring_moves.answered(memory, handle, size);
            log::info!("a ring of {size:#x} bytes can be moved to with handle {handle:#x}");
        }
    }

    /// Each queue gets a number of its own, which can't be zero, and the guest needs a heap size.
    fn finish_queue_data(&mut self, queue: u32, private: &mut [u8], serial: u32) -> Result<(), BackendError> {
        let too_short = || BackendError::new("a queue's private data is too short");
        match GuestRing::of_queue(private) {
            Some(ring) => {
                self.guest_rings.insert(queue, ring);
            }
            None => grant_list_entries(private).ok_or_else(too_short)?,
        }
        let token = serial.wrapping_add(1).max(1);
        let field = private.get_mut(RECORD_QUEUE_TOKEN..RECORD_QUEUE_TOKEN + 4).ok_or_else(too_short)?;
        field.copy_from_slice(&token.to_le_bytes());
        let field = private.get_mut(RECORD_QUEUE_HEAP..RECORD_QUEUE_HEAP + 8).ok_or_else(too_short)?;
        field.copy_from_slice(&QUEUE_HEAP_SIZE.to_le_bytes());
        Ok(())
    }

    fn kept_gpu_ranges(&self) -> &'static [GpuRange] {
        &[escape::RUNTIME_GPU_RANGE, escape::RUNTIME_PLACED_RANGE, context_room::CONTEXT_BUFFER_ROOM]
    }

    fn guest_restarted(&mut self) {
        self.device.guest_clock.restart();
    }

    fn adapter_opened(&mut self, guest_adapter_id: u64) {
        self.answers.set_adapter_id(guest_adapter_id);
    }

    fn escape(&mut self, process: u32, data: &mut [u8], books: &GuestBooks<'_>) {
        log::debug!("escape {:02x?}, {} bytes", data.get(0x10..0x14), data.len());
        let live = self.sensors.as_ref().and_then(|sensors| sensors.latest());
        let facts =
            escape::EscapeFacts { channel_classes: &self.channel_classes, nodes: &self.nodes, books, live };
        if let Err(error) = escape::answer(&mut self.device, facts, data) {
            log::warn!("an escape failed: {error}");
        }
        if escape::asks_to_encode(data) && self.encode_sessions.ask(&mut self.device, process) {
            escape::allow_encoding(data);
        }
    }

    fn context_made(&mut self, process: u32, node: u32) {
        if self.answers.node_is_encoder(node) {
            self.encode_sessions.context_made(process, node);
        }
    }

    fn context_gone(&mut self, process: u32, node: u32) {
        if self.answers.node_is_encoder(node) {
            self.encode_sessions.context_gone(&mut self.device, process, node);
        }
    }

    fn process_gone(&mut self, process: u32) {
        self.encode_sessions.process_gone(&mut self.device, process);
    }

    fn all_processes_gone(&mut self) {
        self.encode_sessions.all_gone(&mut self.device);
    }

    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
        let live = self.sensors.as_ref().and_then(|sensors| sensors.latest());
        live.and_then(|live| perf_data::answer(query, &live, out)).or_else(|| self.answers.answer(query, out))
    }

    fn registry_value(&mut self, name: &RegistryName) -> Option<RegistryValue> {
        self.answers.registry().lookup(name)
    }

    fn gpu_clock(&mut self) -> Result<u64, BackendError> {
        self.device.gpu_time().map_err(memory_error)
    }

    fn export_memory(&mut self, memory: MemoryId, size: u64) -> Result<OwnedFd, BackendError> {
        self.device.export_dmabuf(handle_of(memory), size).map_err(memory_error)
    }

    /// Only the engines with no channel here are refused, which a guest can fall back from.
    fn context_allowed(&mut self, node: u32, _client_hint: u32) -> bool {
        self.answers.node_has_channel(node) || self.answers.node_is_video(node)
    }

    /// A context on a video node gets a one in its answer. Measured: left at zero, Direct3D 11 still
    /// decodes, and `cuvidCreateDecoder` fails.
    fn finish_context_data(&mut self, node: u32, private: &mut [u8]) {
        if self.answers.node_is_video(node)
            && let Some(field) = private.get_mut(VIDEO_CONTEXT_WORD..VIDEO_CONTEXT_WORD + 4)
        {
            field.copy_from_slice(&1u32.to_le_bytes());
        }
    }

    fn unfinished_work_limit(&self) -> Option<u64> {
        Some(UNFINISHED_ENTRIES_LIMIT)
    }

    fn start_work(&mut self, host: &mut dyn HostServices, work: &Work<'_>) -> WorkStart {
        self.start_work_on_gpu(host, work)
    }

    /// Writing a doorbell can't fail.
    fn ring_doorbell(&mut self, queue: u32, doorbell: u64) -> Result<(), BackendError> {
        self.ring(queue, doorbell);
        Ok(())
    }

    fn close_queue(&mut self, queue: u32) {
        self.close(queue);
    }

    fn start_staging_copy(&mut self, host: &mut dyn HostServices, copy: &StagingCopy) -> StagingStart {
        self.start_copy(host, copy)
    }

    fn poll_staging_copy(&mut self, host: &mut dyn HostServices, now: Instant) -> Option<Status> {
        self.poll_copy(host, now)
    }

    fn start_slot_copy(&mut self, host: &mut dyn HostServices, copy: &SlotCopy) -> StagingStart {
        self.start_frame_copy(host, copy)
    }

    fn copy_pixel_bytes(&self, surface: &SurfaceShape) -> u32 {
        staging::pixel_bytes(surface)
    }

    fn tiled_bytes(&self, surface: &SurfaceShape, x: u64, y: u64) -> Option<u64> {
        if !tiling::origin_fits(x, y) {
            return None;
        }
        tiling::surface_bytes(surface.pitch, surface.height, surface.tiling?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asking(entries: u32) -> Vec<u8> {
        let mut private = vec![0; 0xB4];
        private[QUEUE_LIST_ENTRIES_ASKED..QUEUE_LIST_ENTRIES_ASKED + 4]
            .copy_from_slice(&entries.to_le_bytes());
        private
    }

    fn granted(private: &[u8]) -> u32 {
        u32::from_le_bytes(private[QUEUE_LIST_ENTRIES..QUEUE_LIST_ENTRIES + 4].try_into().unwrap())
    }

    #[test]
    fn a_queue_gets_the_list_length_it_asks_for() {
        let mut private = asking(0x1000);
        assert_eq!(grant_list_entries(&mut private), Some(()));
        assert_eq!(granted(&private), 0x1000);
    }

    #[test]
    fn a_queue_gets_no_longer_lists_than_the_ring_takes() {
        let mut private = asking(u32::MAX);
        assert_eq!(grant_list_entries(&mut private), Some(()));
        assert_eq!(granted(&private), ListChannel::LONGEST_LIST);
    }

    #[test]
    fn private_data_too_short_for_the_answer_is_refused() {
        let mut private = asking(0x1000);
        private.truncate(QUEUE_LIST_ENTRIES + 3);
        assert_eq!(grant_list_entries(&mut private), None);
    }
}
