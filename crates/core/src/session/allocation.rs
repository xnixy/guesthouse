//! Handlers for allocations and resources.

use super::{Handled, Session, answer_status};
use crate::deferred::{DeferredAction, FreedAllocation};
use crate::gpu_addresses::ADDRESSES_END;
use crate::pending::QueueAction;
use crate::tables::{Allocation, Queue};
use guesthouse_backend::{
    AllocationShape, Backend, GuestMemory, MapRequest, MemoryKind, SurfaceShape, UnmapRequest,
};
use guesthouse_messages::address::PAGE_SIZE;
use guesthouse_messages::allocation::{
    CACHED_FOR_CPU, CreateAllocation, CreateAllocationAnswer, DestroyAllocation, OpenResource, PRIMARY,
    SetGuestPages, lock_answer, locked_allocation,
};
use guesthouse_messages::priority::{self, NORMAL_PRIORITY, PriorityRequest};
use guesthouse_messages::standard::{DescribeStandard, StandardRecord};
use guesthouse_messages::wire::{align_up, checked_align_up};
use guesthouse_messages::{MessageHeader, Status};
use std::time::Instant;

/// The guest's own number for 32-bit colour.
const FORMAT_32_BIT: u32 = 21;
/// Standard surfaces fill whole units of this size in video memory.
const STANDARD_GRAIN: u64 = 64 << 10;
/// The colour word of a standard surface, which is never a vendor's special case.
const STANDARD_COLOUR: u32 = 0;
/// How many of the vendor's descriptions of standard surfaces are kept. The kernel creates each
/// surface right after asking, and makes only a few at a time.
const STANDARD_DATA_KEPT: usize = 16;

fn standard_shape(record: &StandardRecord) -> AllocationShape {
    AllocationShape {
        // Past the largest allocation either way, which is refused when it's made.
        size: checked_align_up(record.exact_size(), STANDARD_GRAIN).unwrap_or(u64::MAX),
        exact_size: record.exact_size(),
        surface: Some(SurfaceShape {
            width: record.width,
            height: record.height,
            format: FORMAT_32_BIT,
            pitch: record.pitch,
            tiling: None,
            refresh: record.refresh,
            samples: (1, 0),
            colour: STANDARD_COLOUR,
        }),
        gpu_cached: false,
        kind: MemoryKind::PLAIN,
    }
}

impl<B: Backend, M: GuestMemory> Session<B, M> {
    fn shape_of(&mut self, private: &[u8]) -> AllocationShape {
        match self.standard_shape_of(private) {
            Some(shape) => shape,
            None => self.backend.describe_allocation(private),
        }
    }

    /// The shape of a standard surface, if `private` describes one: the daemon's own record, or
    /// the vendor's data the core handed out for it.
    fn standard_shape_of(&self, private: &[u8]) -> Option<AllocationShape> {
        let ours = StandardRecord::parse(private).map(|record| standard_shape(&record));
        ours.or_else(|| {
            let mut given = self.standard_data.iter().rev();
            given.find(|(data, _)| data.as_slice() == private).map(|&(_, shape)| shape)
        })
    }

    /// Describes a surface the guest's kernel makes itself, in the vendor's private data when the
    /// vendor's user-mode driver reads it.
    pub(super) fn describe_standard_allocation(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = DescribeStandard::parse(message)?;
        let surface = request.surface();
        let vendor_data = self.backend.standard_allocation_data(&surface);
        request.answer(vendor_data.as_deref(), out);
        if let Some(data) = vendor_data {
            let shape = self.backend.standard_allocation_shape(&surface);
            let shape = shape.unwrap_or_else(|| standard_shape(&surface.record()));
            if self.standard_data.len() == STANDARD_DATA_KEPT {
                self.standard_data.pop_front();
            }
            self.standard_data.push_back((data, shape));
        }
        Ok(())
    }

    /// Guest memory does not count: it is not on the card.
    fn over_video_cap(&self, cap: u64, wanted: u64) -> bool {
        let used = self
            .allocations
            .values()
            .filter(|allocation| {
                allocation.memory.is_some() && allocation.guest_address.is_none() && allocation.view.is_none()
            })
            .map(|allocation| allocation.size);
        used.fold(wanted, u64::saturating_add) > cap
    }

    /// Whether memory the guest is yet to list would be more than it has. Each page waiting for its
    /// number takes room here, so the list can't be left to grow with what a message says.
    fn over_guest_memory(&self, wanted: u64) -> bool {
        let waiting = self.allocations.values().filter(|allocation| allocation.pending_pages.is_some());
        let waiting = waiting.map(|allocation| allocation.size);
        waiting.fold(wanted, u64::saturating_add) > self.config.limits.system_budget
    }

    /// What stops these allocations being made, if anything does.
    fn refusal(&self, guest_backed: bool, shapes: &[AllocationShape]) -> Option<Status> {
        if guest_backed {
            let wanted = shapes.iter().map(|shape| shape.exact_size).fold(0, u64::saturating_add);
            return self.over_guest_memory(wanted).then_some(Status::NO_MEMORY);
        }
        let wanted = shapes.iter().map(|shape| shape.size).fold(0, u64::saturating_add);
        let too_large = shapes.iter().any(|shape| shape.size > ADDRESSES_END);
        let over_cap = self.config.limits.video_cap.is_some_and(|cap| self.over_video_cap(cap, wanted));
        (too_large || over_cap).then_some(Status::NO_VIDEO_MEMORY)
    }

    pub(super) fn create_allocation(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = CreateAllocation::parse(message)?;
        let guest_backed = request.guest_backed();
        CreateAllocationAnswer::begin(request.count, out);
        let shapes: Vec<AllocationShape> =
            request.entries().map(|entry| self.shape_of(entry.private)).collect();
        // All of it or none, as nothing has been made yet that would need taking back.
        let refused = request.entries().any(|entry| {
            self.standard_shape_of(entry.private).is_none()
                && !self.backend.allocation_data_valid(entry.private)
        });
        if refused {
            log::info!("refused an allocation whose private data the vendor's driver wouldn't take");
            CreateAllocationAnswer::fail(Status::INVALID_PARAMETER, out);
            return Ok(());
        }
        if let Some(status) = self.refusal(guest_backed, &shapes) {
            CreateAllocationAnswer::fail(status, out);
            return Ok(());
        }
        let owner = MessageHeader::process_of(message)?;
        let resource = self.handles.take();
        self.resources.insert(resource, Vec::with_capacity(request.count));
        CreateAllocationAnswer::set_resource(resource, out);
        if request.shared_by_global_handle() {
            // The guest opens it by this from another device, and an open finds resources by handle.
            CreateAllocationAnswer::set_global_share(resource, out);
        }
        for (index, (entry, shape)) in request.entries().zip(shapes).enumerate() {
            let handle = self.handles.take();
            let mut allocation = Allocation {
                owner,
                resource,
                gpu_cached: shape.gpu_cached,
                primary: entry.flags & PRIMARY != 0,
                surface: shape.surface,
                kind: shape.kind,
                ..Allocation::default()
            };
            if guest_backed {
                // The size in the answer is how much memory the guest locks, so it must be exact.
                allocation.size = shape.exact_size;
                let pages = shape.exact_size.div_ceil(PAGE_SIZE) as usize;
                allocation.pending_pages = Some(vec![None; pages]);
            } else {
                let (memory, size) = self.backend.allocate_video_memory(shape.size, shape.kind)?;
                allocation.memory = Some(memory);
                allocation.size = size;
            }
            log::debug!(
                "allocation {handle:#x}: {:#x} bytes, guest backed {guest_backed}, primary {}",
                allocation.size,
                allocation.primary
            );
            // On the basic display the guest's kernel copies DWM's primaries to the staging surface,
            // and that path needs the flag back. Once our display driver has a ring, the screen is a
            // surface in its process and nothing scans out, so no surface is a primary any more:
            // the guest's kernel would otherwise look for a display adapter behind each one, and a
            // render-only adapter has none.
            let primary = if self.display_driver_seen { 0 } else { entry.flags & PRIMARY };
            let flags = primary | CACHED_FOR_CPU;
            CreateAllocationAnswer::set_entry(index, handle, entry.private_size, flags, allocation.size, out);
            if let Some(surface) = shape.surface {
                let fields = [
                    surface.width,
                    surface.height,
                    surface.format,
                    surface.samples.0,
                    surface.samples.1,
                    surface.refresh.0,
                    surface.refresh.1,
                ];
                CreateAllocationAnswer::set_surface(index, fields, out);
            }
            let private_at = out.len();
            out.extend_from_slice(entry.private);
            if self.standard_shape_of(entry.private).is_none() {
                self.backend.finish_allocation_data(&mut out[private_at..], shape.size);
            }
            self.allocations.insert(handle, allocation);
            self.resources.get_mut(&resource).expect("made above").push(handle);
        }
        CreateAllocationAnswer::finish(out);
        Ok(())
    }

    pub(super) fn destroy_allocation(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = DestroyAllocation::parse(message)?;
        let resource = request.resource;
        answer_status(Status::SUCCESS, out);
        if let Some(users) = self.resource_users.get_mut(&resource)
            && *users > 1
        {
            *users -= 1;
            return Ok(());
        }
        let request = request.with_handles()?;
        let members = self.resources.get(&resource).map_or(&[][..], Vec::as_slice);
        if !self.copies.is_empty()
            && request.handles.iter().chain(members.iter().copied()).any(|handle| self.copy_uses(handle))
        {
            out.clear();
            self.park(message);
            return Ok(());
        }
        self.resource_users.remove(&resource);
        // Each process sends this twice: once for the resource and once for its allocations by name.
        // Naming one frees it, unless other processes may still reach it, like dwm's last frame. An
        // entry in the users table stays until the last user goes, however few are left.
        let held_elsewhere = |handle: &u32| {
            self.allocations.get(handle).is_some_and(|allocation| {
                let owner = allocation.resource;
                self.resource_users.contains_key(&owner)
                    || self.shared_objects.values().any(|&object| object == owner)
            })
        };
        let mut handles: Vec<u32> = request.handles.iter().filter(|handle| !held_elsewhere(handle)).collect();
        handles.extend(self.resources.remove(&resource).unwrap_or_default());
        self.run_deferred();
        for handle in handles {
            self.allocation_going(handle);
            let Some(allocation) = self.allocations.remove(&handle) else { continue };
            self.leave_resource(allocation.resource, handle);
            let Some(memory) = allocation.memory else { continue };
            let freed = FreedAllocation {
                memory,
                mappings: allocation.mappings,
                view: allocation.view,
                shared: allocation.guest_address.map(|address| (address, allocation.size)),
            };
            self.defer(DeferredAction::FreeAllocation(freed));
        }
        Ok(())
    }

    /// Keeps the priorities for the allocations named, or for the whole resource if none is. The
    /// host's driver is left to choose what stays in video memory, so this only remembers them.
    pub(super) fn set_allocation_priority(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = PriorityRequest::parse(message)?;
        let priorities = request.priorities()?;
        if request.handles.is_empty() {
            let priority = priorities.iter().next();
            for handle in self.resources.get(&request.resource).into_iter().flatten() {
                if let Some(allocation) = self.allocations.get_mut(handle) {
                    allocation.priority = priority;
                }
            }
        } else {
            for (handle, priority) in request.handles.iter().zip(priorities.iter()) {
                if let Some(allocation) = self.allocations.get_mut(&handle) {
                    allocation.priority = Some(priority);
                }
            }
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// The priorities the guest set, or the normal one for what it never set. A resource alone is
    /// answered with its first allocation's.
    pub(super) fn get_allocation_priority(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = PriorityRequest::parse(message)?;
        let priority_of = |handle: &u32| {
            self.allocations.get(handle).and_then(|allocation| allocation.priority).unwrap_or(NORMAL_PRIORITY)
        };
        if request.handles.is_empty() {
            let first = self.resources.get(&request.resource).and_then(|members| members.first());
            priority::answer(std::iter::once(first.map_or(NORMAL_PRIORITY, priority_of)), out);
        } else {
            priority::answer(request.handles.iter().map(|handle| priority_of(&handle)), out);
        }
        Ok(())
    }

    /// A resource goes with its last allocation, as nothing can open it after that.
    fn leave_resource(&mut self, resource: u32, handle: u32) {
        let Some(members) = self.resources.get_mut(&resource) else { return };
        members.retain(|&member| member != handle);
        if members.is_empty() {
            self.resources.remove(&resource);
        }
    }

    /// Handles mean the same to every process here, so opening returns the existing ones and the
    /// resource counts its users.
    pub(super) fn open_resource(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = OpenResource::parse(message)?;
        let resource = self.shared_objects.get(&request.shared).copied().unwrap_or(request.shared);
        match self.resources.get(&resource).filter(|handles| handles.len() >= request.count) {
            Some(handles) => {
                request.answer(resource, &handles[..request.count], out);
                *self.resource_users.entry(resource).or_insert(1) += 1;
            }
            None => request.refuse(out),
        }
        Ok(())
    }

    /// Holds a first lock until the queues in its address spaces get to where they are now, if they
    /// aren't there. Moving an allocation under an engine that's using it loses what the engine
    /// writes, which shows up as damage here and there in encoded video.
    fn wait_for_first_lock(&mut self, handle: u32, message: &[u8]) -> bool {
        let mappings = &self.allocations[&handle].mappings;
        let in_use = |queue: &Queue| mappings.iter().any(|mapping| mapping.space == queue.space);
        let queues: Vec<u32> =
            self.queues.iter().filter(|(_, queue)| in_use(queue)).map(|(&handle, _)| handle).collect();
        if self.queues_idle(queues.iter().copied()) {
            return false;
        }
        self.first_locks_waited.insert(handle);
        let action =
            QueueAction::Retry { message: message.to_vec(), ticket: self.ticket, started: Instant::now() };
        self.after_queues(queues.iter().copied(), action);
        self.hold_reply();
        true
    }

    /// Gives the guest's CPU access. The first lock moves the allocation from video memory to pages
    /// shared with the guest, at the same GPU addresses.
    pub(super) fn lock_for_cpu(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let handle = locked_allocation(message)?;
        if self.copy_uses(handle) {
            self.park(message);
            return Ok(());
        }
        let Some(allocation) = self.allocations.get(&handle) else {
            lock_answer(Status::INVALID_PARAMETER, 0, out);
            return Ok(());
        };
        let Some(memory) = allocation.memory else {
            lock_answer(Status::INVALID_PARAMETER, 0, out);
            return Ok(());
        };
        if let Some(address) = allocation.guest_address {
            lock_answer(Status::SUCCESS, address, out);
            return Ok(());
        }
        let size = allocation.size;
        let preserve = self.work_started > 0 && !allocation.mappings.is_empty();
        if preserve && !self.first_locks_waited.remove(&handle) && self.wait_for_first_lock(handle, message) {
            return Ok(());
        }
        if preserve && !self.copies.is_empty() {
            // The vendor's copies run in order, and this one needs them finished.
            self.park(message);
            return Ok(());
        }
        let (fresh, limit) = (self.placed_io_space()?.after_fences(), self.memory.window()?.end);
        let Some((address, used_before)) = self.guest_ranges.take(size, fresh, limit) else {
            lock_answer(Status::NO_MEMORY, 0, out);
            return Ok(());
        };
        if used_before {
            // Another allocation's data must not reach this one.
            self.memory.zero(address, size)?;
        }
        let pages_size = align_up(size, PAGE_SIZE);
        let host_address = self.memory.host_address(address, pages_size)?;
        let shared = self.backend.adopt_host_pages(host_address, pages_size)?;
        let has_mappings = !self.allocations[&handle].mappings.is_empty();
        let own = if has_mappings { Some(self.own_space()?) } else { None };
        if let (true, Some(own)) = (preserve, own) {
            let host = &mut super::services!(self);
            self.backend.preserve_contents(host, own, memory, shared, size)?;
        }
        for mapping in &self.allocations[&handle].mappings {
            self.log_unmap_in_flight("a first lock", mapping.address, mapping.length);
        }
        let allocation = self.allocations.get_mut(&handle).expect("read above");
        for mapping in &allocation.mappings {
            let unmap = UnmapRequest {
                space: mapping.space,
                memory,
                address: mapping.address,
                length: mapping.length,
            };
            self.backend.unmap(&unmap)?;
            self.backend.map(&MapRequest {
                space: mapping.space,
                memory: shared,
                offset: mapping.first,
                length: mapping.length,
                address: mapping.address,
                small_pages: false,
                // The daemon's own copies carry no cache invalidates, so its mappings stay uncached.
                gpu_cached: Some(mapping.space) != own && allocation.gpu_cached,
                readable: true,
                writable: true,
                executable: false,
                memory_type: guesthouse_backend::MapMemoryType::Default,
                driver_protection: 0,
            })?;
        }
        allocation.memory = Some(shared);
        allocation.guest_address = Some(address);
        self.backend.release_memory(memory);
        lock_answer(Status::SUCCESS, address, out);
        Ok(())
    }

    /// Long lists come in several messages, and the allocation is ready after the last.
    pub(super) fn set_guest_pages(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = SetGuestPages::parse(message)?;
        let pending = self.allocations.get_mut(&request.allocation).and_then(|a| a.pending_pages.as_mut());
        let Some(pending) = pending else {
            answer_status(Status::INVALID_PARAMETER, out);
            return Ok(());
        };
        let slots =
            request.first.checked_add(request.count).and_then(|end| pending.get_mut(request.first..end));
        let Some(slots) = slots else {
            answer_status(Status::INVALID_PARAMETER, out);
            return Ok(());
        };
        slots.iter_mut().zip(request.pages()?.iter()).for_each(|(slot, page)| *slot = Some(page));
        answer_status(Status::SUCCESS, out);
        if pending.iter().any(Option::is_none) {
            return Ok(());
        }
        let pages: Vec<u64> = pending.iter().flatten().copied().collect();
        let allocation = self.allocations.get_mut(&request.allocation).expect("found above");
        allocation.pending_pages = None;
        let view = self.memory.map_pages(&pages).map_err(|error| {
            guesthouse_backend::BackendError::new(format!("mapping guest pages: {error}"))
        })?;
        allocation.view = Some(view);
        allocation.memory = Some(self.backend.adopt_host_pages(view.host_address, view.size)?);
        Ok(())
    }
}
