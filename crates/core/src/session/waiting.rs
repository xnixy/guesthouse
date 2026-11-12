//! What waits: deferred work, things that need a fence to move, and queue gates.

use super::{Handled, Session, answer_status, services};
use crate::deferred::{Deferred, DeferredAction, FreedAllocation};
use crate::fault_rate::{FAULT_WINDOW, MOST_FAULTS};
use crate::pending::{
    GATE_TIMEOUT, GateStep, MOST_FAILED_QUEUES, Pending, QUEUE_TIMEOUT, QueueAction, RING_ROOM_TIMEOUT,
    SharedWait, Signal, Target, Waiting,
};
use crate::session::{Answer, HeldReply, ReplyTicket};
use crate::tables::Queue;
use crate::timing::{HeldFor, Stage};
use guesthouse_backend::{Backend, GuestMemory, SpaceId, UnmapRequest, WorkStart};
use guesthouse_messages::Status;
use guesthouse_messages::notice::Notice;
use guesthouse_messages::queue::PresentToken;
use guesthouse_messages::sync::{SignalSharedFence, SignalSyncObject, WaitOnCpu, WaitOnGpu};
use std::collections::VecDeque;
use std::time::Instant;

/// What became of a submission.
pub(crate) enum GpuTook {
    Nothing,
    /// The work can't run, as its queue or the whole guest has stopped.
    Dropped,
    NoRoom,
    Work(u32),
}

impl GpuTook {
    /// What the guest hears. A failed submission is the only way the host has of telling a program
    /// that its work didn't run: the guest's driver marks that device as lost, and the program sees
    /// that as it would on real Windows. Nothing the host sends unasked can say it.
    pub(crate) fn status(&self) -> Status {
        match self {
            Self::Dropped => Status::UNSUCCESSFUL,
            _ => Status::SUCCESS,
        }
    }
}

impl<B: Backend, M: GuestMemory> Session<B, M> {
    /// Does something once the GPU has finished what it has been given so far, or now if it has.
    pub(crate) fn defer(&mut self, action: DeferredAction) {
        let waits: Vec<(u32, u64)> = self
            .queues
            .iter()
            .filter_map(|(&handle, queue)| {
                let value = queue.last_fence?;
                (!queue.failed && self.fence_value(queue.fence) < value).then_some((handle, value))
            })
            .collect();
        if waits.is_empty() {
            self.run_deferred_action(action);
        } else {
            self.deferred.push(Deferred { waits, action });
        }
    }

    /// Finishes whatever has stopped waiting. A queue that has gone or failed waits for nothing.
    pub(crate) fn run_deferred(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        for mut item in std::mem::take(&mut self.deferred) {
            item.waits.retain(|&(handle, value)| {
                self.queues
                    .get(&handle)
                    .is_some_and(|queue| !queue.failed && self.fence_value(queue.fence) < value)
            });
            if item.waits.is_empty() {
                self.run_deferred_action(item.action);
            } else {
                self.deferred.push(item);
            }
        }
    }

    pub(crate) fn run_deferred_action(&mut self, action: DeferredAction) {
        match action {
            DeferredAction::ReturnFence(fence) => self.fences.give_back(fence),
            DeferredAction::FreeAllocation(freed) => self.free_allocation(freed),
            DeferredAction::CloseSpace(space) => self.close_space(space),
            DeferredAction::GiveBackReserved(range) => self.give_back_reserved(range),
        }
    }

    /// Notes an unmap the GPU could still be affected by.
    pub(crate) fn log_unmap_in_flight(&self, why: &str, address: u64, length: u64) {
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let busy = self.queues.iter().filter(|(_, queue)| {
            !queue.failed && queue.last_fence.is_some_and(|value| self.fence_value(queue.fence) < value)
        });
        let busy: Vec<String> = busy.map(|(handle, _)| format!("{handle:#x}")).collect();
        if !busy.is_empty() {
            log::debug!(
                "unmap for {why} at {address:#x}, {length:#x} bytes, with queues busy: {}",
                busy.join(" ")
            );
        }
    }

    pub(crate) fn free_allocation(&mut self, freed: FreedAllocation) {
        for mapping in &freed.mappings {
            if !self.closed_spaces.contains(&mapping.space) {
                self.log_unmap_in_flight("a free", mapping.address, mapping.length);
                let unmap = UnmapRequest {
                    space: mapping.space,
                    memory: freed.memory,
                    address: mapping.address,
                    length: mapping.length,
                };
                if let Err(error) = self.backend.unmap(&unmap) {
                    log::warn!("unmapping freed memory: {error}");
                }
            }
            self.give_back_mapping(mapping.address, freed.in_system_memory());
        }
        self.backend.release_memory(freed.memory);
        if let Some(view) = freed.view {
            self.memory.unmap_pages(view);
        }
        if let Some((address, size)) = freed.shared {
            self.guest_ranges.release(address, size);
        }
    }

    /// Calls `visit` with each queue behind a handle that may be a queue or a context, if the GPU has
    /// work from it, until `visit` says stop.
    fn visit_live_queues(&self, handle: u32, mut visit: impl FnMut(&Queue) -> bool) -> bool {
        let one = [handle];
        let queues = self.context_queues.get(&handle).map_or(&one[..], Vec::as_slice);
        queues.iter().filter_map(|queue| self.queues.get(queue)).all(|queue| {
            let live = queue.last_fence.is_some() && !queue.failed;
            !live || visit(queue)
        })
    }

    /// Where each queue's progress fence will be once it has done what it has been given.
    pub(crate) fn queue_targets(&self, handles: impl Iterator<Item = u32>) -> Vec<Target> {
        let mut targets = Vec::new();
        for handle in handles {
            self.visit_live_queues(handle, |queue| {
                targets.extend(queue.last_fence.map(|value| (queue.fence, value)));
                true
            });
        }
        targets
    }

    pub(crate) fn targets_reached(&self, targets: &[Target]) -> bool {
        targets.iter().all(|&(fence, value)| self.fence_value(fence) >= value)
    }

    /// Whether no queue behind these handles has work the GPU has not finished.
    pub(crate) fn queues_idle(&self, mut handles: impl Iterator<Item = u32>) -> bool {
        handles.all(|handle| {
            self.visit_live_queues(handle, |queue| {
                queue.last_fence.is_none_or(|value| self.fence_value(queue.fence) >= value)
            })
        })
    }

    pub(crate) fn after_queues(&mut self, handles: impl Iterator<Item = u32> + Clone, action: QueueAction) {
        if self.queues_idle(handles.clone()) {
            self.run_queue_action(action);
        } else {
            let targets = self.queue_targets(handles);
            let deadline = Instant::now() + QUEUE_TIMEOUT;
            self.pending.push(Pending::AfterQueues { targets, deadline, action });
        }
    }

    fn run_queue_action(&mut self, action: QueueAction) {
        match action {
            QueueAction::WakeEvent { event, release } => {
                self.notices.push(Notice::WakeEvent { event, release });
            }
            QueueAction::WriteFences(signals) => {
                signals.into_iter().for_each(|(handle, value)| self.write_fence(handle, value));
            }
            QueueAction::PresentDone(token) => self.notices.push(Notice::PresentDone { token }),
            QueueAction::Retry { message, ticket, started } => {
                self.pending.push(Pending::Retry { message, ticket, started });
            }
        }
    }

    /// The queues behind a handle that have work on the GPU.
    pub(crate) fn live_queues_of(&self, handle: u32) -> Vec<u32> {
        let one = [handle];
        let queues = self.context_queues.get(&handle).map_or(&one[..], Vec::as_slice);
        let live = |queue: &&u32| {
            self.queues.get(queue).is_some_and(|queue| queue.last_fence.is_some() && !queue.failed)
        };
        queues.iter().filter(live).copied().collect()
    }

    pub(super) fn signal_sync_object(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = SignalSyncObject::parse(message)?;
        let contexts = request.contexts.iter();
        if let Some(event) = request.cpu_event {
            // The guest passes its reference to the host here, so the event must be released.
            self.after_queues(contexts, QueueAction::WakeEvent { event, release: 1 });
        } else if self.queues_idle(contexts.clone()) {
            match request.values {
                Some(values) => request
                    .objects
                    .iter()
                    .zip(values.iter())
                    .for_each(|(handle, value)| self.write_fence(handle, value)),
                None => request.objects.iter().for_each(|handle| self.write_fence(handle, 0)),
            }
        } else {
            let values = request.values.map(|values| values.iter().collect::<Vec<_>>());
            let value_of = |index: usize| values.as_ref().map_or(0, |values| values[index]);
            let signals = request.objects.iter().enumerate();
            let signals = signals.map(|(index, handle)| (handle, value_of(index))).collect();
            self.after_queues(contexts, QueueAction::WriteFences(signals));
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    pub(super) fn signal_shared_fence(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = SignalSharedFence::parse(message)?;
        let action = QueueAction::WriteFences(vec![(request.handle, request.value)]);
        self.after_queues(std::iter::once(request.context), action);
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// The guest's compositor learns of a new frame when the token comes back, so it must not come
    /// back before the GPU gets here.
    pub(super) fn present_token(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = PresentToken::parse(message)?;
        self.after_queues(std::iter::once(request.context), QueueAction::PresentDone(request.token));
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    fn signals_finished(&self, signals: &[Signal], wake_on_any: bool) -> bool {
        let reached = |&(handle, value): &Signal| self.fence_reached(handle, value);
        if wake_on_any { signals.iter().any(reached) } else { signals.iter().all(reached) }
    }

    pub(super) fn wait_on_cpu(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = WaitOnCpu::parse(message)?;
        let release = u64::from(request.release_event);
        let signals: Vec<Signal> = request.objects.iter().zip(request.values.iter()).collect();
        if self.signals_finished(&signals, request.wake_on_any) {
            self.take_tokens(&signals, request.wake_on_any);
            self.notices.push(Notice::WakeEvent { event: request.event, release });
        } else {
            let (wake_on_any, event) = (request.wake_on_any, request.event);
            self.pending.push(Pending::CpuWait { signals, wake_on_any, event, release });
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// The queue runs on the guest's own ring, so the only way to hold it is to hold its doorbells.
    pub(super) fn wait_on_gpu(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = WaitOnGpu::parse(message)?;
        let signals: Vec<Signal> = request.objects.iter().zip(request.values.iter()).collect();
        let handle = request.queue_or_context;
        if self.context_nodes.contains_key(&handle) {
            self.queue_of_context(handle, message)?;
        }
        let queues = self.context_queues.get(&handle).cloned().unwrap_or_else(|| vec![handle]);
        log::debug!(
            "wait on gpu for {handle:#x}: {} queues {queues:x?}, gated now {:?}, signals {signals:x?}",
            queues.len(),
            queues.iter().map(|queue| self.gates.contains_key(queue)).collect::<Vec<_>>()
        );
        for &(object, value) in &signals {
            if !self.fence_known(object) {
                log::warn!(
                    "wait on gpu names {object:#x} for {value}, which is no sync object or queue here, so it counts as reached"
                );
            }
        }
        let deadline = Instant::now() + GATE_TIMEOUT;
        let id = self.next_wait;
        self.next_wait += 1;
        // A wait on a context is one wait, however many queues the context has: a mutex it takes is
        // taken once, and lets every one of them through.
        let mut went = false;
        let mut held = 0;
        for queue in queues {
            if !self.gates.contains_key(&queue) && (went || self.signals_finished(&signals, false)) {
                if !went {
                    self.take_tokens(&signals, false);
                    went = true;
                }
                continue;
            }
            held += 1;
            let step = GateStep::Wait { signals: signals.clone(), deadline, id };
            if let Some(gate) = self.gates.get_mut(&queue) {
                gate.push_back(step);
            } else {
                self.gates.insert(queue, VecDeque::from([step]));
                self.pending.push(Pending::Gate { queue });
            }
        }
        if held > 0 && !went {
            self.join_lines(&signals, id);
        }
        if held > 1 || (held > 0 && went) {
            self.shared_waits.insert(id, SharedWait { queues_left: held, went });
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// Works through what a queue held back. True once nothing is left.
    fn run_gate(&mut self, queue: u32, now: Instant) -> bool {
        while let Some(step) = self.gates.get_mut(&queue).and_then(VecDeque::pop_front) {
            match step {
                GateStep::Wait { signals, deadline, id } => {
                    let went = self.shared_waits.get(&id).is_some_and(|shared| shared.went);
                    let can_go = went || self.wait_can_go(&signals, id);
                    if !can_go && now < deadline {
                        let held = GateStep::Wait { signals, deadline, id };
                        self.gates.get_mut(&queue).expect("just read").push_front(held);
                        return false;
                    }
                    if !can_go {
                        log::warn!(
                            "a wait on queue {queue:#x} gave up at its deadline, and its work goes on: {signals:x?}"
                        );
                    }
                    self.leave_wait(id, &signals, can_go);
                }
                GateStep::Ring { doorbell, fence, value } => self.ring(queue, doorbell, fence, value),
                GateStep::Copy { id } => {
                    if self.copy_is_active(id) {
                        self.gates.get_mut(&queue).expect("just read").push_front(GateStep::Copy { id });
                        return false;
                    }
                }
            }
        }
        self.gates.remove(&queue);
        true
    }

    /// One queue is done with a wait, through it or past its deadline. The first queue through takes
    /// what the wait holds, for all the queues it covers.
    pub(crate) fn leave_wait(&mut self, id: u64, signals: &[Signal], through: bool) {
        let Some(shared) = self.shared_waits.get_mut(&id) else {
            self.leave_lines(signals, id);
            if through {
                self.take_tokens(signals, false);
            }
            return;
        };
        let first_through = through && !shared.went;
        shared.went |= through;
        shared.queues_left -= 1;
        let last = shared.queues_left == 0;
        if last {
            self.shared_waits.remove(&id);
        }
        if first_through {
            self.take_tokens(signals, false);
        }
        if first_through || last {
            self.leave_lines(signals, id);
        }
    }

    /// Lets work run, or leaves it behind the queue's gate if there is one.
    pub(crate) fn ring_or_hold(&mut self, queue: u32, doorbell: u64, fence: u64, value: u64) {
        match self.gates.get_mut(&queue) {
            Some(gate) => gate.push_back(GateStep::Ring { doorbell, fence, value }),
            None => self.ring(queue, doorbell, fence, value),
        }
    }

    fn ring(&mut self, queue: u32, doorbell: u64, fence: u64, value: u64) {
        if let Err(error) = self.backend.ring_doorbell(queue, doorbell) {
            log::warn!("queue {queue:#x} couldn't start its work: {error}");
            if let Some(record) = self.queues.get_mut(&queue) {
                record.failed = true;
            }
            // Nothing will run, so stand in for the GPU where the guest would notice.
            if let Err(error) = self.memory.write_u64(fence, value) {
                log::warn!("standing in for a failed doorbell: {error}");
            }
            return;
        }
        self.timings.mark(Stage::Rung);
        self.pending.push(Pending::Watch { queue, fence, value, rung: Instant::now() });
    }

    /// Whether queues may still be given work.
    pub(crate) fn work_allowed(&self) -> bool {
        !self.faults.stopped()
    }

    /// Whether faults have stopped all of this guest's GPU work.
    pub fn gpu_work_stopped(&self) -> bool {
        self.faults.stopped()
    }

    /// Gives up on a queue the GPU has stopped answering, and frees the guest to carry on.
    fn queue_timed_out(&mut self, handle: u32, fence: u64, value: u64, now: Instant) {
        log::error!("queue {handle:#x} did not reach fence value {value:#x} in time");
        if let Some(queue) = self.queues.get_mut(&handle)
            && !queue.failed
        {
            queue.failed = true;
            let space = queue.space;
            self.count_failed_queue(space, now);
        }
        self.store_fence(fence, value);
    }

    /// A process counts as one fault however many of its queues go with it, as a healthy app can have
    /// more queues than a guest gets faults.
    fn count_failed_queue(&mut self, space: SpaceId, now: Instant) {
        let failed = self.failed_queues.entry(space).or_default();
        *failed += 1;
        let failed = *failed;
        if failed == 1 && self.faults.note(now) {
            log::error!(
                "{MOST_FAULTS} processes faulted within {} seconds, so this guest's GPU work has stopped \
                 until it restarts, and what programs in it submit from now on will fail",
                FAULT_WINDOW.as_secs()
            );
        }
        if failed >= MOST_FAILED_QUEUES {
            log::error!("{failed} queues of process {space:?} have failed, so it comes off the GPU");
            self.failed_queues.remove(&space);
            self.stop_space(space);
        }
    }

    /// Adds to a queue's backlog, and holds the reply if too much of it is unfinished.
    ///
    /// The reply is held while the unfinished work is over the limit or over this submission's own
    /// work, whichever is more. The GPU may not finish a submission until the guest sends its next
    /// one, so a submission bigger than the limit must not wait for itself. It waits only for what
    /// was ahead of it.
    ///
    /// A guest that fills its ring loses its device. Its thread waits for this reply, so holding it
    /// stops the ring filling. Only that thread waits, and other queues go on.
    pub(super) fn hold_if_ring_is_full(&mut self, handle: u32, value: u64, work: u32) {
        let Some(limit) = self.backend.unfinished_work_limit() else { return };
        let Some(queue) = self.queues.get_mut(&handle) else { return };
        queue.backlog.push_back((value, work));
        queue.unfinished += u64::from(work);
        let fence = queue.fence;
        if u64::from(work) > limit {
            log::debug!(
                "queue {handle:#x} was given {work:#x} of work at once, over the limit of {limit:#x}"
            );
        }
        if let Some(ticket) = self.ticket {
            let unfinished = self.unfinished_work(handle);
            let allowed = limit.max(u64::from(work));
            if unfinished <= allowed {
                return;
            }
            if u64::from(work) > limit {
                log::debug!(
                    "queue {handle:#x} holds its reply with {unfinished:#x} unfinished, over its own {work:#x} of work"
                );
            } else {
                log::debug!(
                    "queue {handle:#x} holds its reply with {unfinished:#x} unfinished, over the limit of {limit:#x}"
                );
            }
            self.pending.push(Pending::RingRoom {
                queue: handle,
                fence,
                work,
                ticket,
                started: Instant::now(),
            });
            self.hold_reply();
        }
    }

    /// What the GPU has yet to finish on a queue. A queue that has gone has nothing left.
    fn unfinished_work(&mut self, handle: u32) -> u64 {
        let Some(fence) = self.queues.get(&handle).map(|queue| queue.fence) else { return 0 };
        let finished = self.fence_value(fence);
        let Some(queue) = self.queues.get_mut(&handle) else { return 0 };
        while let Some(&(value, work)) = queue.backlog.front()
            && value <= finished
        {
            queue.backlog.pop_front();
            queue.unfinished -= u64::from(work);
        }
        queue.unfinished
    }

    fn release_reply(&mut self, ticket: ReplyTicket, started: Instant) {
        self.timings.reply_released(HeldFor::RingRoom, started.elapsed());
        let bytes = Status::SUCCESS.0.to_le_bytes().to_vec();
        self.replies.push(HeldReply { ticket, bytes });
    }

    /// Checks everything that is waiting, and says how soon to look again. `now` decides deadlines,
    /// and tests pass their own.
    pub fn poll(&mut self, now: Instant) -> Waiting {
        if !self.copies.is_empty() {
            self.poll_copies(now);
        }
        if self.frames.has_work() {
            self.service_frames(now);
        }
        if self.pending.is_empty() {
            return self.how_soon();
        }
        // The two lists swap places, so polling does not allocate.
        let spare = std::mem::take(&mut self.pending_spare);
        let mut items = std::mem::replace(&mut self.pending, spare);
        for item in items.drain(..) {
            if let Some(item) = self.try_finish(item, now) {
                self.pending.push(item);
            }
        }
        self.pending_spare = items;
        self.how_soon()
    }

    fn how_soon(&self) -> Waiting {
        let copies_or_frames = !self.copies.is_empty() || self.frames.is_urgent();
        let waiting = Waiting::of(self.pending.iter(), copies_or_frames);
        if waiting == Waiting::Nothing && self.frames.wants_a_look() { Waiting::Background } else { waiting }
    }

    /// Finishes an item if it can. Otherwise hands it back.
    fn try_finish(&mut self, item: Pending, now: Instant) -> Option<Pending> {
        match item {
            Pending::AfterQueues { targets, deadline, action } => {
                if self.targets_reached(&targets) || now > deadline {
                    self.run_queue_action(action);
                    None
                } else {
                    Some(Pending::AfterQueues { targets, deadline, action })
                }
            }
            Pending::CpuWait { signals, wake_on_any, event, release } => {
                if self.signals_finished(&signals, wake_on_any) {
                    self.take_tokens(&signals, wake_on_any);
                    self.notices.push(Notice::WakeEvent { event, release });
                    None
                } else {
                    Some(Pending::CpuWait { signals, wake_on_any, event, release })
                }
            }
            Pending::Gate { queue } => (!self.run_gate(queue, now)).then_some(Pending::Gate { queue }),
            Pending::Update { fence, value, space, operations } => {
                if self.fence_reached(fence, value) {
                    self.finish_update(&operations, space, fence, value);
                    None
                } else {
                    Some(Pending::Update { fence, value, space, operations })
                }
            }
            Pending::Retry { message, ticket, started } => {
                let mut out = Vec::new();
                let answer = self.handle_message(&message, ticket, &mut out);
                if let (Answer::Now, Some(ticket)) = (answer, ticket) {
                    self.timings.reply_released(HeldFor::Retry, started.elapsed());
                    self.replies.push(HeldReply { ticket, bytes: out });
                }
                None
            }
            Pending::RingRoom { queue, fence, work, ticket, started } => {
                let limit = self.backend.unfinished_work_limit().unwrap_or(u64::MAX);
                let room = self.unfinished_work(queue) <= limit.max(u64::from(work));
                let held = now.duration_since(started);
                if room || held > RING_ROOM_TIMEOUT {
                    if room {
                        log::debug!("queue {queue:#x} was held for {held:?} until the gpu caught up");
                    } else {
                        log::warn!(
                            "queue {queue:#x} was held for {held:?}, and let go at the timeout with the gpu behind"
                        );
                    }
                    self.release_reply(ticket, started);
                    None
                } else {
                    Some(Pending::RingRoom { queue, fence, work, ticket, started })
                }
            }
            Pending::Watch { queue, fence, value, rung } => {
                // A queue that has gone may have passed its fence page on to someone else.
                if !self.queues.contains_key(&queue) {
                    None
                } else if self.fence_value(fence) >= value {
                    self.timings.fence_seen(now.saturating_duration_since(rung));
                    None
                } else if now.saturating_duration_since(rung) > QUEUE_TIMEOUT {
                    self.queue_timed_out(queue, fence, value, now);
                    None
                } else {
                    Some(Pending::Watch { queue, fence, value, rung })
                }
            }
        }
    }

    /// Sets a submission aside for another try, as the backend had no room for it.
    pub(super) fn retry_later(&mut self, message: &[u8]) {
        let ticket = self.ticket;
        self.pending.push(Pending::Retry { message: message.to_vec(), ticket, started: Instant::now() });
        self.hold_reply();
    }

    /// Hands a submission to the backend.
    pub(crate) fn start_on_gpu(
        &mut self,
        queue: u32,
        source: guesthouse_backend::WorkSource,
        private: &[u8],
        value: u64,
    ) -> GpuTook {
        let Some(record) = self.queues.get(&queue) else { return GpuTook::Nothing };
        let (fence, fence_gpu_address) = (record.fence, record.fence_gpu_address);
        let work = guesthouse_backend::Work {
            source,
            queue,
            space: record.space,
            node: self.context_nodes.get(&record.context).copied().unwrap_or(0),
            client_hint: self.context_hints.get(&record.context).copied().unwrap_or(0),
            private,
            fence,
            fence_gpu_address,
            fence_value: value,
            may_run: self.work_allowed(),
            queue_failed: record.failed,
        };
        self.timings.mark(Stage::Started);
        let dropped = !work.may_run || work.queue_failed;
        let start = self.backend.start_work(&mut services!(self), &work);
        self.timings.mark(Stage::Prepared);
        match start {
            WorkStart::Finished if dropped => GpuTook::Dropped,
            WorkStart::Finished => GpuTook::Nothing,
            WorkStart::Ran => {
                self.work_started += 1;
                GpuTook::Nothing
            }
            WorkStart::Busy => GpuTook::NoRoom,
            WorkStart::QueueFailed => {
                self.queues.get_mut(&queue).expect("read above").failed = true;
                GpuTook::Dropped
            }
            WorkStart::Started { doorbell, work } => {
                self.work_started += 1;
                self.queues.get_mut(&queue).expect("read above").last_fence = Some(value);
                self.ring_or_hold(queue, doorbell, fence, value);
                GpuTook::Work(work)
            }
        }
    }
}
