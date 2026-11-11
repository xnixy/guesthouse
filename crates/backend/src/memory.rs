//! How the host reaches memory it shares with the guest.

use crate::BackendError;
use std::fmt;
use std::io;
use std::ops::Range;
use std::os::fd::OwnedFd;

/// Where QEMU places the shared window in guest physical memory, on a host that can address it.
pub const WINDOW_BASE: u64 = 1 << 40;

/// Where the window goes on this host. KVM refuses guest memory above what the host's CPU can
/// address, and many Intel CPUs address 39 bits, 512 GiB, which is below [`WINDOW_BASE`]. The app and
/// the daemon each ask the CPU, so they agree without telling each other, even inside a jail.
pub fn window_base() -> u64 {
    window_base_for(host_physical_bits())
}

/// Halfway up a smaller address space: far above the guest's RAM, with room for the window above it.
pub fn window_base_for(physical_bits: u32) -> u64 {
    if physical_bits > 40 { WINDOW_BASE } else { 1 << (physical_bits.max(37) - 1) }
}

/// The CPU's physical address width, from CPUID leaf 0x80000008.
fn host_physical_bits() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        const ADDRESS_SIZES: u32 = 0x8000_0008;
        // SAFETY: CPUID only reads, and every x86_64 CPU has it.
        #[allow(unused_unsafe)]
        let highest = unsafe { __cpuid(0x8000_0000) }.eax;
        if highest >= ADDRESS_SIZES {
            #[allow(unused_unsafe)]
            return unsafe { __cpuid(ADDRESS_SIZES) }.eax & 0xFF;
        }
    }
    48
}

/// Why shared memory couldn't be reached. Nothing panics over these, as the address may have come
/// from a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    /// This address isn't in the window, or isn't aligned for the access.
    Outside(u64),
    /// The window's file can't be opened, which the log says more about.
    Unavailable,
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Outside(address) => write!(f, "{address:#x} is not usable in the shared window"),
            Self::Unavailable => f.write_str("the shared window is not available"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<MemoryError> for BackendError {
    fn from(error: MemoryError) -> Self {
        Self::new(error.to_string())
    }
}

/// Guest pages mapped side by side in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagesView {
    pub host_address: u64,
    pub size: u64,
}

/// Whether all of `length` bytes at `address` are in `window`.
pub fn in_window(window: &Range<u64>, address: u64, length: u64) -> bool {
    address >= window.start && address.checked_add(length).is_some_and(|end| end <= window.end)
}

/// The shared window and the guest's RAM. Addresses are guest physical.
///
/// Fences live in the window, where guest and GPU read them while the host writes, so the 64-bit
/// accesses must each be a single one.
pub trait GuestMemory {
    /// The guest physical addresses the window covers.
    fn window(&self) -> Result<Range<u64>, MemoryError>;

    fn read_u64(&self, address: u64) -> Result<u64, MemoryError>;

    fn write_u64(&mut self, address: u64, value: u64) -> Result<(), MemoryError>;

    /// A 32-bit field, which the GPU may read while it changes, so this is a single store too.
    fn write_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryError>;

    /// Copies bytes out of the window, as far as `out` goes or the window does. Returns how many,
    /// which is none for an address outside it.
    fn read_bytes(&self, address: u64, out: &mut [u8]) -> usize;

    fn zero(&mut self, address: u64, length: u64) -> Result<(), MemoryError>;

    /// Where `length` bytes at a window address are in this process. All of them must be in the
    /// window.
    fn host_address(&self, address: u64, length: u64) -> Result<u64, MemoryError>;

    /// Maps scattered guest pages, by page number, at consecutive addresses here.
    fn map_pages(&mut self, pages: &[u64]) -> io::Result<PagesView>;

    fn unmap_pages(&mut self, view: PagesView);

    /// Copies bytes out of a view, as far as `out` goes or the view does. Returns how many.
    fn read_view(&self, view: PagesView, offset: u64, out: &mut [u8]) -> usize;

    /// A 32-bit field in a view, as a single store for the same reason as [`Self::write_u32`].
    /// Does nothing past the end of the view.
    fn write_view_u32(&mut self, view: PagesView, offset: u64, value: u32);

    /// Memory of this process that a viewer can share: a file of `size` bytes, and where it's mapped
    /// here. A viewer whose screen is on another GPU gets its frames in these.
    fn host_buffer(&mut self, _size: u64) -> io::Result<HostBuffer> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Unmaps a buffer from [`Self::host_buffer`]. The file stays for whoever still holds it.
    fn release_host_buffer(&mut self, _address: u64, _size: u64) {}
}

/// Memory from [`GuestMemory::host_buffer`].
#[derive(Debug)]
pub struct HostBuffer {
    pub file: OwnedFd,
    pub host_address: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW_SIZE: u64 = 64 << 30;

    #[test]
    fn a_wide_host_keeps_the_window_where_it_always_was() {
        assert_eq!(window_base_for(48), WINDOW_BASE);
        assert_eq!(window_base_for(41), WINDOW_BASE);
    }

    #[test]
    fn a_narrow_host_gets_a_window_it_can_address() {
        for bits in 37..=40 {
            let base = window_base_for(bits);
            assert!(base + WINDOW_SIZE <= 1 << bits, "{bits}");
            assert!(base >= 64 << 30, "{bits}: the window must stay above the guest's RAM");
        }
        assert_eq!(window_base_for(39), 256 << 30);
    }

    #[test]
    fn this_hosts_window_fits_this_host() {
        assert!(window_base() + WINDOW_SIZE <= 1 << host_physical_bits().max(37));
    }
}
