//! An open AMD render node and a CPU view of a buffer on it.

use crate::info::{self, DeviceInfo};
use crate::ioctl;
use crate::memory::GemHandle;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::ptr;

/// An open render node, with what was read off the card at open time. amdgpu gives each open file
/// its own GPU address space, so a guest process's space is a second open, see [`Device::open_again`].
#[derive(Debug)]
pub struct Device {
    file: File,
    info: DeviceInfo,
    folder: Option<PathBuf>,
    node: PathBuf,
}

impl Device {
    /// Opens the AMD card, by the vendor id on its PCI device, or the one at `address` if given.
    pub fn open(address: Option<&str>) -> io::Result<Self> {
        let node = ioctl::amd_render_node(address)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no AMD render node found"))?;
        let file = OpenOptions::new().read(true).write(true).open(&node)?;
        let folder = ioctl::device_folder(&node);
        let info = info::read_device_info(&file, folder.as_deref())?;
        Ok(Self { file, info, folder, node })
    }

    /// The same render node opened anew, with a GPU address space of its own.
    pub fn open_again(&self) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(&self.node)
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    /// The card's PCI device folder in sysfs, where amdgpu reports what it knows outside INFO.
    pub fn device_folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    /// The GPU's clock in nanoseconds.
    pub fn gpu_clock(&self) -> io::Result<u64> {
        info::read_gpu_clock(&self.file, self.info.counter_freq_khz)
    }

    /// Maps a buffer's bytes into this process so the CPU can read what the GPU wrote, or fill an IB.
    pub fn map_cpu(&self, handle: GemHandle, size: usize) -> io::Result<Mapping> {
        Mapping::of(&self.file, handle, size)
    }
}

impl AsRawFd for Device {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// A CPU mapping of a buffer object. It reads and writes 32-bit words, which is all the harness and
/// the completion readback need.
#[derive(Debug)]
pub struct Mapping {
    address: *mut u8,
    size: usize,
}

impl Mapping {
    /// Maps `size` bytes of a buffer that is a handle on `file`, any open of the render node.
    pub fn of(file: &File, handle: GemHandle, size: usize) -> io::Result<Self> {
        let offset = crate::memory::mmap_offset(file, handle)?;
        // SAFETY: a shared mapping of `size` bytes at the kernel-provided offset of this node.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { address: address.cast(), size })
    }

    pub fn write_u32(&mut self, offset: usize, value: u32) {
        assert!(offset + 4 <= self.size, "write past the mapping");
        // SAFETY: the offset is inside the mapping, which is writable.
        unsafe { ptr::write_volatile(self.address.add(offset).cast::<u32>(), value) };
    }

    pub fn read_u32(&self, offset: usize) -> u32 {
        assert!(offset + 4 <= self.size, "read past the mapping");
        // SAFETY: the offset is inside the mapping, which is readable.
        unsafe { ptr::read_volatile(self.address.add(offset).cast::<u32>()) }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.address
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Fills the start of the mapping with dwords, for writing an IB.
    pub fn write_dwords(&mut self, dwords: &[u32]) {
        for (index, &word) in dwords.iter().enumerate() {
            self.write_u32(index * 4, word);
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping was made here with this address and size.
        unsafe { libc::munmap(self.address.cast(), self.size) };
    }
}
