//! The amdgpu render node and the DRM ioctls it answers.
//!
//! Each request is a plain structure with a fixed layout, written here by offset. The numbers and
//! layouts come from the kernel's public uAPI headers, `include/uapi/drm/amdgpu_drm.h` and `drm.h`,
//! which are GPL-2.0 and the only description the open driver has. An ioctl's number packs its
//! direction, the request's size and a per-driver command number, the same encoding the NVIDIA
//! backend uses for its own DRM calls.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DRM_CLASS_FOLDER: &str = "/sys/class/drm";
const DRM_DEVICE_FOLDER: &str = "/dev/dri";
const RENDER_NODE_PREFIX: &str = "renderD";
const VENDOR_FILE: &str = "device/vendor";
/// The PCI vendor id AMD's cards carry. The node number is never hardcoded: it moves with whatever
/// else is in the machine, so the card is found by this id.
pub const AMD_VENDOR: u16 = 0x1002;

const IOCTL_WRITE: u64 = 1 << 30;
const IOCTL_READ_WRITE: u64 = 3 << 30;
const IOCTL_SIZE_SHIFT: u32 = 16;
const DRM_IOCTL_MAGIC: u64 = b'd' as u64;
/// Where a DRM driver's own ioctls begin, after the ones every driver shares.
const DRM_COMMAND_BASE: u64 = 0x40;

// Core DRM ioctls, shared by every driver.
const DRM_GEM_CLOSE: u64 = 0x09;
const DRM_PRIME_HANDLE_TO_FD: u64 = 0x2D;
const DRM_PRIME_FD_TO_HANDLE: u64 = 0x2E;
const DRM_SYNCOBJ_CREATE: u64 = 0xBF;
const DRM_SYNCOBJ_DESTROY: u64 = 0xC0;
const DRM_SYNCOBJ_WAIT: u64 = 0xC3;
const DRM_SYNCOBJ_EVENTFD: u64 = 0xCF;

// amdgpu's own ioctls, at DRM_COMMAND_BASE plus the driver's command number.
const DRM_AMDGPU_GEM_CREATE: u64 = 0x00;
const DRM_AMDGPU_GEM_MMAP: u64 = 0x01;
const DRM_AMDGPU_CTX: u64 = 0x02;
const DRM_AMDGPU_CS: u64 = 0x04;
const DRM_AMDGPU_INFO: u64 = 0x05;
const DRM_AMDGPU_GEM_METADATA: u64 = 0x06;
const DRM_AMDGPU_GEM_VA: u64 = 0x08;
const DRM_AMDGPU_GEM_USERPTR: u64 = 0x11;

fn number(direction: u64, command: u64, size: usize) -> u64 {
    direction | (size as u64) << IOCTL_SIZE_SHIFT | DRM_IOCTL_MAGIC << 8 | command
}

/// The ioctl numbers this backend calls, each for the fixed size of its request.
pub mod call {
    use super::*;

    pub fn gem_create() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_CREATE, 32)
    }
    pub fn gem_mmap() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_MMAP, 8)
    }
    pub fn gem_userptr() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_USERPTR, 24)
    }
    pub fn gem_metadata() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_METADATA, 288)
    }
    pub fn gem_va() -> u64 {
        number(IOCTL_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_GEM_VA, 64)
    }
    pub fn ctx() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_CTX, 16)
    }
    pub fn cs() -> u64 {
        number(IOCTL_READ_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_CS, 24)
    }
    pub fn info() -> u64 {
        number(IOCTL_WRITE, DRM_COMMAND_BASE + DRM_AMDGPU_INFO, 32)
    }
    pub fn gem_close() -> u64 {
        number(IOCTL_WRITE, DRM_GEM_CLOSE, 8)
    }
    pub fn prime_handle_to_fd() -> u64 {
        number(IOCTL_READ_WRITE, DRM_PRIME_HANDLE_TO_FD, 12)
    }
    pub fn prime_fd_to_handle() -> u64 {
        number(IOCTL_READ_WRITE, DRM_PRIME_FD_TO_HANDLE, 12)
    }
    pub fn syncobj_create() -> u64 {
        number(IOCTL_READ_WRITE, DRM_SYNCOBJ_CREATE, 8)
    }
    pub fn syncobj_destroy() -> u64 {
        number(IOCTL_READ_WRITE, DRM_SYNCOBJ_DESTROY, 8)
    }
    pub fn syncobj_wait() -> u64 {
        number(IOCTL_READ_WRITE, DRM_SYNCOBJ_WAIT, 32)
    }
    pub fn syncobj_eventfd() -> u64 {
        number(IOCTL_READ_WRITE, DRM_SYNCOBJ_EVENTFD, 24)
    }
}

pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

pub fn get64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// How long a call amdgpu answers with "try again" keeps being tried before its caller hears it.
/// amdgpu says that to a submission when the pages of a buffer it lists moved while it was being
/// checked, and the next try goes through, which is why DRM's own library tries again without a
/// limit. The limit only stops a call that never goes through from holding the daemon for ever.
const TRY_AGAIN_FOR: Duration = Duration::from_millis(500);

/// A DRM ioctl against an open file. It is tried again if a signal cuts it short or amdgpu says try
/// again, as DRM's own library does. If amdgpu still says try again after [`TRY_AGAIN_FOR`], the
/// error comes back with the kind `WouldBlock`, and nothing was done. The request buffer is the size
/// the number says, and the kernel writes only inside it.
pub fn drm_ioctl(file: &impl AsRawFd, ioctl: u64, request: &mut [u8]) -> io::Result<()> {
    let pointer = request.as_mut_ptr();
    let call = || {
        // SAFETY: the request is a live buffer of the size the ioctl number carries, and the kernel
        // reads and writes only inside it.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), ioctl as _, pointer) };
        if result >= 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    };
    let tried = try_until_taken(call, TRY_AGAIN_FOR);
    match &tried.result {
        Ok(()) if tried.refusals > 0 => log::info!(
            "amdgpu said try again {} times to {}, and took it on the next try",
            tried.refusals,
            what(ioctl)
        ),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => log::warn!(
            "amdgpu said try again {} times to {} in {} ms, so it waits for later",
            tried.refusals,
            what(ioctl),
            TRY_AGAIN_FOR.as_millis()
        ),
        _ => {}
    }
    tried.result
}

/// What came of a call, and how many times it was refused for now on the way.
#[derive(Debug)]
struct Tried {
    result: io::Result<()>,
    refusals: u32,
}

/// Makes `call` until it goes through or fails for good. A signal cutting it short doesn't count,
/// and a "try again" is tried again for up to `patience`.
fn try_until_taken(mut call: impl FnMut() -> io::Result<()>, patience: Duration) -> Tried {
    let mut refusals = 0;
    let mut first_refusal = None;
    loop {
        let error = match call() {
            Ok(()) => return Tried { result: Ok(()), refusals },
            Err(error) => error,
        };
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                refusals += 1;
                let since = *first_refusal.get_or_insert_with(Instant::now);
                if since.elapsed() >= patience {
                    return Tried { result: Err(error), refusals };
                }
                // What amdgpu waits for is another thread finishing with the pages, so let it run.
                thread::yield_now();
            }
            _ => return Tried { result: Err(error), refusals },
        }
    }
}

/// A call's name for the log.
fn what(ioctl: u64) -> String {
    let names = [
        (call::cs(), "a submission"),
        (call::gem_create(), "a new buffer"),
        (call::gem_userptr(), "a buffer over guest memory"),
        (call::gem_mmap(), "a CPU mapping"),
        (call::gem_va(), "a GPU mapping"),
        (call::gem_metadata(), "a buffer's metadata"),
        (call::gem_close(), "a buffer's close"),
        (call::ctx(), "a context call"),
        (call::info(), "a query"),
        (call::prime_handle_to_fd(), "a buffer export"),
        (call::prime_fd_to_handle(), "a buffer import"),
        (call::syncobj_create(), "a new sync object"),
        (call::syncobj_destroy(), "a sync object's close"),
        (call::syncobj_wait(), "a look at a sync object"),
        (call::syncobj_eventfd(), "a sync object's eventfd"),
    ];
    match names.iter().find(|(number, _)| *number == ioctl) {
        Some((_, name)) => (*name).into(),
        None => format!("ioctl {ioctl:#x}"),
    }
}

/// The render node of an AMD card. It is found by the vendor id on its PCI device, as the node
/// numbers change with what else is in the machine. With an address given, only the node of the card
/// at that address is accepted.
pub fn amd_render_node(address: Option<&str>) -> Option<PathBuf> {
    amd_render_node_in(Path::new(DRM_CLASS_FOLDER), Path::new(DRM_DEVICE_FOLDER), address)
}

/// The sysfs folder of a render node's PCI device, where what INFO leaves out can be read.
pub fn device_folder(node: &Path) -> Option<PathBuf> {
    Some(Path::new(DRM_CLASS_FOLDER).join(node.file_name()?).join("device"))
}

fn amd_render_node_in(class_folder: &Path, device_folder: &Path, address: Option<&str>) -> Option<PathBuf> {
    let mut names: Vec<_> = fs::read_dir(class_folder)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with(RENDER_NODE_PREFIX))
        .collect();
    names.sort();
    let is_amd = |name: &&std::ffi::OsString| {
        let vendor = fs::read_to_string(class_folder.join(name).join(VENDOR_FILE));
        vendor
            .is_ok_and(|text| u16::from_str_radix(text.trim().trim_start_matches("0x"), 16) == Ok(AMD_VENDOR))
    };
    let at_address = |name: &&std::ffi::OsString| {
        let Some(wanted) = address else { return true };
        let device = fs::canonicalize(class_folder.join(name).join("device"));
        device.ok().and_then(|path| Some(path.file_name()?.to_string_lossy() == *wanted)).unwrap_or(false)
    };
    names.iter().filter(at_address).find(is_amd).map(|name| device_folder.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::{env, process};

    fn refused(times: u32, then: io::Result<()>) -> impl FnMut() -> io::Result<()> {
        let mut left = times;
        let mut then = Some(then);
        move || {
            if left > 0 {
                left -= 1;
                return Err(io::Error::from_raw_os_error(libc::EAGAIN));
            }
            then.take().expect("called once more")
        }
    }

    #[test]
    fn a_call_refused_for_now_goes_through_on_a_later_try() {
        let tried = try_until_taken(refused(6, Ok(())), Duration::from_secs(5));
        assert!(tried.result.is_ok());
        assert_eq!(tried.refusals, 6);
    }

    #[test]
    fn a_call_refused_past_the_patience_comes_back_as_would_block() {
        let always = || Err(io::Error::from_raw_os_error(libc::EAGAIN));
        let tried = try_until_taken(always, Duration::from_millis(20));
        assert_eq!(tried.result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(tried.refusals > 1);
    }

    #[test]
    fn a_signal_is_tried_again_and_isnt_a_refusal() {
        let mut cut_short = true;
        let call = || {
            if std::mem::take(&mut cut_short) {
                return Err(io::Error::from_raw_os_error(libc::EINTR));
            }
            Ok(())
        };
        let tried = try_until_taken(call, Duration::ZERO);
        assert!(tried.result.is_ok());
        assert_eq!(tried.refusals, 0);
    }

    #[test]
    fn any_other_error_comes_back_at_once() {
        let tried = try_until_taken(
            refused(2, Err(io::Error::from_raw_os_error(libc::EINVAL))),
            Duration::from_secs(5),
        );
        assert_eq!(tried.result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
        assert_eq!(tried.refusals, 2);
    }

    #[test]
    fn the_log_names_a_call_in_words() {
        assert_eq!(what(call::cs()), "a submission");
        assert_eq!(what(call::gem_userptr()), "a buffer over guest memory");
        assert_eq!(what(0x1234), "ioctl 0x1234");
    }

    #[test]
    fn ioctl_numbers_follow_the_kernels_encoding() {
        // Checked against the header's own DRM_IOCTL_AMDGPU_* and DRM_IOCTL_SYNCOBJ_* macros.
        assert_eq!(call::gem_create(), 0xC020_6440);
        assert_eq!(call::gem_mmap(), 0xC008_6441);
        assert_eq!(call::ctx(), 0xC010_6442);
        assert_eq!(call::cs(), 0xC018_6444);
        assert_eq!(call::info(), 0x4020_6445);
        assert_eq!(call::gem_va(), 0x4040_6448);
        assert_eq!(call::gem_metadata(), 0xC120_6446);
        assert_eq!(call::gem_userptr(), 0xC018_6451);
        assert_eq!(call::gem_close(), 0x4008_6409);
        assert_eq!(call::prime_handle_to_fd(), 0xC00C_642D);
        assert_eq!(call::syncobj_create(), 0xC008_64BF);
        assert_eq!(call::syncobj_wait(), 0xC020_64C3);
        assert_eq!(call::syncobj_eventfd(), 0xC018_64CF);
    }

    fn class_folder(name: &str, nodes: &[(&str, &str, u16)]) -> (PathBuf, PathBuf) {
        let base = env::temp_dir().join(format!("guesthouse-amd-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let class = base.join("class");
        let dev = base.join("dev");
        fs::create_dir_all(&dev).unwrap();
        for (node, pci, vendor) in nodes {
            let device = base.join("devices").join(pci);
            fs::create_dir_all(&device).unwrap();
            fs::write(device.join("vendor"), format!("0x{vendor:04x}\n")).unwrap();
            fs::create_dir_all(class.join(node)).unwrap();
            symlink(&device, class.join(node).join("device")).unwrap();
        }
        (class, dev)
    }

    #[test]
    fn the_render_node_is_the_amd_one() {
        let (class, dev) = class_folder(
            "pick",
            &[("renderD128", "0000:7c:00.0", 0x1002), ("renderD129", "0000:02:00.0", 0x10DE)],
        );
        assert_eq!(amd_render_node_in(&class, &dev, None), Some(dev.join("renderD128")));
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }

    #[test]
    fn with_no_amd_card_there_is_no_node() {
        let (class, dev) = class_folder("none", &[("renderD128", "0000:02:00.0", 0x10DE)]);
        assert_eq!(amd_render_node_in(&class, &dev, None), None);
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_address_narrows_it_to_one_card() {
        let (class, dev) = class_folder(
            "addr",
            &[("renderD128", "0000:03:00.0", 0x1002), ("renderD129", "0000:04:00.0", 0x1002)],
        );
        assert_eq!(amd_render_node_in(&class, &dev, Some("0000:04:00.0")), Some(dev.join("renderD129")));
        assert_eq!(amd_render_node_in(&class, &dev, Some("0000:09:00.0")), None);
        fs::remove_dir_all(class.parent().unwrap()).unwrap();
    }
}
