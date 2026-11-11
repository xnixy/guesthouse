//! The guest's memory, from the memfds the app hands QEMU and this daemon.

use crate::handover::Part;
use crate::slot::Slot;
use guesthouse_backend::{GuestMemory, HostBuffer, MemoryError, PagesView, in_window};
pub use guesthouse_backend::{WINDOW_BASE, window_base};
use guesthouse_frame_socket::sized_buffer;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// With this much RAM, QEMU's machine keeps the first part below 4 GiB, and the rest above it.
const RAM_BELOW_HOLE: u64 = 0x8000_0000;
const HOLE_END: u64 = 1 << 32;
const PAGE_SIZE: u64 = 4096;
const WORD_SIZE: u64 = 8;
const HALF_WORD_SIZE: u64 = 4;

/// Where a guest physical address is in the RAM file. The machine splits RAM around a hole below
/// 4 GiB, which the file does not have.
pub fn ram_offset(address: u64) -> Option<u64> {
    if address < RAM_BELOW_HOLE {
        Some(address)
    } else if address >= HOLE_END {
        Some(address - HOLE_END + RAM_BELOW_HOLE)
    } else {
        None
    }
}

fn check(result: *mut libc::c_void) -> io::Result<*mut libc::c_void> {
    if result == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(result) }
}

const HOST_BUFFER_NAME: &str = "frame-slot";

/// A new file in memory, mapped here for the GPU to write and a viewer to read.
fn map_host_buffer(size: u64) -> io::Result<HostBuffer> {
    let file = sized_buffer(HOST_BUFFER_NAME, size)?;
    let protection = libc::PROT_READ | libc::PROT_WRITE;
    // SAFETY: the kernel picks the address, and the file is `size` bytes long and sealed so.
    let at = check(unsafe {
        libc::mmap(ptr::null_mut(), size as usize, protection, libc::MAP_SHARED, file.as_raw_fd(), 0)
    })?;
    Ok(HostBuffer { file, host_address: at as u64 })
}

fn unmap_host_buffer(address: u64, size: u64) {
    // SAFETY: the buffer came from `map_host_buffer`, and the caller has stopped using it.
    unsafe { libc::munmap(address as *mut libc::c_void, size as usize) };
}

/// The window mapped here, and the RAM ready to map from.
pub struct SharedMemory {
    window: *mut u8,
    /// Where QEMU put it in guest physical memory: see [`window_base`].
    base: u64,
    window_size: u64,
    ram: File,
}

impl SharedMemory {
    /// What the app left open for this daemon. See [`crate::handover`].
    pub fn inherited(slot: Slot) -> io::Result<Self> {
        Self::new(&Part::Window.inherited(slot)?, Part::Ram.inherited(slot)?)
    }

    /// Maps all of the window, which the app has given its full size.
    pub fn new(file: &File, ram: File) -> io::Result<Self> {
        let window_size = file.metadata()?.len();
        let flags = libc::PROT_READ | libc::PROT_WRITE;
        // SAFETY: a fresh mapping at an address the kernel picks disturbs nothing. It lasts until
        // Drop, and the file may close once it exists.
        let window = unsafe {
            libc::mmap(ptr::null_mut(), window_size as usize, flags, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        Ok(Self { window: check(window)?.cast(), base: window_base(), window_size, ram })
    }

    fn window_range(&self) -> Range<u64> {
        self.base..self.base + self.window_size
    }

    /// How far `length` bytes at an address are into the mapping, if all of them are in it.
    fn offset_of(&self, address: u64, length: u64) -> Result<usize, MemoryError> {
        if in_window(&self.window_range(), address, length) {
            Ok((address - self.base) as usize)
        } else {
            Err(MemoryError::Outside(address))
        }
    }

    /// The same for a word, which also has to be aligned to its size.
    fn word_offset(&self, address: u64, size: u64) -> Result<usize, MemoryError> {
        let offset = self.offset_of(address, size)?;
        if address.is_multiple_of(size) { Ok(offset) } else { Err(MemoryError::Outside(address)) }
    }

    /// The word at a guest physical address. Guest and GPU change these while the host looks, so
    /// reads and writes go through an atomic and never through a plain reference.
    fn word(&self, address: u64) -> Result<&AtomicU64, MemoryError> {
        let offset = self.word_offset(address, WORD_SIZE)?;
        // SAFETY: `word_offset` keeps it inside the mapping and aligned, and the mapping outlives &self.
        Ok(unsafe { AtomicU64::from_ptr(self.window.add(offset).cast()) })
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // SAFETY: this is the mapping made in `open`, and no reference outlives self.
        unsafe { libc::munmap(self.window.cast(), self.window_size as usize) };
    }
}

impl GuestMemory for SharedMemory {
    fn window(&self) -> Result<Range<u64>, MemoryError> {
        Ok(self.window_range())
    }

    fn read_u64(&self, address: u64) -> Result<u64, MemoryError> {
        Ok(self.word(address)?.load(Ordering::Acquire))
    }

    fn write_u64(&mut self, address: u64, value: u64) -> Result<(), MemoryError> {
        self.word(address)?.store(value, Ordering::Release);
        Ok(())
    }

    fn write_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryError> {
        let offset = self.word_offset(address, HALF_WORD_SIZE)?;
        // SAFETY: `word_offset` keeps it inside the mapping and aligned, and the mapping outlives self.
        let word = unsafe { AtomicU32::from_ptr(self.window.add(offset).cast()) };
        word.store(value, Ordering::Release);
        Ok(())
    }

    fn read_bytes(&self, address: u64, out: &mut [u8]) -> usize {
        let Ok(offset) = self.offset_of(address, 0) else { return 0 };
        let length = out.len().min(self.window_size as usize - offset);
        // SAFETY: `length` stops at the end of the mapping, and `out` is not part of it.
        unsafe { ptr::copy_nonoverlapping(self.window.add(offset), out.as_mut_ptr(), length) };
        length
    }

    fn zero(&mut self, address: u64, length: u64) -> Result<(), MemoryError> {
        let offset = self.offset_of(address, length)?;
        // SAFETY: `offset_of` keeps the range inside the mapping. Only freed memory gets cleared,
        // which neither guest nor GPU is using.
        unsafe { ptr::write_bytes(self.window.add(offset), 0, length as usize) };
        Ok(())
    }

    fn host_address(&self, address: u64, length: u64) -> Result<u64, MemoryError> {
        Ok(self.window as u64 + self.offset_of(address, length)? as u64)
    }

    fn map_pages(&mut self, pages: &[u64]) -> io::Result<PagesView> {
        let fd = self.ram.as_raw_fd();
        let size = pages.len() as u64 * PAGE_SIZE;
        let reserve = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
        // SAFETY: this only reserves address space that the kernel picks.
        let base =
            check(unsafe { libc::mmap(ptr::null_mut(), size as usize, libc::PROT_NONE, reserve, -1, 0) })?;
        let view = PagesView { host_address: base as u64, size };
        let mut index = 0;
        while index < pages.len() {
            let run = 1 + pages[index..]
                .windows(2)
                .take_while(|pair| pair[0].checked_add(1) == Some(pair[1]))
                .count();
            let result = map_run(base, index, run, fd, pages[index]);
            if let Err(error) = result {
                self.unmap_pages(view);
                return Err(error);
            }
            index += run;
        }
        Ok(view)
    }

    fn unmap_pages(&mut self, view: PagesView) {
        // SAFETY: the view came from `map_pages`, and the caller has stopped using it.
        unsafe { libc::munmap(view.host_address as *mut libc::c_void, view.size as usize) };
    }

    fn read_view(&self, view: PagesView, offset: u64, out: &mut [u8]) -> usize {
        let length = view.size.saturating_sub(offset).min(out.len() as u64) as usize;
        let from = (view.host_address + offset) as *const u8;
        // SAFETY: the view stays mapped until `unmap_pages`, and `length` keeps this inside it.
        unsafe { ptr::copy_nonoverlapping(from, out.as_mut_ptr(), length) };
        length
    }

    fn write_view_u32(&mut self, view: PagesView, offset: u64, value: u32) {
        if offset.is_multiple_of(4) && offset + 4 <= view.size {
            let at = (view.host_address + offset) as *mut u32;
            // SAFETY: the check keeps it inside the view and aligned, as views start on a page. It
            // is atomic because the GPU can be reading these pages.
            unsafe { AtomicU32::from_ptr(at).store(value, Ordering::Release) };
        }
    }

    fn host_buffer(&mut self, size: u64) -> io::Result<HostBuffer> {
        map_host_buffer(size)
    }

    fn release_host_buffer(&mut self, address: u64, size: u64) {
        unmap_host_buffer(address, size);
    }
}

/// Maps `run` consecutive guest pages over part of a reservation.
fn map_run(base: *mut libc::c_void, index: usize, run: usize, fd: i32, page: u64) -> io::Result<()> {
    let offset = page.checked_mul(PAGE_SIZE).and_then(ram_offset);
    let offset = offset
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("page {page:#x} is not RAM")))?;
    let at = base.wrapping_byte_add(index * PAGE_SIZE as usize);
    let protection = libc::PROT_READ | libc::PROT_WRITE;
    let flags = libc::MAP_SHARED | libc::MAP_FIXED;
    // SAFETY: `at` lies inside the reservation made by `map_pages`, which nothing else uses.
    let done = unsafe { libc::mmap(at, run * PAGE_SIZE as usize, protection, flags, fd, offset as i64) };
    check(done).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::process;

    fn temporary_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = env::temp_dir().join(format!("guesthouse-test-{}-{name}", process::id()));
        fs::write(&path, bytes).unwrap();
        path
    }

    fn open(path: &Path) -> File {
        OpenOptions::new().read(true).write(true).open(path).unwrap()
    }

    /// The window in this file, with RAM that's only there to have one.
    fn window_only(window: &Path) -> SharedMemory {
        SharedMemory::new(&open(window), open(window)).unwrap()
    }

    #[test]
    fn ram_is_split_around_the_hole() {
        assert_eq!(ram_offset(0x1000), Some(0x1000));
        assert_eq!(ram_offset(RAM_BELOW_HOLE), None);
        assert_eq!(ram_offset(HOLE_END), Some(RAM_BELOW_HOLE));
    }

    #[test]
    fn the_window_reads_and_writes_the_file() {
        let window = temporary_file("window", &[0; 0x2000]);
        let mut memory = window_only(&window);
        assert_eq!(memory.window(), Ok(window_base()..window_base() + 0x2000));
        memory.write_u64(window_base() + 0x1000, 0x1122).unwrap();
        assert_eq!(memory.read_u64(window_base() + 0x1000), Ok(0x1122));
        drop(memory);
        assert_eq!(fs::read(&window).unwrap()[0x1000..0x1002], [0x22, 0x11]);
        fs::remove_file(window).unwrap();
    }

    #[test]
    fn a_host_address_is_only_ever_inside_the_mapping() {
        let window = temporary_file("host-address", &[0; 0x2000]);
        let memory = window_only(&window);
        let base = memory.window as u64;
        assert_eq!(memory.host_address(window_base(), 0x2000), Ok(base));
        assert_eq!(memory.host_address(window_base() + 0x1000, 0x1000), Ok(base + 0x1000));
        assert!(memory.host_address(window_base() + 0x1000, 0x1001).is_err());
        assert!(memory.host_address(window_base() - 0x1000, 0x1000).is_err());
        assert!(memory.host_address(u64::MAX, 2).is_err());
        fs::remove_file(window).unwrap();
    }

    #[test]
    fn zero_clears_only_what_it_says() {
        let window = temporary_file("zero", &[0xFF; 0x3000]);
        let mut memory = window_only(&window);
        memory.zero(window_base() + 0x1000, 0x1000).unwrap();
        assert_eq!(memory.read_u64(window_base() + 0x1000), Ok(0));
        assert_eq!(memory.read_u64(window_base() + 0x2000), Ok(u64::MAX));
        fs::remove_file(window).unwrap();
    }

    #[test]
    fn accesses_past_the_window_are_errors_and_nothing_else() {
        let window = temporary_file("outside", &[0xFF; 0x1000]);
        let mut memory = window_only(&window);
        for address in [0, window_base() - 8, window_base() + 0x1000, window_base() + 4, u64::MAX - 7] {
            assert_eq!(memory.read_u64(address), Err(MemoryError::Outside(address)));
            assert_eq!(memory.write_u64(address, 0), Err(MemoryError::Outside(address)));
        }
        assert!(memory.write_u32(window_base() + 0x1000, 0).is_err());
        assert!(memory.write_u32(window_base() + 2, 0).is_err());
        assert!(memory.zero(window_base() + 0x800, 0x801).is_err());
        assert!(memory.zero(u64::MAX, 2).is_err());
        assert_eq!(memory.read_u64(window_base()), Ok(u64::MAX), "a refused write changes nothing");
        fs::remove_file(window).unwrap();
    }

    #[test]
    fn reads_outside_the_window_find_nothing() {
        let window = temporary_file("short-read", &[7; 0x1000]);
        let memory = window_only(&window);
        assert_eq!(memory.read_bytes(window_base() - 8, &mut [0; 8]), 0);
        assert_eq!(memory.read_bytes(window_base() + 0x1000, &mut [0; 8]), 0);
        assert_eq!(memory.read_bytes(window_base() + 0xFFC, &mut [0; 8]), 4);
        fs::remove_file(window).unwrap();
    }

    #[test]
    fn scattered_pages_come_out_side_by_side() {
        let mut ram = vec![0; 4 * PAGE_SIZE as usize];
        (0..4).for_each(|page| ram[page * PAGE_SIZE as usize] = page as u8 + 1);
        let window = temporary_file("pages-window", &[0; 8]);
        let ram = temporary_file("pages-ram", &ram);
        let mut memory = SharedMemory::new(&open(&window), open(&ram)).unwrap();
        let view = memory.map_pages(&[3, 1, 2]).unwrap();
        assert_eq!(view.size, 3 * PAGE_SIZE);
        let bytes = view.host_address as *const u8;
        // SAFETY: the view is mapped for three pages until the unmap below.
        let firsts: Vec<u8> = (0..3).map(|page| unsafe { *bytes.add(page * PAGE_SIZE as usize) }).collect();
        assert_eq!(firsts, [4, 2, 3]);
        memory.unmap_pages(view);
        fs::remove_file(window).unwrap();
        fs::remove_file(ram).unwrap();
    }

    #[test]
    fn a_view_reads_and_writes_across_scattered_pages_and_stops_at_its_end() {
        let ram = vec![0; 2 * PAGE_SIZE as usize];
        let window = temporary_file("view-window", &[0; 8]);
        let ram = temporary_file("view-ram", &ram);
        let mut memory = SharedMemory::new(&open(&window), open(&ram)).unwrap();
        let view = memory.map_pages(&[1, 0]).unwrap();
        memory.write_view_u32(view, PAGE_SIZE, 0x1234_5678);
        memory.write_view_u32(view, 2 * PAGE_SIZE, 1);
        let mut out = [0; 4];
        assert_eq!(memory.read_view(view, PAGE_SIZE, &mut out), 4);
        assert_eq!(u32::from_le_bytes(out), 0x1234_5678);
        assert_eq!(memory.read_view(view, 2 * PAGE_SIZE - 2, &mut out), 2);
        assert_eq!(memory.read_view(view, 3 * PAGE_SIZE, &mut out), 0);
        memory.unmap_pages(view);
        fs::remove_file(window).unwrap();
        fs::remove_file(ram).unwrap();
    }

    #[test]
    fn a_page_in_the_hole_is_refused() {
        let window = temporary_file("hole-window", &[0; 8]);
        let ram = temporary_file("hole-ram", &[0; 4096]);
        let mut memory = SharedMemory::new(&open(&window), open(&ram)).unwrap();
        assert!(memory.map_pages(&[RAM_BELOW_HOLE / PAGE_SIZE]).is_err());
        assert!(memory.map_pages(&[u64::MAX - 1, u64::MAX]).is_err());
        fs::remove_file(window).unwrap();
        fs::remove_file(ram).unwrap();
    }
}
