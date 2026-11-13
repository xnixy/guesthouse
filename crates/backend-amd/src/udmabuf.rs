//! Guest memory handed to the card as a dma-buf.
//!
//! The guest's memory is a file that QEMU and this daemon both have open. The kernel's udmabuf
//! device makes a dma-buf of parts of such a file and keeps those pages where they are for as long
//! as the dma-buf lives, and amdgpu takes a dma-buf in as it does another open's buffer. Pages
//! adopted by their address are looked up again for every submission that lists them, so a
//! submission pays for all the guest memory mapped in its space, and pays nothing for a dma-buf.
//!
//! The device only takes a file from `memfd_create` that is sealed against shrinking, and it has
//! limits of its own on a buffer's size and on how many ranges it is made of. Whatever it refuses
//! is adopted by its address, as all guest memory was before.
//!
//! The request layout is the kernel's uAPI, `include/uapi/linux/udmabuf.h`.

use crate::ioctl::{put32, put64};
use guesthouse_backend::{FilePages, FileRange};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const DEVICE: &str = "/dev/udmabuf";

const IOCTL_WRITE: u64 = 1 << 30;
const IOCTL_SIZE_SHIFT: u32 = 16;
const UDMABUF_MAGIC: u64 = b'u' as u64;
const UDMABUF_CREATE_LIST: u64 = 0x43;

/// The new dma-buf's descriptor closes on exec.
const FLAG_CLOEXEC: u32 = 1;

// udmabuf_create_list: the flags, how many items follow, and the items.
const LIST_FLAGS: usize = 0;
const LIST_COUNT: usize = 4;
const LIST_HEADER_BYTES: usize = 8;

// udmabuf_create_item: the file, four bytes of padding, and a range of it in whole pages.
const ITEM_FILE: usize = 0;
const ITEM_OFFSET: usize = 8;
const ITEM_LENGTH: usize = 16;
const ITEM_BYTES: usize = 24;

/// The ioctl's number, which carries the size of the header alone.
fn create_list_call() -> u64 {
    IOCTL_WRITE | (LIST_HEADER_BYTES as u64) << IOCTL_SIZE_SHIFT | UDMABUF_MAGIC << 8 | UDMABUF_CREATE_LIST
}

/// The request for one dma-buf of these ranges of a file, in their order.
fn create_list(file: RawFd, ranges: &[FileRange]) -> Vec<u8> {
    let mut request = vec![0u8; LIST_HEADER_BYTES + ranges.len() * ITEM_BYTES];
    put32(&mut request, LIST_FLAGS, FLAG_CLOEXEC);
    put32(&mut request, LIST_COUNT, ranges.len() as u32);
    for (index, range) in ranges.iter().enumerate() {
        let item = LIST_HEADER_BYTES + index * ITEM_BYTES;
        put32(&mut request, item + ITEM_FILE, file as u32);
        put64(&mut request, item + ITEM_OFFSET, range.offset);
        put64(&mut request, item + ITEM_LENGTH, range.length);
    }
    request
}

/// The udmabuf device, if this user may open it.
#[derive(Debug)]
pub struct Udmabuf {
    device: Option<File>,
    /// Whether the log has the first refusal. The reason is most often the same for every buffer,
    /// such as a file that isn't a sealed memfd, so only the first is worth a line.
    refusal_logged: bool,
}

impl Udmabuf {
    /// Opens the device. Without it nothing is lost but speed, so that is all the log says.
    pub fn open() -> Self {
        let device = OpenOptions::new().read(true).write(true).open(DEVICE);
        if let Err(error) = &device {
            log::info!(
                "{DEVICE} can't be opened ({error}), so the card gets guest memory by its address, \
                 and each submission pays for all of it"
            );
        }
        Self { device: device.ok(), refusal_logged: false }
    }

    /// A dma-buf of these pages, or `None` when the device isn't there or refuses them.
    pub fn wrap(&mut self, pages: &FilePages<'_>) -> Option<OwnedFd> {
        let device = self.device.as_ref()?;
        match create(device, pages) {
            Ok(buffer) => Some(buffer),
            Err(error) => {
                let (bytes, ranges) = (pages.size(), pages.ranges.len());
                if self.refusal_logged {
                    log::debug!("{DEVICE} refused {bytes:#x} bytes in {ranges} ranges: {error}");
                } else {
                    log::info!(
                        "{DEVICE} refused {bytes:#x} bytes in {ranges} ranges ({error}), so they go to \
                         the card by their address; later refusals are only in the debug log"
                    );
                    self.refusal_logged = true;
                }
                None
            }
        }
    }
}

fn create(device: &File, pages: &FilePages<'_>) -> io::Result<OwnedFd> {
    let mut request = create_list(pages.file.as_raw_fd(), &pages.ranges);
    // SAFETY: the request is the header and as many items as its count says, and the kernel only
    // reads it.
    let made = unsafe { libc::ioctl(device.as_raw_fd(), create_list_call() as _, request.as_mut_ptr()) };
    if made < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel returned a fresh file descriptor the caller now owns.
    Ok(unsafe { OwnedFd::from_raw_fd(made) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn the_call_has_the_number_the_kernel_header_gives() {
        // _IOW('u', 0x43, struct udmabuf_create_list), whose fixed part is eight bytes.
        assert_eq!(create_list_call(), 0x4008_7543);
    }

    #[test]
    fn a_request_lists_every_range_in_order() {
        let ranges =
            [FileRange { offset: 0x3000, length: 0x2000 }, FileRange { offset: 0x1000, length: 0x1000 }];
        let request = create_list(7, &ranges);
        assert_eq!(request.len(), 8 + 2 * 24);
        assert_eq!(request[..8], [1, 0, 0, 0, 2, 0, 0, 0], "close on exec, two items");
        let first = &request[8..32];
        assert_eq!(first[..8], [7, 0, 0, 0, 0, 0, 0, 0], "the file, then padding");
        assert_eq!(first[8..16], 0x3000u64.to_le_bytes());
        assert_eq!(first[16..24], 0x2000u64.to_le_bytes());
        let second = &request[32..56];
        assert_eq!(second[8..16], 0x1000u64.to_le_bytes());
        assert_eq!(second[16..24], 0x1000u64.to_le_bytes());
    }

    #[test]
    fn without_the_device_nothing_is_wrapped() {
        let mut udmabuf = Udmabuf { device: None, refusal_logged: false };
        let file = File::open("/dev/null").unwrap();
        let pages = FilePages { file: file.as_fd(), ranges: vec![FileRange { offset: 0, length: 0x1000 }] };
        assert!(udmabuf.wrap(&pages).is_none());
        assert!(!udmabuf.refusal_logged, "a missing device was said once, when it was opened");
    }

    /// A file like the guest's memory: from `memfd_create`, and sealed against shrinking.
    fn sealed_memory(bytes: u64) -> File {
        // SAFETY: the name is a C string, and the descriptor returned is new and owned here.
        let file = unsafe {
            let made =
                libc::memfd_create(c"udmabuf-test".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
            assert!(made >= 0, "memfd_create: {}", io::Error::last_os_error());
            File::from_raw_fd(made)
        };
        file.set_len(bytes).unwrap();
        // SAFETY: a seal is added to a file this test owns.
        let sealed = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) };
        assert_eq!(sealed, 0, "sealing: {}", io::Error::last_os_error());
        file
    }

    /// How long a dma-buf is, which the kernel tells by a seek to its end.
    fn length_of(buffer: &OwnedFd) -> u64 {
        // SAFETY: a seek on a descriptor this test owns.
        let end = unsafe { libc::lseek(buffer.as_raw_fd(), 0, libc::SEEK_END) };
        assert!(end >= 0, "seeking a dma-buf: {}", io::Error::last_os_error());
        end as u64
    }

    #[test]
    #[ignore = "needs /dev/udmabuf, which this user may not have"]
    fn scattered_pages_of_a_sealed_file_become_one_buffer() {
        let mut udmabuf = Udmabuf::open();
        assert!(udmabuf.device.is_some(), "{DEVICE} is not open to this user");
        let file = sealed_memory(0x8000);
        let ranges =
            vec![FileRange { offset: 0x5000, length: 0x2000 }, FileRange { offset: 0, length: 0x1000 }];
        let buffer = udmabuf.wrap(&FilePages { file: file.as_fd(), ranges }).expect("a dma-buf");
        assert_eq!(length_of(&buffer), 0x3000);
    }

    #[test]
    #[ignore = "needs /dev/udmabuf, which this user may not have"]
    fn a_file_that_could_shrink_is_refused_and_said_once() {
        let mut udmabuf = Udmabuf::open();
        assert!(udmabuf.device.is_some(), "{DEVICE} is not open to this user");
        let path = std::env::temp_dir().join(format!("guesthouse-udmabuf-{}", std::process::id()));
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        file.set_len(0x1000).unwrap();
        let pages = FilePages { file: file.as_fd(), ranges: vec![FileRange { offset: 0, length: 0x1000 }] };
        assert!(udmabuf.wrap(&pages).is_none());
        assert!(udmabuf.refusal_logged);
        std::fs::remove_file(path).unwrap();
    }
}
