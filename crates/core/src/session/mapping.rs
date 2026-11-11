//! Handlers for GPU addresses.

use super::{Handled, Session, answer_status};
use crate::deferred::{DeferredAction, FreedAllocation};
use crate::gpu_addresses::{ADDRESSES_END, ALLOCATION_ALIGN, AddressUse, GpuAddresses, LARGE_ALIGN, Window};
use crate::pending::Pending;
use crate::tables::Mapping;
use guesthouse_backend::{Backend, BackendError, GuestMemory, MapRequest, MemoryId, SpaceId, UnmapRequest};
use guesthouse_messages::address::{
    MapRequest as GuestMap, PAGE_SIZE, UpdateOperation, UpdateRequest, two_values_and_status,
};
use guesthouse_messages::wire::{Malformed, align_up, checked_align_up};
use guesthouse_messages::{MessageHeader, Status};

/// From this size an allocation can use 2 MiB pages.
const LARGE_PAGES_FROM: u64 = 2 << 20;

fn map_answer(address: u64, status: Status, out: &mut Vec<u8>) {
    two_values_and_status(address, 0, status, out);
}

/// Where a mapping starts in its allocation, how long it is, and where it ends, in bytes. `None` if
/// it couldn't fit in any address space, which keeps later sums on these well inside 64 bits.
fn mapped_span(request: &GuestMap, allocation_size: u64) -> Option<(u64, u64, u64)> {
    let first = request.first_page.checked_mul(PAGE_SIZE)?;
    let length = if request.pages != 0 { request.pages.checked_mul(PAGE_SIZE)? } else { allocation_size };
    let end = first.checked_add(length).filter(|&end| end <= ADDRESSES_END)?;
    Some((first, length, end))
}

/// Where the guest will take a mapping the host places.
fn map_window(request: &GuestMap) -> Window {
    let to = if request.limit == 0 { u64::MAX } else { request.limit };
    Window { from: request.lowest, to }
}

/// The end of a range from an address update.
fn range_end(address: u64, size: u64) -> Result<u64, BackendError> {
    address.checked_add(size).ok_or(RANGE_TOO_LONG)
}

impl<B: Backend, M: GuestMemory> Session<B, M> {
    pub(super) fn map_gpu_addresses(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = GuestMap::parse(message)?;
        let handle = request.allocation;
        if handle == 0 {
            map_answer(request.address, Status::SUCCESS, out);
            return Ok(());
        }
        let Some(memory) = self.allocations.get(&handle).and_then(|allocation| allocation.memory) else {
            map_answer(0, Status::INVALID_PARAMETER, out);
            return Ok(());
        };
        let space = self.space_for(MessageHeader::process_of(message)?)?;
        let allocation = &self.allocations[&handle];
        let Some((first, length, needed)) = mapped_span(&request, allocation.size) else {
            map_answer(0, Status::INVALID_PARAMETER, out);
            return Ok(());
        };
        let mut memory = memory;
        // Guest pages already hold what the guest wrote. Replacing them with fresh video memory
        // leaves this address pointing at bytes the guest never filled, which is what made the
        // encoder reject its picture setup: the setup is 0x2800 bytes, and the guest maps it in
        // whole pages, which is more than that.
        let guest_pages = allocation.view.is_some();
        if needed > allocation.size
            && allocation.guest_address.is_none()
            && allocation.mappings.is_empty()
            && !guest_pages
        {
            // A vendor's size can be too small, and nothing is stored here yet.
            self.backend.release_memory(memory);
            let allocation = self.allocations.get_mut(&handle).expect("read above");
            allocation.memory = None;
            let (grown, size) = self.backend.allocate_video_memory(needed, allocation.kind)?;
            allocation.memory = Some(grown);
            allocation.size = size;
            memory = grown;
        }
        if request.address != 0 {
            // A guest-chosen address, usually in a reserved range: map only the pages it asked for.
            log::debug!("map {handle:#x} at {:#x}: {first:#x} for {length:#x}", request.address);
            let chosen = self.map_chosen(request.address, memory, first, length, space, handle);
            if let Err(error) = chosen {
                log::warn!("mapping {handle:#x} at {:#x}: {error}", request.address);
                map_answer(0, Status::UNSUCCESSFUL, out);
                return Ok(());
            }
            self.add_mapping(handle, Mapping { address: request.address, first, length, space });
            map_answer(request.address, Status::SUCCESS, out);
            return Ok(());
        }
        let allocation = &self.allocations[&handle];
        if let Some(mine) = allocation.mappings.iter().find(|mapping| mapping.space == space) {
            // This takes the mapping to start at the allocation's first byte, which one at a
            // guest-chosen address may not.
            map_answer(mine.address + first, Status::SUCCESS, out);
            return Ok(());
        }
        // Mapping is only possible in whole pages of the size in use, and the guest asks for single
        // small ones. So each allocation is mapped whole, once, and addresses come from inside that.
        let size = allocation.size;
        // The adopted guest pages cover whole 4 KiB pages, which is more than the exact size and
        // less than a big page. Map those pages, not a new allocation.
        let (mapped, small_pages) = match allocation.view {
            Some(view) => (align_up(size, PAGE_SIZE).min(view.size), true),
            None => (size, false),
        };
        let align = if size >= LARGE_PAGES_FROM { LARGE_ALIGN } else { ALLOCATION_ALIGN };
        let whole = align_up(size, ALLOCATION_ALIGN);
        let usage = AddressUse::mapped(allocation.in_system_memory());
        let Some(base) = self.choose_address(whole, align, usage, map_window(&request), space) else {
            log::warn!("no room for {handle:#x} in {:x?}", map_window(&request));
            map_answer(0, Status::UNSUCCESSFUL, out);
            return Ok(());
        };
        log::debug!("map {handle:#x} at {base:#x}, chosen here, for {mapped:#x}");
        let allocation = &self.allocations[&handle];
        let gpu_cached = allocation.gpu_cached;
        let map = MapRequest {
            space,
            memory,
            offset: 0,
            length: mapped,
            address: base,
            small_pages,
            gpu_cached,
            readable: true,
            writable: true,
            executable: false,
            memory_type: guesthouse_backend::MapMemoryType::Default,
        };
        if let Err(error) = self.backend.map(&map) {
            log::warn!("mapping {handle:#x}: {error}");
            self.release_address(base, whole, align, usage);
            map_answer(0, Status::UNSUCCESSFUL, out);
            return Ok(());
        }
        self.mapped_addresses.insert(base, (whole, align));
        self.add_mapping(handle, Mapping { address: base, first: 0, length: mapped, space });
        map_answer(base + first, Status::SUCCESS, out);
        Ok(())
    }

    /// Notes a mapping. Which allocation was mapped first decides the order they are closed in.
    fn add_mapping(&mut self, handle: u32, mapping: Mapping) {
        let allocation = self.allocations.get_mut(&handle).expect("the caller has just used it");
        if allocation.first_mapped.is_none() {
            allocation.first_mapped = Some(self.mapped_so_far);
            self.mapped_so_far += 1;
        }
        allocation.mappings.push(mapping);
    }

    /// Frees the addresses of a mapping that has gone, if the host chose them. `system_memory` says
    /// what was behind it at the end, which a first lock can have changed since it was mapped.
    pub(crate) fn give_back_mapping(&mut self, address: u64, system_memory: bool) {
        if let Some((size, align)) = self.mapped_addresses.remove(&address) {
            self.release_address(address, size, align, AddressUse::mapped(system_memory));
        }
    }

    /// Addresses for a mapping the host places, inside the guest's window. New addresses are
    /// handed out in order, so a window below the next new one is searched in this space alone,
    /// and what is found there is placed, so nothing is handed out over it.
    fn choose_address(
        &mut self,
        size: u64,
        align: u64,
        usage: AddressUse,
        window: Window,
        space: SpaceId,
    ) -> Option<u64> {
        if let Some(base) = self.gpu_addresses.take_within(size, align, usage, window) {
            return Some(base);
        }
        let taken = self.taken_in(space);
        let base = self.gpu_addresses.find_gap(window, size, align, &taken)?;
        self.gpu_addresses.place(base, size);
        self.placed_mappings.insert(base, size);
        Some(base)
    }

    /// The opposite of [`Session::choose_address`].
    fn release_address(&mut self, base: u64, size: u64, align: u64, usage: AddressUse) {
        match self.placed_mappings.remove(&base) {
            Some(placed) => self.gpu_addresses.unplace(base, placed),
            None => self.gpu_addresses.give_back(base, size, align, usage),
        }
    }

    /// Maps at an address the guest chose. If it is taken, the guest has finished with what is there:
    /// drivers manage pools of addresses themselves, and reuse one without saying it is free.
    fn map_chosen(
        &mut self,
        address: u64,
        memory: MemoryId,
        offset: u64,
        length: u64,
        space: SpaceId,
        handle: u32,
    ) -> Result<(), BackendError> {
        if !GpuAddresses::in_a_range(address) {
            return Err(BackendError::new("no GPU address range holds this"));
        }
        let gpu_cached = self.allocations[&handle].gpu_cached;
        let map = MapRequest {
            space,
            memory,
            offset,
            length,
            address,
            small_pages: true,
            gpu_cached,
            readable: true,
            writable: true,
            executable: false,
            memory_type: guesthouse_backend::MapMemoryType::Default,
        };
        if self.backend.map(&map).is_ok() {
            return Ok(());
        }
        self.clear_gpu_addresses(address, length, space)?;
        self.backend.map(&map)
    }

    /// Maps part of an allocation, at an address the guest chose earlier.
    fn map_piece(
        &mut self,
        address: u64,
        length: u64,
        handle: u32,
        offset: u64,
        space: SpaceId,
    ) -> Result<(), BackendError> {
        let allocation = self.allocations.get(&handle).ok_or(UNKNOWN_ALLOCATION)?;
        let memory = allocation.memory.ok_or(UNKNOWN_ALLOCATION)?;
        let gpu_cached = allocation.gpu_cached;
        self.backend.map(&MapRequest {
            space,
            memory,
            offset,
            length,
            address,
            small_pages: true,
            gpu_cached,
            readable: true,
            writable: true,
            executable: false,
            memory_type: guesthouse_backend::MapMemoryType::Default,
        })?;
        self.add_mapping(handle, Mapping { address, first: offset, length, space });
        Ok(())
    }

    /// Finds a live mapping that overlaps a range, and its allocation.
    fn overlapping(&self, address: u64, size: u64, space: SpaceId) -> Option<(u32, Mapping)> {
        self.allocations.iter().find_map(|(&handle, allocation)| {
            let mut mappings = allocation.mappings.iter();
            let found = mappings.find(|mapping| mapping.space == space && mapping.overlaps(address, size))?;
            Some((handle, *found))
        })
    }

    /// Unmaps a range. Only whole mappings can be removed, so any part outside is mapped again.
    fn unmap_gpu_addresses(&mut self, address: u64, size: u64, space: SpaceId) -> Result<(), BackendError> {
        let end = range_end(address, size)?;
        while let Some((handle, found)) = self.overlapping(address, size, space) {
            self.log_unmap_in_flight("an address change", found.address, found.length);
            let allocation = self.allocations.get_mut(&handle).expect("just found");
            let memory = allocation.memory.ok_or(UNKNOWN_ALLOCATION)?;
            self.backend.unmap(&UnmapRequest {
                space,
                memory,
                address: found.address,
                length: found.length,
            })?;
            allocation.mappings.retain(|mapping| *mapping != found);
            if found.address < address {
                self.map_piece(found.address, address - found.address, handle, found.first, space)?;
            }
            let found_end = found.address + found.length;
            if end < found_end {
                self.map_piece(end, found_end - end, handle, found.first + (end - found.address), space)?;
            }
        }
        Ok(())
    }

    /// Empties a range, including anything still waiting for the GPU before it is unmapped.
    fn clear_gpu_addresses(&mut self, address: u64, size: u64, space: SpaceId) -> Result<(), BackendError> {
        self.unmap_gpu_addresses(address, size, space)?;
        let in_range = |freed: &FreedAllocation| {
            freed.mappings.iter().any(|mapping| mapping.space == space && mapping.overlaps(address, size))
        };
        let (due, kept) = std::mem::take(&mut self.deferred).into_iter().partition(
            |item| matches!(&item.action, DeferredAction::FreeAllocation(freed) if in_range(freed)),
        );
        self.deferred = kept;
        let due: Vec<crate::deferred::Deferred> = due;
        due.into_iter().for_each(|item| self.run_deferred_action(item.action));
        Ok(())
    }

    /// The mapping holding an address: its allocation and where it is.
    fn mapping_at(&self, address: u64, space: SpaceId) -> Option<(u32, Mapping)> {
        self.allocations.iter().find_map(|(&handle, allocation)| {
            let mut mappings = allocation.mappings.iter();
            let found = mappings.find(|mapping| mapping.space == space && mapping.contains(address))?;
            Some((handle, *found))
        })
    }

    /// The first mapping that starts in a range, for walking past the gaps in it.
    fn next_mapping_start(&self, address: u64, end: u64, space: SpaceId) -> Option<u64> {
        let mappings = self.allocations.values().flat_map(|allocation| &allocation.mappings);
        // An empty mapping holds no address, so stopping at one would never get past it.
        let in_space = mappings.filter(|mapping| mapping.space == space && mapping.length != 0);
        in_space.map(|mapping| mapping.address).filter(|start| (address..end).contains(start)).min()
    }

    /// Maps at `destination` what's mapped at `source`, leaving the gaps as they are.
    fn copy_mappings(
        &mut self,
        source: u64,
        size: u64,
        destination: u64,
        space: SpaceId,
    ) -> Result<(), BackendError> {
        let source_end = range_end(source, size)?;
        let destination_end = range_end(destination, size)?;
        if source < destination_end && destination < source_end {
            // Each piece would be something more to copy, and clearing the way first loses the source.
            return Err(OVERLAPPING_COPY);
        }
        self.unmap_gpu_addresses(destination, size, space)?;
        let mut done = 0;
        while done < size {
            let Some((handle, found)) = self.mapping_at(source + done, space) else {
                // Whole pages at a time, as the pages of a gap can't hold a piece of anything.
                let Some(next) = self.next_mapping_start(source + done, source_end, space) else {
                    return Ok(());
                };
                done = checked_align_up(next - source, PAGE_SIZE).ok_or(RANGE_TOO_LONG)?;
                continue;
            };
            let piece = (size - done).min(found.address + found.length - source - done);
            let offset = found.first + (source + done - found.address);
            self.map_piece(destination + done, piece, handle, offset, space)?;
            done += piece;
        }
        Ok(())
    }

    fn apply_update(&mut self, operation: UpdateOperation, space: SpaceId) -> Result<(), BackendError> {
        match operation {
            UpdateOperation::Map { address, size, allocation, offset } => {
                let end = range_end(address, size)?;
                range_end(offset, size)?;
                if let Some((handle, current)) = self.mapping_at(address, space)
                    && handle == allocation
                    && current.first + (address - current.address) == offset
                    && end <= current.address + current.length
                {
                    // Already there. Only page attributes could differ, and those stay at the default.
                    return Ok(());
                }
                self.unmap_gpu_addresses(address, size, space)?;
                self.map_piece(address, size, allocation, offset, space)
            }
            UpdateOperation::Unmap { address, size } => self.unmap_gpu_addresses(address, size, space),
            UpdateOperation::Copy { source, size, destination } => {
                self.copy_mappings(source, size, destination, space)
            }
            UpdateOperation::Unknown(kind) => {
                log::warn!("unknown address update {kind}");
                Ok(())
            }
        }
    }

    fn apply_updates(&mut self, operations: &[u8], space: SpaceId) -> Result<(), super::Failure> {
        for bytes in operations.chunks(UpdateRequest::OPERATION_SIZE) {
            let operation = UpdateOperation::parse(bytes)?;
            if matches!(operation, UpdateOperation::Map { .. }) {
                log::debug!("update {operation:x?}, protection {:02x?}", UpdateOperation::protection(bytes));
            }
            self.apply_update(operation, space)?;
        }
        Ok(())
    }

    /// Runs an update that waited, and raises its fence for the next.
    pub(crate) fn finish_update(&mut self, operations: &[u8], space: SpaceId, fence: u32, value: u64) {
        log::debug!("waiting update runs in {space:?}: {}", describe_operations(operations));
        if let Err(failure) = self.apply_updates(operations, space) {
            log::error!("a waiting address update failed: {failure}");
            self.stop_space(space);
        }
        self.write_fence(fence, value.saturating_add(1));
    }

    /// Says so when work starts in a space whose address updates are still waiting, as the GPU may
    /// reach addresses that are not mapped yet. Only logs.
    pub(crate) fn note_waiting_updates(&self, queue: u32) {
        let Some(space) = self.queues.get(&queue).map(|queue| queue.space) else { return };
        let mut waiting = self.pending.iter().filter_map(|item| match item {
            Pending::Update { fence, value, space: waits, operations } if *waits == space => {
                Some((*fence, *value, operations))
            }
            _ => None,
        });
        if let Some((fence, value, operations)) = waiting.next() {
            let more = waiting.count();
            log::debug!(
                "work starts on queue {queue:#x} in {space:?} (gated: {}) with {} address updates still waiting;                  the first needs fence {fence:#x} to reach {value}: {}",
                self.gates.contains_key(&queue),
                more + 1,
                describe_operations(operations)
            );
        }
    }

    /// Takes every queue of an address space off the GPU, after an update that didn't happen.
    ///
    /// The guest has no way to hear this: the update's own reply went long ago, and it believes the
    /// addresses are there. Its commands are already on the GPU behind the update's fence, so holding
    /// the fence down only lasts until a queue times out and that raises it. Closing the channels
    /// first means nothing of this process can run, so its fences can go up, and the guest isn't left
    /// waiting. The app draws nothing from here on, and other processes carry on. It hears of this at
    /// its next submission, which fails. A failed update isn't a fault, so it doesn't count towards
    /// stopping all work.
    pub(crate) fn stop_space(&mut self, space: SpaceId) {
        let in_space = self.queues.iter().filter(|(_, queue)| queue.space == space);
        let handles: Vec<u32> = in_space.map(|(&handle, _)| handle).collect();
        for handle in handles {
            self.backend.close_queue(handle);
            let queue = self.queues.get_mut(&handle).expect("just found");
            queue.failed = true;
            if let Some(value) = queue.last_fence {
                let fence = queue.fence;
                self.store_fence(fence, value);
            }
        }
    }

    /// Updates on a fence run in turn, each raising it for the next. The guest's driver has the queue
    /// wait for that, so nothing needs holding back here.
    pub(super) fn update_gpu_addresses(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = UpdateRequest::parse(message)?;
        let space = self.space_for(MessageHeader::process_of(message)?)?;
        let (fence, value) = (request.fence, request.fence_value);
        if !self.fence_known(fence) {
            log::warn!(
                "address update waits on {fence:#x}, which is no sync object or queue here, so it runs now"
            );
        }
        if self.fence_reached(fence, value) {
            let applied = self.apply_updates(request.raw_operations(), space);
            if applied.is_err() {
                self.stop_space(space);
                self.write_fence(fence, value.saturating_add(1));
            }
            applied?;
            if !request.is_complete() {
                return Err(Malformed.into());
            }
            self.write_fence(fence, value.saturating_add(1));
        } else {
            let operations = request.raw_operations().to_vec();
            log::debug!(
                "update waits in {space:?} for fence {fence:#x} to reach {value}: {}",
                describe_operations(&operations)
            );
            self.pending.push(Pending::Update { fence, value, space, operations });
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// A GPU address for a whole allocation in the daemon's own space, for its copies.
    pub(crate) fn surface_address(&mut self, handle: u32) -> Result<u64, BackendError> {
        let space = self.own_space()?;
        let allocation = self.allocations.get(&handle).ok_or(UNKNOWN_ALLOCATION)?;
        if let Some(mapping) = allocation.mappings.iter().find(|mapping| mapping.space == space) {
            return mapping.address.checked_sub(mapping.first).ok_or(RANGE_TOO_LONG);
        }
        let (size, memory) = (allocation.size, allocation.memory.ok_or(UNKNOWN_ALLOCATION)?);
        let whole = align_up(size, ALLOCATION_ALIGN);
        let address = self.gpu_addresses.take(whole, ALLOCATION_ALIGN, AddressUse::Host)?;
        let map = MapRequest {
            space,
            memory,
            offset: 0,
            length: size,
            address,
            small_pages: true,
            gpu_cached: false,
            readable: true,
            writable: true,
            executable: false,
            memory_type: guesthouse_backend::MapMemoryType::Default,
        };
        if let Err(error) = self.backend.map(&map) {
            self.gpu_addresses.give_back(address, whole, ALLOCATION_ALIGN, AddressUse::Host);
            return Err(error);
        }
        self.add_mapping(handle, Mapping { address, first: 0, length: size, space });
        Ok(address)
    }
}

/// The first few operations of a batch, for the log.
fn describe_operations(operations: &[u8]) -> String {
    let parsed: Vec<_> = operations
        .chunks(UpdateRequest::OPERATION_SIZE)
        .take(3)
        .map(|bytes| {
            UpdateOperation::parse(bytes).map_or_else(|_| "unreadable".to_string(), |op| format!("{op:x?}"))
        })
        .collect();
    let total = operations.len().div_ceil(UpdateRequest::OPERATION_SIZE);
    format!("{total} operations, first {}", parsed.join(", "))
}

const UNKNOWN_ALLOCATION: BackendError = BackendError(std::borrow::Cow::Borrowed("no such allocation"));
const OVERLAPPING_COPY: BackendError = BackendError(std::borrow::Cow::Borrowed("a copy over its own source"));
const RANGE_TOO_LONG: BackendError = BackendError(std::borrow::Cow::Borrowed("a range too long for 64 bits"));
