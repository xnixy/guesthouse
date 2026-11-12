//! How the guest's memory reaches QEMU and the daemon. The app makes both parts as memfds before
//! either process starts, and each child finds them open at the same descriptor numbers. Nothing of
//! the guest is a file anyone can open by name, so `/dev/shm`'s size doesn't limit it, and the
//! memory goes back to the system once the last of QEMU and the daemon has gone.
//!
//! Each memfd is named like the file it replaces, as `gpupv-ram-2`, so a process holding a slot's
//! memory can still be told from `/proc`.

use crate::slot::Slot;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How the kernel shows a memfd in `/proc/PID/fd` and `/proc/PID/maps`.
const MEMFD_PREFIX: &str = "memfd:";
const MEMFD_SUFFIX: &str = " (deleted)";
const OWN_DESCRIPTORS: &str = "/proc/self/fd";
/// The app keeps its own copies at this number or above, past both parts' numbers, so leaving one
/// at its number in a child can never land on the other's copy.
pub const KEPT_ABOVE: RawFd = 64;

/// The two parts of a GPU guest's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The 64 GiB window at the top of the VMBus range, which holds the IO space. It fills only as
    /// far as the guest's locked allocations go.
    Window,
    /// The guest's RAM, all of it claimed before the start.
    Ram,
}

impl Part {
    pub const ALL: [Self; 2] = [Self::Window, Self::Ram];

    /// The word in its name, which is also the name its file had in `/dev/shm`.
    pub fn word(self) -> &'static str {
        match self {
            Self::Window => "iomem",
            Self::Ram => "ram",
        }
    }

    /// Where QEMU and the daemon find it open. Out of the way of the network's descriptors, 10 and
    /// 11, and of the unix sockets the jail's launcher opens from 20.
    pub fn descriptor(self) -> RawFd {
        match self {
            Self::Window => 12,
            Self::Ram => 13,
        }
    }

    /// The memfd's name for this slot.
    pub fn name(self, slot: Slot) -> String {
        slot.memory_name(self.word())
    }

    /// What a process holding it shows for it, in `/proc/PID/fd` as the link's target, and at the
    /// end of a line in `/proc/PID/maps`.
    pub fn shown_as(self, slot: Slot) -> String {
        format!("/{}", shown_file_name(&self.name(slot)))
    }

    /// The path QEMU opens it by. It's QEMU's own descriptor, so a jail that lets QEMU read `/proc`
    /// lets it reach this and nothing else of the kind.
    pub fn path_for_qemu(self) -> PathBuf {
        Path::new(OWN_DESCRIPTORS).join(self.descriptor().to_string())
    }

    /// Takes the part the app left open for this daemon, and keeps it from the programs the daemon
    /// runs.
    pub fn inherited(self, slot: Slot) -> io::Result<File> {
        self.take(slot, self.descriptor())
    }

    /// The same, from any descriptor. It must be the memfd of this part and this slot: anything
    /// else there was left open by mistake, and mapping it as the guest would be wrong.
    pub(crate) fn take(self, slot: Slot, descriptor: RawFd) -> io::Result<File> {
        let link = fs::read_link(Path::new(OWN_DESCRIPTORS).join(descriptor.to_string()));
        let wanted = self.shown_as(slot);
        if link.as_deref().ok() != Some(Path::new(&wanted)) {
            let found = link.map_or_else(|_| "nothing".to_owned(), |link| link.display().to_string());
            return Err(io::Error::other(format!(
                "the guest's {} should be open at descriptor {descriptor} as {wanted}, and {found} is there. \
                 Start the machine from Guesthouse. To run the daemon beside your own QEMU,                  see docs/standalone.md.",
                self.word()
            )));
        }
        // SAFETY: the descriptor is open, as its link was just read, and nothing else in this
        // process owns it: the app opened it for this one use.
        let file = unsafe { File::from_raw_fd(descriptor) };
        // SAFETY: setting a flag on a descriptor this process owns touches nothing else.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }
}

/// Leaves each part open in what this command starts, at the part's number. The copies passed here
/// must be at [`KEPT_ABOVE`] or past it.
pub fn leave_open(command: &mut Command, parts: [(&OwnedFd, Part); 2]) {
    let placed = parts.map(|(fd, part)| (fd, part.descriptor()));
    leave_open_at(command, &placed);
}

/// The same for any descriptors, such as the drives the app opens for QEMU. The numbers must be below
/// [`KEPT_ABOVE`].
pub fn leave_open_at(command: &mut Command, placed: &[(&OwnedFd, RawFd)]) {
    let placed: Vec<(RawFd, RawFd)> = placed.iter().map(|(fd, to)| (fd.as_raw_fd(), *to)).collect();
    assert!(placed.iter().all(|(from, _)| *from >= KEPT_ABOVE), "a copy is too low to place safely");
    assert!(placed.iter().all(|(_, to)| *to < KEPT_ABOVE), "a number is too high to be clear of the copies");
    // SAFETY: between fork and exec the closure only calls dup2, which is async-signal-safe and makes
    // a copy without the close-on-exec flag. The copies are above every part's number, so placing
    // one never overwrites another before it's placed. The caller keeps them open until the spawn.
    unsafe {
        command.pre_exec(move || {
            for &(from, to) in &placed {
                if libc::dup2(from, to) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// How a memfd of this name is shown, without the leading slash: the last part of the link.
pub fn shown_file_name(name: &str) -> String {
    format!("{MEMFD_PREFIX}{name}{MEMFD_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;

    fn memfd(name: &str) -> File {
        let name = CString::new(name).unwrap();
        // SAFETY: a fresh memfd, owned by the File from here on.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd >= 0);
        // SAFETY: just made, and owned by nothing else.
        unsafe { File::from_raw_fd(fd) }
    }

    fn slot(number: &str) -> Slot {
        Slot::parse(Some(number)).unwrap()
    }

    #[test]
    fn the_names_are_the_old_files_names() {
        assert_eq!(Part::Ram.name(slot("0")), "gpupv-ram");
        assert_eq!(Part::Window.name(slot("2")), "gpupv-iomem-2");
        assert_eq!(Part::Ram.shown_as(slot("1")), "/memfd:gpupv-ram-1 (deleted)");
        assert_eq!(Part::Window.path_for_qemu(), Path::new("/proc/self/fd/12"));
    }

    #[test]
    fn the_slots_memfd_is_taken_and_closes_on_exec() {
        let made = memfd("gpupv-ram-3");
        made.write_all_at(b"guest", 0).unwrap();
        let descriptor = made.as_raw_fd();
        std::mem::forget(made);
        let taken = Part::Ram.take(slot("3"), descriptor).unwrap();
        let mut read = [0; 5];
        taken.read_exact_at(&mut read, 0).unwrap();
        assert_eq!(&read, b"guest");
        // SAFETY: only reads the flags of a descriptor this test owns.
        let flags = unsafe { libc::fcntl(taken.as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
    }

    #[test]
    fn another_slots_memory_or_another_part_or_nothing_is_refused() {
        let other_slot = memfd("gpupv-ram-1");
        let error = Part::Ram.take(slot("2"), other_slot.as_raw_fd()).unwrap_err();
        assert!(error.to_string().contains("/memfd:gpupv-ram-2 (deleted)"), "{error}");
        assert!(Part::Window.take(slot("1"), other_slot.as_raw_fd()).is_err());
        let error = Part::Window.take(slot("1"), RawFd::MAX).unwrap_err();
        assert!(error.to_string().contains("nothing is there"), "{error}");
    }
}
