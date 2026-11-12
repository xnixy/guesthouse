//! The real RM, through the ioctls of NVIDIA's Linux kernel module.
//!
//! Each request is a plain structure with a fixed layout, written here by offset. The numbers and
//! layouts are in the public headers of the open kernel module. Sizes that change with a driver
//! branch come from [`HostBranch`].

use super::classes::{CONTROL_EXPORT_OBJECT_TO_FD, ROOT_CLIENT};
use super::listing::CardPlace;
use super::{DmaMap, Handle, Rm, RmError, dmabuf};
use crate::profile::HostBranch;
use guesthouse_backend::PciAddress;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

const CONTROL_DEVICE: &str = "/dev/nvidiactl";
/// Each card's device file is this with its number on the end.
const GPU_DEVICE_PREFIX: &str = "/dev/nvidia";

const IOCTL_READ_WRITE: u64 = 3 << 30;
const IOCTL_SIZE_SHIFT: u32 = 16;
const IOCTL_MAGIC: u64 = b'F' as u64;

const ESCAPE_FREE: u64 = 0x29;
const ESCAPE_CONTROL: u64 = 0x2A;
const ESCAPE_ALLOC: u64 = 0x2B;
const ESCAPE_ALLOC_MEMORY: u64 = 0x27;
const ESCAPE_MAP_MEMORY: u64 = 0x4E;
const ESCAPE_MAP_MEMORY_DMA: u64 = 0x57;
const ESCAPE_UNMAP_MEMORY_DMA: u64 = 0x58;
const ESCAPE_REGISTER_FD: u64 = 200 + 1;
const ESCAPE_CHECK_VERSION: u64 = 200 + 10;

const VERSION_REQUEST_SIZE: usize = 72;
const VERSION_STRING_AT: usize = 8;
const VERSION_COMMAND_STRICT: u32 = 0;

const CLIENT_PARAMS_NAME_AT: usize = 8;
const CLIENT_NAME: &[u8] = b"guesthouse";

const MEMORY_OS_DESCRIPTOR: u32 = 0x71;
/// Non-contiguous, reached over PCI, cached, with no memory of its own, and not mapped by the kernel.
const OS_DESCRIPTOR_FLAGS: u32 = 1 << 4 | 1 << 12 | 1 << 16 | 0x4000_0000;

const MAP_FLAG_FIXED_ADDRESS: u32 = 1 << 15;
const MAP_FLAG_SMALL_PAGES: u32 = 1 << 8;
/// The guest maps host pages cached, so the GPU has to snoop. Video memory ignores this.
const MAP_FLAG_SNOOP: u32 = 1 << 4;
const MAP_FLAG_GPU_CACHED: u32 = 1 << 17;
/// Takes the page kind from the request's kind field, not from the memory.
const MAP_FLAG_PAGE_KIND_OVERRIDE: u32 = 1 << 19;

const DOORBELL_MAP_SIZE: usize = 0x10000;
/// Where the doorbell register is in the page.
const DOORBELL_REGISTER: usize = 0x90;
/// The GPU's timer, in the same page: `NVC361_TIME_0` and `NVC361_TIME_1` in `clc361.h`.
const TIMER_LOW_REGISTER: usize = 0x80;
const TIMER_HIGH_REGISTER: usize = 0x84;
/// The low word rolls over about every four seconds, so a second try can only fail if something
/// has gone wrong.
const TIMER_READ_TRIES: usize = 3;

fn request_number(escape: u64, size: usize) -> u64 {
    IOCTL_READ_WRITE | (size as u64) << IOCTL_SIZE_SHIFT | IOCTL_MAGIC << 8 | escape
}

fn system(what: &str, error: io::Error) -> RmError {
    RmError::System(format!("{what}: {error}"))
}

fn ioctl(file: &File, escape: u64, request: &mut [u8]) -> Result<(), RmError> {
    let number = request_number(escape, request.len());
    // SAFETY: the request is a live buffer of the size the number says, and the kernel writes
    // only inside it.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), number as _, request.as_mut_ptr()) };
    if result < 0 { Err(system("ioctl", io::Error::last_os_error())) } else { Ok(()) }
}

/// A DRM ioctl, tried again if a signal cuts it short, as DRM's own library does.
fn drm_ioctl(file: &File, number: u64, request: &mut [u8]) -> Result<(), RmError> {
    loop {
        // SAFETY: the request is a live buffer of the size the number says, and the kernel writes
        // only inside it.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), number as _, request.as_mut_ptr()) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(system("DRM ioctl", error));
        }
    }
}

fn put32(request: &mut [u8], at: usize, value: u32) {
    request[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(request: &mut [u8], at: usize, value: u64) {
    request[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// The bytes of a request to map memory into a GPU address range.
fn map_dma_request(sizes: &HostBranch, client: Handle, map: &DmaMap) -> Vec<u8> {
    let mut flags = MAP_FLAG_FIXED_ADDRESS | MAP_FLAG_SNOOP;
    if map.small_pages {
        flags |= MAP_FLAG_SMALL_PAGES;
    }
    if map.gpu_cached {
        flags |= MAP_FLAG_GPU_CACHED;
    }
    let mut request = vec![0; sizes.map_dma_request];
    if let Some(kind) = map.page_kind {
        flags |= MAP_FLAG_PAGE_KIND_OVERRIDE;
        put32(&mut request, sizes.map_dma_request_kind, kind);
    }
    put32(&mut request, 0, client);
    put32(&mut request, 4, map.device);
    put32(&mut request, 8, map.range);
    put32(&mut request, 12, map.memory);
    put64(&mut request, 16, map.offset);
    put64(&mut request, 24, map.length);
    put32(&mut request, 32, flags);
    put64(&mut request, sizes.map_dma_request_address, map.address);
    request
}

fn get32(request: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(request[at..at + 4].try_into().expect("four bytes"))
}

/// Parameters where the RM will take them: at a real, aligned address, even when there are none. An
/// empty slice's pointer is neither, and the RM refuses it.
struct AlignedParams {
    words: Vec<u64>,
}

impl AlignedParams {
    fn new(params: &[u8]) -> Self {
        let mut words = vec![0; params.len().div_ceil(8).max(1)];
        for (word, bytes) in words.iter_mut().zip(params.chunks(8)) {
            let mut padded = [0; 8];
            padded[..bytes.len()].copy_from_slice(bytes);
            *word = u64::from_le_bytes(padded);
        }
        Self { words }
    }

    fn address(&mut self) -> u64 {
        self.words.as_mut_ptr() as u64
    }

    fn copy_back(&self, params: &mut [u8]) {
        for (bytes, word) in params.chunks_mut(8).zip(&self.words) {
            bytes.copy_from_slice(&word.to_le_bytes()[..bytes.len()]);
        }
    }
}

/// See the module documentation.
pub struct LinuxRm {
    control_file: File,
    gpu_file: File,
    gpu_path: String,
    /// Where the card is, for finding its render node.
    address: Option<PciAddress>,
    client: Handle,
    sizes: &'static HostBranch,
    doorbell: *mut u8,
    /// Opened when the first frame buffer is exported, as a guest with no viewer never needs it.
    render_node: Option<File>,
}

impl LinuxRm {
    /// Opens the control device and the card's device file, and tells the kernel module which version
    /// this client was written for. Call this only once the start decision has passed.
    pub fn open(version: &str, sizes: &'static HostBranch, place: &CardPlace) -> Result<Self, RmError> {
        let mut rm = Self::open_card(version, sizes, place.minor)?;
        rm.address = place.address;
        Ok(rm)
    }

    /// The same for the card with this device file number, which its information file in
    /// `/proc/driver/nvidia/gpus` calls the minor. The RM's device instance for it is another number:
    /// see [`super::instance`].
    pub fn open_card(version: &str, sizes: &'static HostBranch, card: u32) -> Result<Self, RmError> {
        let open = |path: &str| {
            let file = OpenOptions::new().read(true).write(true).open(path);
            file.map_err(|error| system(path, error))
        };
        let control_file = open(CONTROL_DEVICE)?;
        let gpu_path = format!("{GPU_DEVICE_PREFIX}{card}");
        let gpu_file = open(&gpu_path)?;
        let mut request = [0; VERSION_REQUEST_SIZE];
        put32(&mut request, 0, VERSION_COMMAND_STRICT);
        let text = version.as_bytes();
        request[VERSION_STRING_AT..VERSION_STRING_AT + text.len()].copy_from_slice(text);
        ioctl(&control_file, ESCAPE_CHECK_VERSION, &mut request)?;
        let rm = Self {
            control_file,
            gpu_file,
            gpu_path,
            address: None,
            client: 0,
            sizes,
            doorbell: ptr::null_mut(),
            render_node: None,
        };
        rm.register(&rm.gpu_file)?;
        Ok(rm)
    }

    /// RM clients belong to the control file, so a GPU file has to be tied to it.
    fn register(&self, file: &File) -> Result<(), RmError> {
        let mut request = self.control_file.as_raw_fd().to_le_bytes();
        ioctl(file, ESCAPE_REGISTER_FD, &mut request)
    }

    fn open_render_node(&mut self) -> Result<&File, RmError> {
        if self.render_node.is_none() {
            let no_node = || RmError::System("this card has no render node".to_owned());
            let path = dmabuf::render_node(self.address.as_ref()).ok_or_else(no_node)?;
            let file = OpenOptions::new().read(true).write(true).open(&path);
            self.render_node = Some(file.map_err(|error| system("render node", error))?);
        }
        Ok(self.render_node.as_ref().expect("opened just above"))
    }

    fn raw_alloc(
        &mut self,
        root: Handle,
        parent: Handle,
        handle: Handle,
        class: u32,
        params: &mut [u8],
    ) -> Result<(u32, Handle), RmError> {
        let sizes = self.sizes;
        let mut aligned = AlignedParams::new(params);
        let mut request = vec![0; sizes.alloc_request];
        put32(&mut request, 0, root);
        put32(&mut request, 4, parent);
        put32(&mut request, 8, handle);
        put32(&mut request, 12, class);
        put64(&mut request, 16, aligned.address());
        put32(&mut request, 24, params.len() as u32);
        ioctl(&self.control_file, ESCAPE_ALLOC, &mut request)?;
        aligned.copy_back(params);
        Ok((get32(&request, sizes.alloc_request_status), get32(&request, 8)))
    }
}

impl Rm for LinuxRm {
    fn new_client(&mut self) -> Result<Handle, RmError> {
        let mut params = vec![0; self.sizes.client_params];
        put32(&mut params, 4, std::process::id());
        params[CLIENT_PARAMS_NAME_AT..CLIENT_PARAMS_NAME_AT + CLIENT_NAME.len()].copy_from_slice(CLIENT_NAME);
        let (status, client) = self.raw_alloc(0, 0, 0, ROOT_CLIENT, &mut params)?;
        RmError::check(|| "client".to_owned(), status)?;
        self.client = client;
        Ok(client)
    }

    fn alloc(
        &mut self,
        parent: Handle,
        handle: Handle,
        class: u32,
        params: &mut [u8],
    ) -> Result<u32, RmError> {
        Ok(self.raw_alloc(self.client, parent, handle, class, params)?.0)
    }

    fn control(&mut self, object: Handle, command: u32, params: &mut [u8]) -> Result<u32, RmError> {
        let sizes = self.sizes;
        let mut aligned = AlignedParams::new(params);
        let mut request = vec![0; sizes.control_request];
        put32(&mut request, 0, self.client);
        put32(&mut request, 4, object);
        put32(&mut request, 8, command);
        put64(&mut request, 16, aligned.address());
        put32(&mut request, 24, params.len() as u32);
        ioctl(&self.control_file, ESCAPE_CONTROL, &mut request)?;
        aligned.copy_back(params);
        Ok(get32(&request, sizes.control_request_status))
    }

    fn free(&mut self, parent: Handle, handle: Handle) -> Result<(), RmError> {
        let mut request = vec![0; self.sizes.free_request];
        put32(&mut request, 0, self.client);
        put32(&mut request, 4, parent);
        put32(&mut request, 8, handle);
        ioctl(&self.control_file, ESCAPE_FREE, &mut request)?;
        let status = get32(&request, self.sizes.free_request_status);
        RmError::check(|| format!("free {handle:#x}"), status)
    }

    fn alloc_host_pages(
        &mut self,
        device: Handle,
        handle: Handle,
        address: u64,
        size: u64,
    ) -> Result<(), RmError> {
        let sizes = self.sizes;
        let mut request = vec![0; sizes.alloc_memory_request];
        put32(&mut request, 0, self.client);
        put32(&mut request, 4, device);
        put32(&mut request, 8, handle);
        put32(&mut request, 12, MEMORY_OS_DESCRIPTOR);
        put32(&mut request, 16, OS_DESCRIPTOR_FLAGS);
        put64(&mut request, 24, address);
        put64(&mut request, 32, size - 1);
        put32(&mut request, sizes.alloc_memory_request_fd, u32::MAX);
        ioctl(&self.gpu_file, ESCAPE_ALLOC_MEMORY, &mut request)?;
        let status = get32(&request, sizes.alloc_memory_request_status);
        RmError::check(|| format!("host pages at {address:#x}"), status)
    }

    fn map_dma(&mut self, map: &DmaMap) -> Result<(), RmError> {
        let sizes = self.sizes;
        let mut request = map_dma_request(sizes, self.client, map);
        ioctl(&self.control_file, ESCAPE_MAP_MEMORY_DMA, &mut request)?;
        let status = get32(&request, sizes.map_dma_request_status);
        RmError::check(|| format!("map at {:#x}", map.address), status)
    }

    fn unmap_dma(
        &mut self,
        device: Handle,
        range: Handle,
        memory: Handle,
        address: u64,
        length: u64,
    ) -> Result<(), RmError> {
        let sizes = self.sizes;
        let mut request = vec![0; sizes.unmap_dma_request];
        put32(&mut request, 0, self.client);
        put32(&mut request, 4, device);
        put32(&mut request, 8, range);
        put32(&mut request, 12, memory);
        put64(&mut request, 24, address);
        put64(&mut request, 32, length);
        ioctl(&self.control_file, ESCAPE_UNMAP_MEMORY_DMA, &mut request)?;
        let status = get32(&request, sizes.unmap_dma_request_status);
        RmError::check(|| format!("unmap {address:#x}"), status)
    }

    fn export_dmabuf(&mut self, device: Handle, memory: Handle, size: u64) -> Result<OwnedFd, RmError> {
        let sizes = self.sizes;
        // The memory's file is only a step on the way, and the GEM object holds what it needs.
        let open = OpenOptions::new().read(true).write(true).open(CONTROL_DEVICE);
        let export = open.map_err(|error| system(CONTROL_DEVICE, error))?;
        let mut params = dmabuf::export_params(sizes, device, memory, export.as_raw_fd());
        let status = self.control(self.client, CONTROL_EXPORT_OBJECT_TO_FD, &mut params)?;
        RmError::check(|| format!("export of {memory:#x}"), status)?;

        let render = self.open_render_node()?;
        let params = dmabuf::import_params(sizes, export.as_raw_fd());
        let mut request = dmabuf::import_request(sizes, size, &params);
        drm_ioctl(render, dmabuf::import_number(sizes), &mut request)?;
        let handle = dmabuf::imported_handle(sizes, &request);

        let mut prime = dmabuf::prime_request(handle);
        let made = drm_ioctl(render, dmabuf::prime_number(), &mut prime);
        // The dmabuf keeps what it needs, and this handle has done its job either way.
        let closed = drm_ioctl(render, dmabuf::gem_close_number(), &mut dmabuf::gem_close_request(handle));
        if let Err(error) = closed {
            log::warn!("closing a GEM handle: {error}");
        }
        made?;
        // SAFETY: the kernel has just made this descriptor for us, and nothing else holds it.
        Ok(unsafe { OwnedFd::from_raw_fd(dmabuf::prime_descriptor(&prime)) })
    }

    fn map_doorbell(&mut self, subdevice: Handle, usermode: Handle) -> Result<(), RmError> {
        let sizes = self.sizes;
        let path = self.gpu_path.as_str();
        let file = OpenOptions::new().read(true).write(true).open(path);
        let file = file.map_err(|error| system(path, error))?;
        self.register(&file)?;
        let mut request = vec![0; sizes.map_memory_request];
        put32(&mut request, 0, self.client);
        put32(&mut request, 4, subdevice);
        put32(&mut request, 8, usermode);
        put64(&mut request, 24, DOORBELL_MAP_SIZE as u64);
        put32(&mut request, sizes.map_memory_request_fd, file.as_raw_fd() as u32);
        ioctl(&self.control_file, ESCAPE_MAP_MEMORY, &mut request)?;
        let status = get32(&request, sizes.map_memory_request_status);
        RmError::check(|| "doorbell map".to_owned(), status)?;
        let protection = libc::PROT_READ | libc::PROT_WRITE;
        // SAFETY: the kernel picks the address, and the mapping stays until the process ends.
        let page = unsafe {
            libc::mmap(ptr::null_mut(), DOORBELL_MAP_SIZE, protection, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        if page == libc::MAP_FAILED {
            return Err(system("doorbell mmap", io::Error::last_os_error()));
        }
        self.doorbell = page.cast();
        Ok(())
    }

    fn ring_doorbell(&mut self, token: u32) {
        assert!(!self.doorbell.is_null(), "the doorbell page is not mapped yet");
        // SAFETY: the page is mapped for the life of the process, and the register is inside it.
        unsafe { ptr::write_volatile(self.doorbell.add(DOORBELL_REGISTER).cast::<u32>(), token) };
    }

    fn read_gpu_timer(&self) -> Option<u64> {
        if self.doorbell.is_null() {
            return None;
        }
        // SAFETY: the page is mapped for the life of the process, and both registers are inside it.
        let read = |at: usize| unsafe { ptr::read_volatile(self.doorbell.add(at).cast::<u32>()) };
        (0..TIMER_READ_TRIES).find_map(|_| {
            let high = read(TIMER_HIGH_REGISTER);
            let low = read(TIMER_LOW_REGISTER);
            (read(TIMER_HIGH_REGISTER) == high).then_some(u64::from(high) << 32 | u64::from(low))
        })
    }
}

impl Drop for LinuxRm {
    fn drop(&mut self) {
        if !self.doorbell.is_null() {
            // SAFETY: this is the mapping made in `map_doorbell`, and nothing can ring it now.
            unsafe { libc::munmap(self.doorbell.cast::<c_void>(), DOORBELL_MAP_SIZE) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_come_back_changed_and_any_length_has_an_address() {
        let mut params = [1, 2, 3];
        let mut aligned = AlignedParams::new(&params);
        assert_eq!(aligned.address() % 8, 0);
        aligned.words[0] = 0x0706_0504_0302_0100;
        aligned.copy_back(&mut params);
        assert_eq!(params, [0, 1, 2]);
        assert_ne!(AlignedParams::new(&[]).address(), 0);
    }

    #[test]
    fn request_numbers_follow_the_kernels_encoding() {
        assert_eq!(request_number(ESCAPE_ALLOC, 32), 0xC020_462B);
        assert_eq!(request_number(ESCAPE_REGISTER_FD, 4), 0xC004_46C9);
    }

    fn a_map() -> DmaMap {
        DmaMap {
            device: 1,
            range: 2,
            memory: 3,
            offset: 0x1000,
            length: 0x2000,
            address: 0x5000_0000,
            small_pages: true,
            gpu_cached: false,
            page_kind: None,
        }
    }

    fn flags_of(request: &[u8]) -> u32 {
        get32(request, 32)
    }

    #[test]
    fn a_map_with_a_page_kind_overrides_the_memorys() {
        let sizes = &crate::profile::HOST_615;
        let request = map_dma_request(sizes, 9, &DmaMap { page_kind: Some(0x05), ..a_map() });
        assert_ne!(flags_of(&request) & MAP_FLAG_PAGE_KIND_OVERRIDE, 0);
        assert_eq!(get32(&request, sizes.map_dma_request_kind), 0x05);
        assert_eq!(get32(&request, 0), 9);
        assert_eq!(request.len(), sizes.map_dma_request);
    }

    #[test]
    fn a_map_with_no_page_kind_leaves_the_field_and_flag_clear() {
        let sizes = &crate::profile::HOST_615;
        let request = map_dma_request(sizes, 9, &a_map());
        assert_eq!(flags_of(&request) & MAP_FLAG_PAGE_KIND_OVERRIDE, 0);
        assert_eq!(get32(&request, sizes.map_dma_request_kind), 0);
    }
}
