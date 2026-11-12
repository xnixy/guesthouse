//! A pretend RM, for tests. It keeps a log of every call and answers controls from a table. It never
//! opens a device.

use super::classes::ENGINE_COPY_0;
use super::classes::{CONTROL_GET_ATTACHED_IDS, CONTROL_GET_GPU_ID_INFO, CONTROL_GET_GPU_PCI_INFO, DEVICE};
use super::fake_engine::{ChannelArea, FakeEngine, Mapped, mapping_at};
use super::{
    DmaMap, Handle, Rm, RmError, STATUS_INSUFFICIENT_PERMISSIONS, STATUS_INVALID_ARGUMENT,
    STATUS_NOT_SUPPORTED, STATUS_OK,
};
use crate::channel::{
    CHANNEL_PARAM_CONTROL_MEMORY, CHANNEL_PARAM_ENGINE, CHANNEL_PARAM_RING, CHANNEL_PARAM_RING_ENTRIES,
};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

const FAKE_CLIENT: Handle = 0xC1D0_0001;
const NO_GPU: u32 = u32::MAX;

/// A GPU the pretend RM knows. Its device instance and its device file's number are separate, as on
/// a real PC, where the RM numbers GPUs in the order they were first set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeGpu {
    pub id: u32,
    pub address: guesthouse_backend::PciAddress,
    pub minor: u32,
    pub device_instance: u32,
    /// Set up by the RM, which opening its device file does.
    pub attached: bool,
}

impl FakeGpu {
    /// The only GPU of a one-card PC, at 0000:01:00.0, device file 0 and device instance 0.
    pub fn only() -> Self {
        let address = guesthouse_backend::PciAddress { domain: 0, bus: 1, device: 0, function: 0 };
        Self { id: 0x100, address, minor: 0, device_instance: 0, attached: true }
    }
}

/// One call, as the RM would have seen it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Alloc { parent: Handle, handle: Handle, class: u32, params: Vec<u8> },
    Control { object: Handle, command: u32, params: Vec<u8> },
    Free { parent: Handle, handle: Handle },
    HostPages { handle: Handle, size: u64 },
    MapDma(DmaMap),
    UnmapDma { range: Handle, memory: Handle, address: u64, length: u64 },
    ExportDmabuf { memory: Handle, size: u64 },
    MapDoorbell,
    Doorbell { token: u32 },
}

type Answer = Box<dyn Fn(&mut [u8]) -> u32 + Send>;

/// See the module documentation.
pub struct FakeRm {
    pub calls: Vec<Call>,
    /// Sizes of video memory round up to this, as a real one rounds to its pages.
    pub round_memory_to: u64,
    pub refuse_class: Option<u32>,
    pub refuse_control: Option<u32>,
    pub refuse_exports: bool,
    /// A GPU address no mapping may start at.
    pub refuse_map_at: Option<u64>,
    /// Set to refuse every map that carries a page kind, as an RM that does not take the override.
    pub refuse_page_kind: bool,
    /// What each export handed out, so a test can look at what a viewer would see.
    pub exports: HashMap<Handle, File>,
    /// What video memory starts with, for a copy out to host pages to find.
    pub memory_starts: HashMap<Handle, Vec<u8>>,
    /// How many of the engine's copies have had that done.
    copies_looked_at: usize,
    pub engine: FakeEngine,
    /// What [`Rm::read_gpu_timer`] says. `None` plays a page that isn't mapped yet.
    pub gpu_timer: Option<u64>,
    answers: HashMap<u32, Answer>,
    /// Each range's mappings, address to length, to refuse a map over a map.
    mappings: BTreeMap<(Handle, u64), u64>,
    mapped: Mapped,
    host_pages: HashMap<Handle, u64>,
    next_token: u32,
    /// What the RM has, and which device file this client opened.
    pub gpus: Vec<FakeGpu>,
    pub opened_minor: u32,
}

impl Default for FakeRm {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRm {
    pub fn new() -> Self {
        Self {
            calls: Vec::new(),
            round_memory_to: 1,
            refuse_class: None,
            refuse_control: None,
            refuse_exports: false,
            refuse_map_at: None,
            refuse_page_kind: false,
            exports: HashMap::new(),
            memory_starts: HashMap::new(),
            copies_looked_at: 0,
            engine: FakeEngine::default(),
            gpu_timer: None,
            answers: HashMap::new(),
            mappings: BTreeMap::new(),
            mapped: Mapped::new(),
            host_pages: HashMap::new(),
            next_token: 0x100,
            gpus: vec![FakeGpu::only()],
            opened_minor: 0,
        }
    }

    /// A pretend RM with these GPUs, before any device file is open.
    pub fn with_gpus(gpus: Vec<FakeGpu>) -> Self {
        Self { gpus, opened_minor: NO_GPU, ..Self::new() }
    }

    /// Plays opening a GPU's device file, which attaches it if it wasn't.
    pub fn open_device_file(&mut self, minor: u32) {
        self.opened_minor = minor;
        for gpu in self.gpus.iter_mut().filter(|gpu| gpu.minor == minor) {
            gpu.attached = true;
        }
    }

    fn attached_gpu(&self, id: u32) -> Option<&FakeGpu> {
        self.gpus.iter().find(|gpu| gpu.attached && gpu.id == id)
    }

    /// The controls on the client that say which GPU is where.
    fn answer_gpu_control(&self, command: u32, params: &mut [u8]) -> Option<u32> {
        let put = |params: &mut [u8], at: usize, value: u32| {
            params[at..at + 4].copy_from_slice(&value.to_le_bytes())
        };
        let asked = || u32::from_le_bytes(params[..4].try_into().expect("four bytes"));
        match command {
            CONTROL_GET_ATTACHED_IDS => {
                params.fill(0xFF);
                for (index, gpu) in self.gpus.iter().filter(|gpu| gpu.attached).enumerate() {
                    put(params, index * 4, gpu.id);
                }
                Some(STATUS_OK)
            }
            CONTROL_GET_GPU_PCI_INFO => Some(match self.attached_gpu(asked()) {
                Some(gpu) => {
                    let address = gpu.address;
                    put(params, 4, address.domain);
                    params[8..10].copy_from_slice(&u16::from(address.bus).to_le_bytes());
                    params[10..12].copy_from_slice(&u16::from(address.device).to_le_bytes());
                    STATUS_OK
                }
                None => STATUS_INVALID_ARGUMENT,
            }),
            CONTROL_GET_GPU_ID_INFO => Some(match self.attached_gpu(asked()) {
                Some(gpu) => {
                    put(params, 8, gpu.device_instance);
                    STATUS_OK
                }
                None => STATUS_INVALID_ARGUMENT,
            }),
            _ => None,
        }
    }

    /// The RM's answer to a device with this instance: refused unless this client opened its GPU.
    fn device_status(&self, instance: u32) -> u32 {
        match self.gpus.iter().find(|gpu| gpu.attached && gpu.device_instance == instance) {
            Some(gpu) if gpu.minor == self.opened_minor => STATUS_OK,
            Some(_) => STATUS_INSUFFICIENT_PERMISSIONS,
            None => STATUS_INVALID_ARGUMENT,
        }
    }

    /// Answers a control. Controls with no answer get "not supported".
    pub fn answer(&mut self, command: u32, answer: impl Fn(&mut [u8]) -> u32 + Send + 'static) {
        self.answers.insert(command, Box::new(answer));
    }

    /// (class, params) of everything made, in order.
    pub fn allocated(&self) -> impl Iterator<Item = (u32, &[u8])> {
        self.calls.iter().filter_map(|call| match call {
            Call::Alloc { class, params, .. } => Some((*class, params.as_slice())),
            _ => None,
        })
    }

    /// Tokens rung so far.
    pub fn doorbells(&self) -> Vec<u32> {
        let rung = self.calls.iter().filter_map(|call| match call {
            Call::Doorbell { token } => Some(*token),
            _ => None,
        });
        rung.collect()
    }

    /// The memory mapped at a GPU address.
    pub fn memory_at(&self, address: u64) -> Option<Handle> {
        mapping_at(&self.mapped, address).map(|(_, memory, _)| memory)
    }

    /// Does what the engine's new copies mean for host pages, which only this can reach.
    fn copy_starts_to_host_pages(&mut self) {
        for copy in &self.engine.copies[self.copies_looked_at..] {
            let source = mapping_at(&self.mapped, copy.source).map(|(_, memory, _)| memory);
            let target = mapping_at(&self.mapped, copy.target).map(|(_, memory, _)| memory);
            let start = source.and_then(|memory| self.memory_starts.get(&memory));
            let page = target.and_then(|memory| self.host_pages.get(&memory));
            if let (Some(start), Some(&page)) = (start, page) {
                // SAFETY: a test keeps the page mapped while its copy runs, and a start is smaller.
                unsafe { std::ptr::copy_nonoverlapping(start.as_ptr(), page as *mut u8, start.len()) };
            }
        }
        self.copies_looked_at = self.engine.copies.len();
    }

    /// Runs what every copy channel has been given, as a card that was stuck and has come back.
    pub fn run_engine(&mut self) {
        self.engine.running = true;
        for token in self.engine.channels.keys().copied().collect::<Vec<_>>() {
            self.engine.run(token, &self.mapped, &self.exports);
        }
    }

    /// Tells the engine about a new copy channel, whose ring is in its own host area.
    fn note_channel(&mut self, channel: Handle, token: u32) {
        let found = self.calls.iter().find_map(|call| match call {
            Call::Alloc { handle, params, .. } if *handle == channel => Some(params.as_slice()),
            _ => None,
        });
        let Some(params) = found else { return };
        let word = |at: usize| u32::from_le_bytes(params[at..at + 4].try_into().expect("four bytes"));
        let ring = u64::from_le_bytes(params[CHANNEL_PARAM_RING..][..8].try_into().expect("eight bytes"));
        let memory = word(CHANNEL_PARAM_CONTROL_MEMORY);
        let ring_entries = word(CHANNEL_PARAM_RING_ENTRIES);
        let is_ours = |(_, (mapped, _)): &(&u64, &(Handle, u64))| *mapped == memory;
        let gpu_address = self.mapped.iter().find(is_ours).map(|(address, _)| *address);
        if word(CHANNEL_PARAM_ENGINE) == ENGINE_COPY_0
            && gpu_address == Some(ring)
            && let Some(&host_address) = self.host_pages.get(&memory)
        {
            let area = ChannelArea { host_address, gpu_address: ring, ring_entries };
            self.engine.channels.insert(token, area);
        }
    }

    /// A file in memory stands in for a dmabuf.
    fn new_export(size: u64) -> io::Result<File> {
        // SAFETY: the name is a valid string, and a new descriptor is ours alone.
        let file = unsafe {
            let descriptor = libc::memfd_create(c"fake-dmabuf".as_ptr(), libc::MFD_CLOEXEC);
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            File::from_raw_fd(descriptor)
        };
        file.set_len(size)?;
        Ok(file)
    }

    fn overlaps(&self, range: Handle, address: u64, length: u64) -> bool {
        let before = self.mappings.range((range, 0)..(range, address.saturating_add(length))).next_back();
        before.is_some_and(|(&(_, start), &size)| start.saturating_add(size) > address)
    }
}

impl Rm for FakeRm {
    fn new_client(&mut self) -> Result<Handle, RmError> {
        Ok(FAKE_CLIENT)
    }

    fn alloc(
        &mut self,
        parent: Handle,
        handle: Handle,
        class: u32,
        params: &mut [u8],
    ) -> Result<u32, RmError> {
        if self.refuse_class == Some(class) {
            return Ok(STATUS_NOT_SUPPORTED);
        }
        self.calls.push(Call::Alloc { parent, handle, class, params: params.to_vec() });
        if class == DEVICE {
            let status = self.device_status(u32::from_le_bytes(params[..4].try_into().expect("four bytes")));
            if status != STATUS_OK {
                return Ok(status);
            }
        }
        if class == super::classes::MEMORY_LOCAL_USER {
            let size = u64::from_le_bytes(params[0x40..0x48].try_into().expect("eight bytes"));
            let rounded = size.next_multiple_of(self.round_memory_to);
            params[0x40..0x48].copy_from_slice(&rounded.to_le_bytes());
        }
        Ok(STATUS_OK)
    }

    fn control(&mut self, object: Handle, command: u32, params: &mut [u8]) -> Result<u32, RmError> {
        self.calls.push(Call::Control { object, command, params: params.to_vec() });
        if self.refuse_control == Some(command) {
            return Ok(STATUS_NOT_SUPPORTED);
        }
        if let Some(status) = self.answer_gpu_control(command, params) {
            return Ok(status);
        }
        if command == super::classes::CONTROL_GET_WORK_SUBMIT_TOKEN {
            self.next_token += 1;
            params[..4].copy_from_slice(&self.next_token.to_le_bytes());
            self.note_channel(object, self.next_token);
            return Ok(STATUS_OK);
        }
        if command == super::classes::CONTROL_SCHEDULE_CHANNEL_GROUP
            || command == super::classes::CONTROL_DEFERRED_API
        {
            return Ok(STATUS_OK);
        }
        if command == super::classes::CONTROL_GET_CLASS_ENGINE {
            let asked = u32::from_le_bytes(params[..4].try_into().expect("four bytes"));
            let class = self.calls.iter().find_map(|call| match call {
                Call::Alloc { handle, class, .. } if *handle == asked => Some(*class),
                _ => None,
            });
            params[4..8].copy_from_slice(&class.unwrap_or(0).to_le_bytes());
            return Ok(STATUS_OK);
        }
        Ok(self.answers.get(&command).map_or(STATUS_NOT_SUPPORTED, |answer| answer(params)))
    }

    fn free(&mut self, parent: Handle, handle: Handle) -> Result<(), RmError> {
        self.calls.push(Call::Free { parent, handle });
        Ok(())
    }

    fn alloc_host_pages(
        &mut self,
        _device: Handle,
        handle: Handle,
        address: u64,
        size: u64,
    ) -> Result<(), RmError> {
        self.host_pages.insert(handle, address);
        self.calls.push(Call::HostPages { handle, size });
        Ok(())
    }

    fn map_dma(&mut self, map: &DmaMap) -> Result<(), RmError> {
        let refused =
            self.refuse_map_at == Some(map.address) || self.refuse_page_kind && map.page_kind.is_some();
        if self.overlaps(map.range, map.address, map.length) || refused {
            return Err(RmError::refused(format!("map at {:#x}", map.address), STATUS_NOT_SUPPORTED));
        }
        self.mappings.insert((map.range, map.address), map.length);
        self.mapped.insert(map.address, (map.memory, map.length));
        self.calls.push(Call::MapDma(*map));
        Ok(())
    }

    fn unmap_dma(
        &mut self,
        _device: Handle,
        range: Handle,
        memory: Handle,
        address: u64,
        length: u64,
    ) -> Result<(), RmError> {
        self.mappings.remove(&(range, address));
        self.mapped.remove(&address);
        self.calls.push(Call::UnmapDma { range, memory, address, length });
        Ok(())
    }

    fn export_dmabuf(&mut self, _device: Handle, memory: Handle, size: u64) -> Result<OwnedFd, RmError> {
        if self.refuse_exports {
            return Err(RmError::refused("export", STATUS_NOT_SUPPORTED));
        }
        self.calls.push(Call::ExportDmabuf { memory, size });
        let system = |error: io::Error| RmError::System(error.to_string());
        let file = Self::new_export(size).map_err(system)?;
        let shared = file.try_clone().map_err(system)?;
        self.exports.insert(memory, file);
        Ok(shared.into())
    }

    fn map_doorbell(&mut self, _subdevice: Handle, _usermode: Handle) -> Result<(), RmError> {
        self.calls.push(Call::MapDoorbell);
        Ok(())
    }

    fn ring_doorbell(&mut self, token: u32) {
        self.calls.push(Call::Doorbell { token });
        if self.engine.running {
            self.engine.run(token, &self.mapped, &self.exports);
            self.copy_starts_to_host_pages();
        }
    }

    fn read_gpu_timer(&self) -> Option<u64> {
        self.gpu_timer
    }
}
