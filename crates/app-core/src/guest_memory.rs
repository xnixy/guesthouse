//! A GPU guest's memory, made before it starts: its RAM and the window QEMU and the daemon share, as
//! memfds that both are handed open. See [`guesthouse_daemon::handover`].
//!
//! RAM fills as the guest touches it, so without a claim a VM could pass every check and run the PC
//! out of memory later, when the GPU has pinned what it has and nothing can be swapped to make room.
//! All of it is claimed now instead, which takes about a second for 16 GiB, and a VM either has it or
//! doesn't start. The window is only sized: it fills as far as the guest's locked allocations go.

use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::host::{GIB, available_ram, fits_now, short_now};
use guesthouse_daemon::guest_memory::WINDOW_SIZE;
use guesthouse_daemon::handover::{self, Part};
use guesthouse_daemon::slot::Slot;
use rustix::fs::{
    FallocateFlags, MemfdFlags, SealFlags, fallocate, fcntl_add_seals, ftruncate, memfd_create,
};
use rustix::io::{Errno, fcntl_dupfd_cloexec};
use std::os::fd::{OwnedFd, RawFd};
use std::process::Command;

/// The window's size, which QEMU is told too.
pub const WINDOW_BYTES: u64 = WINDOW_SIZE;
pub const WINDOW_GIB: u64 = WINDOW_BYTES / GIB;
/// The memfds are kept above this in the app, so placing one at its number in a child can never
/// land on the other.
const KEPT_ABOVE: RawFd = handover::KEPT_ABOVE;
/// Neither QEMU nor the daemon may change the size: the other would fault on what went.
const SEALS: SealFlags = SealFlags::SHRINK.union(SealFlags::GROW).union(SealFlags::SEAL);
pub const NO_ROOM_WORDS: &str = "GiB is free";

/// Whether a failed start was only for want of memory.
pub fn is_no_room(failure: &Failure) -> bool {
    failure.0.contains(NO_ROOM_WORDS)
}

/// The two memfds, which close here once QEMU and the daemon have their own.
#[derive(Debug)]
pub struct GuestMemory {
    window: OwnedFd,
    ram: OwnedFd,
}

impl GuestMemory {
    /// Makes both and claims all of the RAM. A failure leaves nothing behind.
    pub fn claim(context: &Context, slot: Slot, ram_bytes: u64) -> Result<Self> {
        // The kernel would take what it can't have from everything else on the PC, a page at a time.
        if let Some(short) = fits_now(context, ram_bytes) {
            return Err(Failure(short));
        }
        let memory = Self::sized(context, slot, ram_bytes)?;
        fallocate(&memory.ram, FallocateFlags::empty(), 0, ram_bytes).map_err(|error| match error {
            Errno::NOMEM | Errno::NOSPC => Failure(short_now(ram_bytes, available_ram(context))),
            other => Failure(format!("The VM's memory could not be reserved: {other}")),
        })?;
        memory.seal()?;
        Ok(memory)
    }

    /// Makes both at their sizes and claims nothing, for a test.
    pub fn sized(_context: &Context, slot: Slot, ram_bytes: u64) -> Result<Self> {
        Ok(Self { window: part(Part::Window, slot, WINDOW_BYTES)?, ram: part(Part::Ram, slot, ram_bytes)? })
    }

    fn seal(&self) -> Result<()> {
        for fd in [&self.window, &self.ram] {
            fcntl_add_seals(fd, SEALS).map_err(std::io::Error::from)?;
        }
        Ok(())
    }

    /// Leaves both open in what this command starts, at their numbers. Everything else the app
    /// opens closes on exec as before.
    pub fn hand_to(&self, command: &mut Command) {
        handover::leave_open(command, [(&self.window, Part::Window), (&self.ram, Part::Ram)]);
    }
}

/// One part, at its size, closed on exec, and kept above the numbers the children see it at.
fn part(part: Part, slot: Slot, bytes: u64) -> Result<OwnedFd> {
    let made = memfd_create(part.name(slot), MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
        .map_err(std::io::Error::from)?;
    ftruncate(&made, bytes).map_err(std::io::Error::from)?;
    Ok(fcntl_dupfd_cloexec(&made, KEPT_ABOVE).map_err(std::io::Error::from)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::process::Stdio;

    const KIB: u64 = 1024;

    fn context_with_free(scratch: &Scratch, kib: u64) -> Context {
        let mut context = scratch_context(scratch);
        context.ram_limit = None;
        context.meminfo = scratch.write("meminfo", format!("MemTotal: {kib} kB\nMemAvailable: {kib} kB\n"));
        context
    }

    fn file_of(fd: &OwnedFd) -> fs::File {
        fs::File::from(fd.try_clone().unwrap())
    }

    #[test]
    fn the_ram_is_claimed_at_once_and_the_window_only_sized() {
        let scratch = Scratch::new();
        let context = context_with_free(&scratch, 64 * KIB * KIB);
        let memory = GuestMemory::claim(&context, slots::number(2), 1 << 20).unwrap();
        let ram = file_of(&memory.ram).metadata().unwrap();
        assert_eq!(ram.len(), 1 << 20);
        assert!(ram.blocks() * 512 >= 1 << 20, "it must take up its room now, and not later");
        let window = file_of(&memory.window).metadata().unwrap();
        assert_eq!((window.len(), window.blocks()), (WINDOW_BYTES, 0));
        let link = fs::read_link(format!("/proc/self/fd/{}", memory.ram.as_raw_fd())).unwrap();
        assert_eq!(link.to_string_lossy(), Part::Ram.shown_as(slots::number(2)));
    }

    #[test]
    fn neither_size_can_change_once_claimed() {
        let scratch = Scratch::new();
        let context = context_with_free(&scratch, 64 * KIB * KIB);
        let memory = GuestMemory::claim(&context, slots::first(), 1 << 20).unwrap();
        assert!(ftruncate(&memory.ram, 0).is_err());
        assert!(ftruncate(&memory.window, WINDOW_BYTES + GIB).is_err());
    }

    #[test]
    fn more_than_the_pc_has_free_is_refused_before_any_is_taken() {
        let scratch = Scratch::new();
        let context = context_with_free(&scratch, 4 * KIB * KIB);
        let error = GuestMemory::claim(&context, slots::first(), 16 * GIB).unwrap_err();
        assert!(is_no_room(&error), "{error}");
        assert!(error.0.contains("needs 16 GiB of RAM, and 4 GiB is free right now"), "{error}");
    }

    #[test]
    fn a_child_finds_both_at_their_numbers_and_nothing_else_of_ours() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let slot = slots::number(1);
        let memory = GuestMemory::sized(&context, slot, 1 << 20).unwrap();
        let mut command = Command::new("sh");
        command.args(["-c", "readlink /proc/self/fd/12 /proc/self/fd/13; ls /proc/self/fd"]);
        memory.hand_to(command.stdout(Stdio::piped()));
        let said = String::from_utf8(command.output().unwrap().stdout).unwrap();
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines[..2], [Part::Window.shown_as(slot), Part::Ram.shown_as(slot)]);
        let own = [memory.window.as_raw_fd(), memory.ram.as_raw_fd()].map(|fd| fd.to_string());
        assert!(
            !lines[2..].iter().any(|fd| own.contains(&fd.to_string())),
            "the app's own copies close on exec"
        );
    }
}
