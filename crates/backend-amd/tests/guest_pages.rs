//! Guest memory reaching the card as a dma-buf, on a real card, and what a submission pays for it.
//!
//! A file like the guest's memory is handed to the card twice: adopted by the address it is mapped
//! at, and as a dma-buf of the file's own pages. The card writes through the dma-buf and the word
//! is read back from the file, and then the same small submission is timed with each of the two in
//! its buffer list. Without an AMD render node, or without `/dev/udmabuf`, it returns early.

use guesthouse_backend::{FilePages, FileRange};
use guesthouse_backend_amd::{
    Context, CpuAccess, DOMAIN_GTT, Device, GemHandle, HW_IP_DMA, Ib, MapFlags, MemoryType, Syncobj, Udmabuf,
};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::time::Instant;

const MAGIC: u32 = 0x5EED_4321;
const PAGE: u64 = 4096;
const MIB: u64 = 1 << 20;
/// Where in the file the card's word lands: not its first page, so a wrong offset shows.
const WORD_AT: u64 = 3 * PAGE;
/// How many submissions each time is the median of.
const REPEATS: usize = 50;
const WAIT_MS: i32 = 2000;

/// A file like the guest's memory, mapped here with every page present.
struct Memory {
    file: File,
    at: *mut u8,
    bytes: u64,
}

impl Memory {
    fn new(bytes: u64) -> Self {
        // SAFETY: the name is a C string, and the descriptor returned is new and owned here.
        let file = unsafe {
            let made =
                libc::memfd_create(c"guest-pages-test".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
            assert!(made >= 0, "memfd_create: {}", io::Error::last_os_error());
            File::from_raw_fd(made)
        };
        file.set_len(bytes).unwrap();
        // SAFETY: a seal is added to a file this test owns.
        let sealed = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) };
        assert_eq!(sealed, 0, "sealing: {}", io::Error::last_os_error());
        let protection = libc::PROT_READ | libc::PROT_WRITE;
        // SAFETY: a fresh shared mapping of the whole file, at an address the kernel picks.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes as usize,
                protection,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(at, libc::MAP_FAILED, "mapping {bytes} bytes: {}", io::Error::last_os_error());
        // SAFETY: the mapping is `bytes` long and writable.
        unsafe { std::ptr::write_bytes(at.cast::<u8>(), 0, bytes as usize) };
        Self { file, at: at.cast(), bytes }
    }

    /// All of the file, as one range.
    fn pages(&self) -> FilePages<'_> {
        FilePages { file: self.file.as_fd(), ranges: vec![FileRange { offset: 0, length: self.bytes }] }
    }

    fn word(&self, offset: u64) -> u32 {
        // SAFETY: the offset is inside the mapping and a multiple of four.
        unsafe { self.at.add(offset as usize).cast::<u32>().read_volatile() }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: the mapping was made in `new` with this size and is not used again.
        unsafe { libc::munmap(self.at.cast(), self.bytes as usize) };
    }
}

/// The median time of the submit call alone, in microseconds, with `buffers` in its list. Each
/// submission is waited for, so the card is idle at the next.
fn median_submit(device: &Device, context: &Context, ib: Ib, buffers: &[GemHandle]) -> f64 {
    let mut times = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let signal = Syncobj::create(device).unwrap();
        let started = Instant::now();
        guesthouse_backend_amd::submit_all(device, context, HW_IP_DMA, &[ib], buffers, &signal).unwrap();
        times.push(started.elapsed().as_secs_f64() * 1e6);
        let event = signal.arm_eventfd(device).unwrap();
        assert!(guesthouse_backend_amd::wait_eventfd(&event, WAIT_MS).unwrap(), "did not complete");
        signal.destroy(device);
    }
    times.sort_by(f64::total_cmp);
    times[REPEATS / 2]
}

#[test]
fn the_card_writes_file_pages_through_a_dma_buf_and_a_submission_pays_less_for_them() {
    let Ok(device) = Device::open(None) else {
        eprintln!("no AMD render node; skipping the guest pages proof");
        return;
    };
    let mut udmabuf = Udmabuf::open();
    let probe = Memory::new(PAGE);
    if udmabuf.wrap(&probe.pages()).is_none() {
        eprintln!("/dev/udmabuf is not usable here; skipping the guest pages proof");
        return;
    }

    let info = device.info().clone();
    let align = info.va_alignment.max(PAGE);
    let flags =
        MapFlags { readable: true, writable: true, executable: true, memory_type: MemoryType::Uncached };
    let context = Context::create(&device).unwrap();
    let ib =
        guesthouse_backend_amd::create_buffer(&device, PAGE, align, DOMAIN_GTT, CpuAccess::Required).unwrap();
    let ib_va = info.va_start + 0x1_0000_0000;
    guesthouse_backend_amd::map_buffer(&device, ib, ib_va, 0, PAGE, flags).unwrap();
    let mut ib_cpu = device.map_cpu(ib, PAGE as usize).unwrap();
    let mut next_va = ib_va + 0x1000_0000;

    println!("\n  user memory named    by address     as a dma-buf");
    for mib in [1, 16, 64] {
        let memory = Memory::new(mib * MIB);
        let by_address =
            guesthouse_backend_amd::adopt_buffer(&device, memory.at as u64, memory.bytes, false).unwrap();
        let shared = udmabuf.wrap(&memory.pages()).expect("a dma-buf of the file");
        let as_dma_buf = guesthouse_backend_amd::import_buffer(&device, &shared).unwrap();
        drop(shared);
        let (address_va, dma_buf_va) = (next_va, next_va + memory.bytes);
        next_va += 2 * memory.bytes;
        guesthouse_backend_amd::map_buffer(&device, by_address, address_va, 0, memory.bytes, flags).unwrap();
        guesthouse_backend_amd::map_buffer(&device, as_dma_buf, dma_buf_va, 0, memory.bytes, flags).unwrap();

        // The card writes through the dma-buf, and the word is in the file where the CPU reads it.
        let packet = guesthouse_backend_amd::sdma_write_ib(dma_buf_va + WORD_AT, &[MAGIC + mib as u32]);
        ib_cpu.write_dwords(&packet);
        let run = Ib { address: ib_va, bytes: (packet.len() * 4) as u32 };
        let dma_buf_time = median_submit(&device, &context, run, &[ib, as_dma_buf]);
        assert_eq!(memory.word(WORD_AT), MAGIC + mib as u32, "the card wrote somewhere else");

        let packet = guesthouse_backend_amd::sdma_write_ib(address_va + WORD_AT + PAGE, &[MAGIC]);
        ib_cpu.write_dwords(&packet);
        let run = Ib { address: ib_va, bytes: (packet.len() * 4) as u32 };
        let address_time = median_submit(&device, &context, run, &[ib, by_address]);
        assert_eq!(memory.word(WORD_AT + PAGE), MAGIC);
        println!("  {mib:>3} MiB            {address_time:>8.1} µs     {dma_buf_time:>8.1} µs");

        guesthouse_backend_amd::unmap_buffer(&device, by_address, address_va, memory.bytes).unwrap();
        guesthouse_backend_amd::unmap_buffer(&device, as_dma_buf, dma_buf_va, memory.bytes).unwrap();
        guesthouse_backend_amd::close_buffer(&device, by_address);
        guesthouse_backend_amd::close_buffer(&device, as_dma_buf);
    }

    guesthouse_backend_amd::unmap_buffer(&device, ib, ib_va, PAGE).unwrap();
    guesthouse_backend_amd::close_buffer(&device, ib);
    context.destroy(&device);
}
