//! Waits the GPU can interrupt, instead of ones the daemon has to keep looking at.
//!
//! A semaphore surface is the RM's monitored-fence object: slots in host memory whose counts
//! engines raise, and waiters the kernel posts to a file descriptor when a bound engine raises its
//! non-stall interrupt. The daemon — or anything else driving the card — arms a waiter for a count
//! it wants, points a release at the slot, and sleeps on the descriptor until the interrupt says
//! the slot may have moved. A poll that times out is the backstop: it re-reads the slots the same
//! way, so a lost wakeup costs the timeout and nothing else.
//!
//! The layout of every request is in the open kernel module's public headers. Slots are 32 bytes,
//! with the count at the start; this generation's counts are 64 bits, so the surface is made
//! without the max-submitted half of the pair, which the driver refuses when it takes 64-bit
//! semaphores.

use crate::rm::classes::{
    NOTIFY_COPY, NOTIFY_FIFO_METHODS, NOTIFY_GRAPHICS, SEMAPHORE_SURFACE, SURFACE_BIND_CHANNEL,
    SURFACE_REGISTER_WAITER, SURFACE_UNBIND_CHANNEL, SURFACE_UNREGISTER_WAITER,
};
use crate::rm::device::{AddressSpace, Device, Mapping};
use crate::rm::{Handle, Rm, RmError, STATUS_ALREADY_SIGNALLED, STATUS_OK};

/// One slot, in bytes: the count at the start, the kernel's threshold in the middle.
const SLOT: usize = 32;
/// How many slots a page holds.
const SLOTS: u32 = 4096 / SLOT as u32;

/// The surface, its memory, and what hangs from it.
pub struct Wakes {
    memory: Handle,
    surface: Handle,
    bound: Vec<Handle>,
    armed: Vec<(u32, u64)>,
    gpu_address: u64,
}

impl std::fmt::Debug for Wakes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wakes").field("surface", &self.surface).finish_non_exhaustive()
    }
}

impl Wakes {
    /// Makes a surface over fresh host pages mapped at `gpu_address` in the space, and tells the
    /// driver to post this client's waiters on the descriptor [`Rm::wake_fd`] names. The surface
    /// hangs from `parent`: the device on this driver.
    pub fn new<R: Rm>(
        device: &mut Device<R>,
        space: &AddressSpace,
        gpu_address: u64,
        parent: Handle,
    ) -> Result<Self, RmError> {
        device.rm.alloc_os_event(device.device)?;
        let memory = device.alloc_sysmem(4096)?;
        device.map(
            space,
            &Mapping {
                memory,
                offset: 0,
                length: 4096,
                address: gpu_address,
                small_pages: true,
                gpu_cached: false,
                page_kind: None,
            },
        )?;
        let mut params = [0; 16];
        params[0..4].copy_from_slice(&memory.to_le_bytes());
        let surface = device.alloc(parent, SEMAPHORE_SURFACE, &mut params)?;
        Ok(Self { memory, surface, bound: Vec::new(), armed: Vec::new(), gpu_address })
    }

    /// Binds a channel, so its engine's non-stall interrupts re-check the waiters. A copy channel
    /// takes the copy notifier; anything else takes the graphics one.
    pub fn bind<R: Rm>(
        &mut self,
        device: &mut Device<R>,
        channel: Handle,
        copy: bool,
    ) -> Result<(), RmError> {
        let mut params = [0; 40];
        params[0..4].copy_from_slice(&channel.to_le_bytes());
        // The runlist reports a channel's semaphore interrupt on the FIFO's index, and the engine
        // itself on its own, so a channel takes both.
        params[4..8].copy_from_slice(&2u32.to_le_bytes());
        params[8..12].copy_from_slice(&NOTIFY_FIFO_METHODS.to_le_bytes());
        let index = u32::from(!copy) * NOTIFY_GRAPHICS + u32::from(copy) * NOTIFY_COPY;
        params[12..16].copy_from_slice(&index.to_le_bytes());
        let status = device.rm.control(self.surface, SURFACE_BIND_CHANNEL, &mut params)?;
        RmError::check(|| "binding a channel to the surface".to_owned(), status)?;
        self.bound.push(channel);
        Ok(())
    }

    /// Arms a one-shot waiter for slot `index` reaching `value`. `Ok(false)` says the count is
    /// already there and there is nothing to wait for.
    pub fn arm<R: Rm>(
        &mut self,
        device: &mut Device<R>,
        index: u32,
        value: u64,
        fd: i32,
    ) -> Result<bool, RmError> {
        let mut params = [0; 32];
        params[0..4].copy_from_slice(&index.to_le_bytes());
        params[8..16].copy_from_slice(&value.to_le_bytes());
        params[24..28].copy_from_slice(&(fd as u32).to_le_bytes());
        let status = device.rm.control(self.surface, SURFACE_REGISTER_WAITER, &mut params)?;
        if status == STATUS_ALREADY_SIGNALLED {
            return Ok(false);
        }
        if status != STATUS_OK {
            return Err(RmError::refused(format!("arming slot {index}"), status));
        }
        self.armed.push((index, value));
        Ok(true)
    }

    /// The GPU address of a slot, for a release to write.
    pub fn slot_address(&self, index: u32) -> u64 {
        self.gpu_address + u64::from(index) * SLOT as u64
    }

    /// A Wakes over pieces a probe made itself. The surface's own constructor is what `new`
    /// runs; this is for asking the mechanism questions one step at a time.
    pub fn from_parts(memory: Handle, surface: Handle, gpu_address: u64) -> Self {
        Self { memory, surface, bound: Vec::new(), armed: Vec::new(), gpu_address }
    }

    /// The surface's own handle, for a probe that talks to it directly.
    pub fn surface_control(&self) -> Handle {
        self.surface
    }

    /// How many slots the page holds.
    pub fn slots(&self) -> u32 {
        SLOTS
    }

    /// Takes the waiters off and the channels away, and frees the surface and its memory. Run this
    /// before the channels and the space are freed, so the unbinds name live channels.
    pub fn close<R: Rm>(&mut self, device: &mut Device<R>) -> Result<(), RmError> {
        for (index, value) in self.armed.drain(..) {
            let mut params = [0; 32];
            params[0..4].copy_from_slice(&index.to_le_bytes());
            params[8..16].copy_from_slice(&value.to_le_bytes());
            let _ = device.rm.control(self.surface, SURFACE_UNREGISTER_WAITER, &mut params);
        }
        for channel in self.bound.drain(..) {
            let mut params = [0; 40];
            params[0..4].copy_from_slice(&channel.to_le_bytes());
            params[4..8].copy_from_slice(&1u32.to_le_bytes());
            params[8..12].copy_from_slice(&NOTIFY_COPY.to_le_bytes());
            let _ = device.rm.control(self.surface, SURFACE_UNBIND_CHANNEL, &mut params);
        }
        let surface = self.surface;
        device.free(surface)?;
        let memory = self.memory;
        device.free(memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_holds_a_hundred_and_twenty_eight_slots() {
        assert_eq!(SLOTS, 128);
    }

    #[test]
    fn a_slot_address_steps_by_the_slot() {
        let wakes = Wakes::from_parts(1, 2, 0x400_0000);
        assert_eq!(wakes.slot_address(0), 0x400_0000);
        assert_eq!(wakes.slot_address(3), 0x400_0000 + 3 * SLOT as u64);
    }
}
