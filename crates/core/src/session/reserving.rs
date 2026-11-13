//! Handlers for setting GPU addresses aside.
//!
//! Most requests leave the place to the host, and take the next addresses as anything else does.
//! A compute runtime names the address, as it uses the same number on the CPU and the GPU. Those
//! are per process, so two processes can hold the same range. Hand-outs stay clear of all of them,
//! as an address the host chooses has to work in whichever process it ends up in.

use super::{Handled, Session, answer_status};
use crate::deferred::DeferredAction;
use crate::gpu_addresses::{ALLOCATION_ALIGN, AddressUse, GPU_RANGES, Window};
use crate::tables::ReservedRange;
use guesthouse_backend::{Backend, GuestMemory, SpaceId};
use guesthouse_messages::address::{ReserveRequest, freed_address};
use guesthouse_messages::wire::{align_up, put_u64};
use guesthouse_messages::{MessageHeader, Status};

/// The answer has no status, so an address of zero is how a reservation fails.
const NO_ADDRESS: u64 = 0;

fn window_of(request: &ReserveRequest) -> Window {
    let to = if request.limit == 0 { u64::MAX } else { request.limit };
    Window { from: request.lowest, to }
}

fn in_one_range(base: u64, size: u64) -> bool {
    GPU_RANGES.iter().any(|range| range.contains(base) && size <= range.end - base)
}

impl<B: Backend, M: GuestMemory> Session<B, M> {
    /// What the guest or the host already has in this space, as (start, end). That includes the
    /// mappings of an allocation the guest has freed but whose unmap is still waiting for the GPU:
    /// they stay mapped in the backend until then.
    pub(super) fn taken_in(&self, space: SpaceId) -> Vec<(u64, u64)> {
        let reserved = self.reserved_addresses.iter().filter(|range| range.space == space);
        let live = self.allocations.values().flat_map(|allocation| &allocation.mappings);
        let freed = self.deferred.iter().filter_map(|item| match &item.action {
            DeferredAction::FreeAllocation(freed) => Some(freed),
            _ => None,
        });
        let waiting = freed.flat_map(|freed| &freed.mappings);
        let mapped = live.chain(waiting).filter(|mapping| mapping.space == space);
        reserved
            .map(|range| (range.address, range.address + range.size))
            .chain(mapped.map(|mapping| (mapping.address, mapping.address + mapping.length)))
            .chain(self.gpu_addresses.host_ranges().iter().copied())
            .collect()
    }

    /// The exact range asked for, if it is in the ranges and free in this space.
    fn place_at(&mut self, base: u64, size: u64, space: SpaceId) -> Option<u64> {
        if !in_one_range(base, size) {
            return None;
        }
        let free = !self.taken_in(space).iter().any(|&(start, end)| start < base + size && base < end);
        free.then(|| self.place(base, size))
    }

    fn place(&mut self, base: u64, size: u64) -> u64 {
        self.gpu_addresses.place(base, size);
        base
    }

    /// Somewhere in the window. Returns the address, and whether it was placed and not handed out.
    fn find_room(&mut self, window: Window, size: u64, space: SpaceId) -> Option<(u64, bool)> {
        let usage = AddressUse::Reserved;
        if let Some(base) = self.gpu_addresses.take_within(size, ALLOCATION_ALIGN, usage, window) {
            return Some((base, false));
        }
        if !self.gpu_addresses.reaches(window) {
            // Some drivers ask for addresses past what any space here covers, and work anywhere.
            log::warn!("a reservation asked for {window:x?}, past every range, and gets another place");
            return self.gpu_addresses.take(size, ALLOCATION_ALIGN, usage).ok().map(|base| (base, false));
        }
        let taken = self.taken_in(space);
        let base = self.gpu_addresses.find_gap(window, size, ALLOCATION_ALIGN, &taken)?;
        Some((self.place(base, size), true))
    }

    pub(super) fn reserve_gpu_addresses(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = ReserveRequest::parse(message)?;
        let space = self.space_for(MessageHeader::process_of(message)?)?;
        let found = request.size.checked_add(ALLOCATION_ALIGN).and_then(|_| {
            let size = align_up(request.size, ALLOCATION_ALIGN);
            let found = match request.base {
                0 => self.find_room(window_of(&request), size, space),
                base => self.place_at(base, size, space).map(|base| (base, true)),
            };
            found.map(|(address, placed)| ReservedRange { address, size, space, placed })
        });
        let address = found.map_or(NO_ADDRESS, |range| range.address);
        log::debug!("reservation {request:x?} got {address:#x}");
        match found {
            Some(range) => self.reserved_addresses.push(range),
            None => log::warn!("refused a reservation: {request:x?}"),
        }
        put_u64(out, address);
        put_u64(out, 0);
        Ok(())
    }

    /// Takes back only what this process reserved here, and not until the GPU has finished with it.
    pub(super) fn free_gpu_addresses(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let address = freed_address(message)?;
        let space = self.space_for(MessageHeader::process_of(message)?)?;
        let held = |range: &ReservedRange| range.address == address && range.space == space;
        if let Some(index) = self.reserved_addresses.iter().position(held) {
            let range = self.reserved_addresses.remove(index);
            self.defer(DeferredAction::GiveBackReserved(range));
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// Lets a reservation's addresses be used again.
    pub(crate) fn give_back_reserved(&mut self, range: ReservedRange) {
        if range.placed {
            self.gpu_addresses.unplace(range.address, range.size);
        } else {
            self.gpu_addresses.give_back(range.address, range.size, ALLOCATION_ALIGN, AddressUse::Reserved);
        }
    }
}
