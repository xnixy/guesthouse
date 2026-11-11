//! Handlers for processes, contexts, queues and work.

use super::waiting::GpuTook;
use super::{Failure, Handled, Session, answer_status};
use crate::deferred::DeferredAction;
use crate::pending::GateStep;
use crate::tables::{Queue, ReservedRange};
use guesthouse_backend::{Backend, GuestMemory, SpaceId, UnmapRequest, WorkSource};
use guesthouse_messages::queue::{
    CreateContext, CreateHardwareQueue, DEFAULT_QUEUED_PRESENT_LIMIT, QueuedLimit, SubmitToContext,
    SubmitToQueue,
};
use guesthouse_messages::sync::first_handle;
use guesthouse_messages::wire::{Malformed, put_u32, u8_at, u32_at};
use guesthouse_messages::{MessageHeader, Status};

/// Where a new process's PID in the guest is, where its flags are, and the flag for a 32-bit one.
const PROCESS_ID: usize = 0x20;
const PROCESS_FLAGS: usize = 0x232;
const PROCESS_32_BIT: u8 = 4;

impl<B: Backend, M: GuestMemory> Session<B, M> {
    pub(super) fn create_process(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let handle = self.handles.take();
        if u8_at(message, PROCESS_FLAGS)? & PROCESS_32_BIT != 0 {
            self.processes_32_bit.insert(handle);
        }
        self.guest_pids.insert(handle, u32_at(message, PROCESS_ID)?);
        put_u32(out, handle);
        Ok(())
    }

    /// The address space closes at the next chance to run deferred work, not now.
    pub(super) fn destroy_process(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let process = MessageHeader::process_of(message)?;
        self.processes_32_bit.remove(&process);
        self.guest_pids.remove(&process);
        self.process_going(process);
        self.context_processes.retain(|_, owner| *owner != process);
        self.reservations.retain(|&(owner, _), _| owner != process);
        self.backend.process_gone(process);
        if let Some(space) = self.spaces.remove(&process) {
            let action = DeferredAction::CloseSpace(space);
            self.deferred.push(crate::deferred::Deferred { waits: Vec::new(), action });
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// Frees an address space, and anything still running or mapped there.
    pub(crate) fn close_space(&mut self, space: SpaceId) {
        let mut owned: Vec<u32> =
            self.queues.iter().filter(|(_, queue)| queue.space == space).map(|(&handle, _)| handle).collect();
        // Oldest first, as each gives back a fence and the order decides who gets which one next.
        owned.sort_unstable();
        owned.into_iter().for_each(|queue| self.close_queue(queue));
        self.failed_queues.remove(&space);
        // Oldest first again, as each gives back addresses.
        let mut mapped: Vec<(u64, u32)> = self
            .allocations
            .iter()
            .filter(|(_, allocation)| allocation.mappings.iter().any(|mapping| mapping.space == space))
            .filter_map(|(&handle, allocation)| Some((allocation.first_mapped?, handle)))
            .collect();
        mapped.sort_unstable();
        for (_, handle) in mapped {
            self.close_mappings(handle, space);
        }
        let (closing, kept) =
            std::mem::take(&mut self.reserved_addresses).into_iter().partition(|range| range.space == space);
        self.reserved_addresses = kept;
        let closing: Vec<ReservedRange> = closing;
        closing.into_iter().for_each(|range| self.give_back_reserved(range));
        self.closed_spaces.insert(space);
        if let Some(block) = self.fences.block {
            let unmap = UnmapRequest {
                space,
                memory: block.memory,
                address: block.gpu_address,
                length: crate::FENCE_AREA,
            };
            if let Err(error) = self.backend.unmap(&unmap) {
                log::warn!("closing an address space: {error}");
            }
        }
        self.backend.destroy_address_space(space);
    }

    /// Removes an allocation's mappings in one address space.
    fn close_mappings(&mut self, handle: u32, space: SpaceId) {
        let allocation = self.allocations.get_mut(&handle).expect("the caller found it");
        let Some(memory) = allocation.memory else { return };
        let system_memory = allocation.in_system_memory();
        let (closing, kept) = std::mem::take(&mut allocation.mappings)
            .into_iter()
            .partition(|mapping: &crate::tables::Mapping| mapping.space == space);
        allocation.mappings = kept;
        for mapping in closing {
            let unmap = UnmapRequest { space, memory, address: mapping.address, length: mapping.length };
            if let Err(error) = self.backend.unmap(&unmap) {
                log::warn!("closing an address space: {error}");
            }
            self.give_back_mapping(mapping.address, system_memory);
        }
    }

    pub(super) fn create_context(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = CreateContext::parse(message)?;
        // Video work sent to a copy channel would be a bad push buffer on the card the host's desktop
        // uses, so it stops here. A handle of zero fails the context, and the program can fall back.
        if !self.backend.context_allowed(request.node, request.client_hint) {
            log::info!("refused a context on node {}, which has no channel here", request.node);
            request.answer(message, 0, out);
            return Ok(());
        }
        let process = MessageHeader::process_of(message)?;
        let private = message.get(CreateContext::PRIVATE..).unwrap_or_default();
        let private = &private[..request.private_size.min(private.len())];
        if !self.backend.context_data_valid(process, request.node, private) {
            log::info!(
                "refused a context on node {} whose private data the vendor's driver wouldn't take",
                request.node
            );
            request.answer(message, 0, out);
            return Ok(());
        }
        let handle = self.handles.take();
        log::debug!("context {handle:#x} on node {}, hint {:#x}", request.node, request.client_hint);
        self.context_nodes.insert(handle, request.node);
        self.context_hints.insert(handle, request.client_hint);
        self.context_processes.insert(handle, process);
        self.backend.context_made(process, request.node);
        let answer_at = out.len();
        request.answer(message, handle, out);
        if let Some(private) = out.get_mut(answer_at + CreateContext::PRIVATE..) {
            self.backend.finish_context_data(request.node, private);
        }
        Ok(())
    }

    /// A device's limit on queued presents. A set is kept for that device, and a read answers it, or
    /// the default until the device has set one.
    pub(super) fn queued_limit(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = QueuedLimit::parse(message)?;
        if request.is_set() {
            self.present_limits.insert(request.device, request.limit);
        }
        let limit = self.present_limits.get(&request.device).copied().unwrap_or(DEFAULT_QUEUED_PRESENT_LIMIT);
        request.answer(message, limit, out);
        Ok(())
    }

    /// The device's limit goes with it. A message too short to name one has nothing to forget.
    pub(super) fn destroy_device(&mut self, message: &[u8], out: &mut Vec<u8>) {
        if let Ok(device) = first_handle(message) {
            self.present_limits.remove(&device);
        }
        answer_status(Status::SUCCESS, out);
    }

    /// An app that exits destroys its contexts and never names their queues.
    pub(super) fn destroy_context(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let context = first_handle(message)?;
        for queue in self.context_queues.get(&context).cloned().unwrap_or_default() {
            self.close_queue(queue);
        }
        self.context_queues.remove(&context);
        let node = self.context_nodes.remove(&context);
        self.context_hints.remove(&context);
        if let (Some(node), Some(process)) = (node, self.context_processes.remove(&context)) {
            self.backend.context_gone(process, node);
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    pub(super) fn create_hardware_queue(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = CreateHardwareQueue::parse(message)?;
        request.begin_answer(message, out);
        let queue = self.handles.take();
        let progress = self.handles.take();
        let (fence, fence_gpu_address) = self.take_fence()?;
        let space = self.space_for(MessageHeader::process_of(message)?)?;
        self.queues.insert(queue, Queue::new(fence, fence_gpu_address, space, request.context));
        self.context_queues.entry(request.context).or_default().push(queue);
        self.queue_progress.insert(progress, fence);
        self.queues_made = self.queues_made.wrapping_add(1);
        let finished =
            self.backend.finish_queue_data(queue, &mut out[CreateHardwareQueue::PRIVATE..], self.queues_made);
        if let Err(error) = finished {
            // The guest gets a failure and never names this queue, so nothing of it may stay.
            self.close_queue(queue);
            self.queue_progress.remove(&progress);
            return Err(error.into());
        }
        CreateHardwareQueue::finish_answer(queue, progress, fence, fence_gpu_address, out);
        Ok(())
    }

    pub(super) fn destroy_hardware_queue(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        self.close_queue(first_handle(message)?);
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    pub(crate) fn close_queue(&mut self, handle: u32) {
        let closed = self.queues.remove(&handle);
        for step in self.gates.remove(&handle).unwrap_or_default() {
            if let GateStep::Wait { signals, id, .. } = step {
                self.leave_wait(id, &signals, false);
            }
        }
        for queues in self.context_queues.values_mut() {
            queues.retain(|&queue| queue != handle);
        }
        self.backend.close_queue(handle);
        if let Some(queue) = closed {
            self.fences.give_back((queue.fence, queue.fence_gpu_address));
        }
    }

    pub(super) fn submit_to_hardware_queue(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        self.run_deferred();
        let request = SubmitToQueue::parse(message)?;
        let value = request.fence_value;
        self.note_waiting_updates(request.queue);
        match self.start_on_gpu(request.queue, WorkSource::HardwareQueue, request.private, value) {
            GpuTook::Work(work) => self.hold_if_ring_is_full(request.queue, value, work),
            GpuTook::NoRoom => {
                self.retry_later(message);
                return Ok(());
            }
            took @ (GpuTook::Nothing | GpuTook::Dropped) => {
                if let Some(queue) = self.queues.get(&request.queue) {
                    // Nothing ran, so stand in for the GPU where the guest would notice.
                    self.memory.write_u64(queue.fence, value)?;
                }
                answer_status(took.status(), out);
                return Ok(());
            }
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// A context that has no queue gets one here, so what waits for it can treat it like any other.
    pub(super) fn queue_of_context(&mut self, context: u32, message: &[u8]) -> Result<u32, Failure> {
        if self.context_queues.get(&context).is_none_or(Vec::is_empty) {
            let handle = self.handles.take();
            let (fence, fence_gpu_address) = self.take_fence()?;
            let space = self.space_for(MessageHeader::process_of(message)?)?;
            self.queues.insert(handle, Queue::new(fence, fence_gpu_address, space, context));
            self.context_queues.entry(context).or_default().push(handle);
        }
        Ok(self.context_queues[&context][0])
    }

    pub(super) fn submit_to_context(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = SubmitToContext::parse(message)?;
        let queue = self.queue_of_context(request.context, message)?;
        let record = self.queues.get(&queue).ok_or(Malformed)?;
        let (fence, value) = (record.fence, record.last_fence.unwrap_or(0).saturating_add(1));
        self.note_waiting_updates(queue);
        match self.start_on_gpu(queue, WorkSource::Context, request.private, value) {
            GpuTook::Work(_) => {}
            GpuTook::NoRoom => {
                self.retry_later(message);
                return Ok(());
            }
            took @ (GpuTook::Nothing | GpuTook::Dropped) => {
                self.memory.write_u64(fence, value)?;
                answer_status(took.status(), out);
                return Ok(());
            }
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }
}
