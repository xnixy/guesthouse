//! Handlers for what the guest asks about the adapter, and for escapes.

use super::{Handled, Session, answer_status};
use crate::io_space::IoSpace;
use crate::tables::Allocation;
use guesthouse_backend::{
    Backend, BackendError, GuestBooks, GuestMemory, ProcessMemory, RegistryName, VendorQuery, VideoMemoryUse,
};
use guesthouse_messages::adapter::{
    ASYNC_MESSAGES, AdapterQuery, CapsQuery, DRIVER_MODEL_NUMBER, DisplayMode, FLIP_QUEUE_ANSWER_SIZE,
    IdentityAnswer, SetIoSpace, adapter_identity, description_answer, driver_caps, flip_queue,
    node_description, node_of, physical_adapter_ids, segment_sizes,
};
use guesthouse_messages::address::{
    ReservationRequest, available_for_reservation, memory_answer, memory_group,
};
use guesthouse_messages::indirect;
use guesthouse_messages::queue::{DriverEscape, clock_answer};
use guesthouse_messages::registry::RegistryQuery;
use guesthouse_messages::wire::{align_up, pad_to, put_u32, put_u64, set_u32, u32_at};
use guesthouse_messages::{MessageHeader, Status};
use std::collections::HashMap;

/// GPU timestamps count nanoseconds.
const GPU_CLOCK_HERTZ: u64 = 1_000_000_000;
/// The guest's CPU clock ticks in units of 100 ns.
const CPU_CLOCK_TICK_NANOS: u128 = 100;
const ESCAPE_ALIGN: u64 = 8;
const VIDEO_MEMORY_GROUP: u32 = 0;
const STATUS_SIZE: usize = 4;

/// Adapter queries answered with zeros when the vendor has nothing better: update status, feature
/// caps, a unique id, performance data, cross-adapter support.
const ZERO_ANSWERS: [u32; 8] = [11, 17, 22, 55, 60, 61, 62, 76];

const NO_IO_SPACE: BackendError = BackendError(std::borrow::Cow::Borrowed("no IO space yet"));

/// What each process holds, by its PID in the guest. A process the guest never named is left out,
/// as a list is no place for a handle of this side's.
fn memory_by_process(allocations: &HashMap<u32, Allocation>, pids: &HashMap<u32, u32>) -> Vec<ProcessMemory> {
    let mut held: HashMap<u32, u64> = HashMap::new();
    for allocation in allocations.values() {
        if let Some(&pid) = pids.get(&allocation.owner) {
            *held.entry(pid).or_default() += allocation.size;
        }
    }
    let mut processes: Vec<ProcessMemory> =
        held.into_iter().map(|(pid, bytes)| ProcessMemory { pid, bytes }).collect();
    processes.sort_unstable_by_key(|process| process.pid);
    processes
}

impl<B: Backend, M: GuestMemory> Session<B, M> {
    pub(super) fn adapter_identity(&self, out: &mut Vec<u8>) {
        let facts = self.backend.facts();
        let answer = IdentityAnswer {
            description: &facts.name,
            vendor: facts.card.vendor,
            device: facts.card.device,
            flags: if self.config.async_messages { ASYNC_MESSAGES } else { 0 },
        };
        adapter_identity(&answer, out);
    }

    pub(super) fn set_io_space(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = SetIoSpace::parse(message)?;
        let placed = IoSpace::place(request.start, &self.memory.window()?);
        // Fence pages already live where the GPU was told, so moving them now would split the two.
        let moved = self.fences.block.is_some() && placed != self.io_space;
        let Some(placed) = placed.filter(|_| !moved) else {
            log::warn!("refused an IO space at {:#x}, {:#x} bytes", request.start, request.length);
            answer_status(Status::INVALID_PARAMETER, out);
            return Ok(());
        };
        self.io_space = Some(placed);
        log::debug!("IO space at {:#x}, {:#x} bytes", request.start, request.length);
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// The IO space, or a failure for a guest that hasn't placed one.
    pub(crate) fn placed_io_space(&self) -> Result<IoSpace, BackendError> {
        self.io_space.ok_or(NO_IO_SPACE)
    }

    pub(super) fn driver_escape(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let escape = DriverEscape::parse(message)?;
        out.extend_from_slice(escape.data);
        let ours = indirect::is_ours(escape.data);
        if !ours {
            let process = MessageHeader::process_of(message)?;
            let total = self.config.limits.video_cap.unwrap_or(self.backend.facts().video_memory);
            let allocations = &self.allocations;
            let memory = || VideoMemoryUse { total, used: allocations.values().map(|one| one.size).sum() };
            let pids = &self.guest_pids;
            let processes = || memory_by_process(allocations, pids);
            self.backend.escape(process, out, &GuestBooks { memory: &memory, processes: &processes });
        }
        pad_to(out, align_up(escape.size as u64, ESCAPE_ALIGN) as usize);
        if ours {
            self.indirect_message(MessageHeader::process_of(message)?, out)?;
        }
        Ok(())
    }

    /// The guest's graphics kernel keeps these numbers itself on real hardware, and nothing here
    /// keeps them, so saying no is the honest answer. A tool that polls would fill the log.
    pub(super) fn refuse_statistics(&mut self, out: &mut Vec<u8>) {
        if !std::mem::replace(&mut self.statistics_refused, true) {
            log::warn!("the guest asked for statistics, which aren't kept here, and was refused");
        }
        put_u32(out, Status::NOT_SUPPORTED.0);
    }

    pub(super) fn query_driver_caps(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let query = CapsQuery::parse(message)?;
        driver_caps(query, self.backend.facts().node_count, out)?;
        Ok(())
    }

    pub(super) fn describe_node(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        node_description(node_of(message)?, out);
        Ok(())
    }

    pub(super) fn query_video_memory(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let group = memory_group(message)?;
        let process = MessageHeader::process_of(message)?;
        let reserved = self.reservations.get(&(process, group)).copied().unwrap_or(0);
        let usage = if group == VIDEO_MEMORY_GROUP {
            self.allocations.values().map(|allocation| allocation.size).sum()
        } else {
            0
        };
        memory_answer(self.group_budget(group), usage, reserved, out);
        Ok(())
    }

    /// Keeps the amount a process asks to have kept, up to what a usage query says it may. Nothing
    /// is set aside: the number only comes back in the process's own usage queries.
    pub(super) fn change_video_memory_reservation(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let request = ReservationRequest::parse(message)?;
        let process = MessageHeader::process_of(message)?;
        if request.bytes > available_for_reservation(self.group_budget(request.group)) {
            answer_status(Status::INVALID_PARAMETER, out);
            return Ok(());
        }
        if request.bytes == 0 {
            self.reservations.remove(&(process, request.group));
        } else {
            self.reservations.insert((process, request.group), request.bytes);
        }
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    fn group_budget(&self, group: u32) -> u64 {
        let limits = self.config.limits;
        if group == VIDEO_MEMORY_GROUP { limits.video_budget } else { limits.system_budget }
    }

    /// The two clocks agree in speed but not in where they start, which is all the guest needs.
    pub(super) fn query_clock(&mut self, out: &mut Vec<u8>) -> Handled {
        let cpu_clock = self.started.elapsed().as_nanos() / CPU_CLOCK_TICK_NANOS;
        clock_answer(GPU_CLOCK_HERTZ, self.backend.gpu_clock()?, cpu_clock as u64, out);
        Ok(())
    }

    pub(super) fn mark_device_failed(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let reason = u32_at(message, 0x1C)?;
        log::warn!("the guest marked a device as failed, reason {reason:#x}");
        answer_status(Status::SUCCESS, out);
        Ok(())
    }

    /// Offers a query to the vendor, and answers what is left from what the core knows.
    pub(super) fn query_adapter(&mut self, message: &[u8], out: &mut Vec<u8>) -> Handled {
        let query = AdapterQuery::parse(message)?;
        let process = MessageHeader::process_of(message)?;
        let vendor_query = VendorQuery {
            kind: query.kind,
            size: query.size,
            data: query.data,
            process_is_32_bit: self.processes_32_bit.contains(&process),
        };
        put_u32(out, Status::SUCCESS.0);
        let status = match self.backend.adapter_answer(&vendor_query, out) {
            Some(status) => status,
            None => self.neutral_adapter_answer(&query, out)?,
        };
        set_u32(out, 0, status.0);
        if status == Status::NOT_SUPPORTED {
            out.truncate(STATUS_SIZE);
        }
        Ok(())
    }

    fn neutral_adapter_answer(
        &mut self,
        query: &AdapterQuery<'_>,
        out: &mut Vec<u8>,
    ) -> Result<Status, super::Failure> {
        let facts = self.backend.facts();
        let limits = self.config.limits;
        match query.kind {
            AdapterQuery::SEGMENT_SIZES => {
                let dedicated = limits.video_cap.unwrap_or(facts.video_memory);
                segment_sizes(dedicated, limits.system_budget, out);
            }
            AdapterQuery::FLIP_QUEUE if query.size == FLIP_QUEUE_ANSWER_SIZE => flip_queue(out),
            AdapterQuery::DRIVER_MODEL | AdapterQuery::DRIVER_MODEL_FOR_RENDER => {
                put_u32(out, DRIVER_MODEL_NUMBER)
            }
            AdapterQuery::DRIVER_VERSION | AdapterQuery::USER_DRIVER_VERSION => {
                put_u64(out, facts.guest_driver_version)
            }
            AdapterQuery::DESCRIPTION => description_answer(&facts.name, query.size, out),
            AdapterQuery::PHYSICAL_ADAPTER_COUNT => put_u32(out, 1),
            AdapterQuery::PHYSICAL_ADAPTER_IDS => {
                physical_adapter_ids(facts.card.vendor, facts.card.device, facts.card.revision, out)
            }
            AdapterQuery::DIRECT_FLIP | AdapterQuery::CONSOLE_CAPS => pad_to(out, STATUS_SIZE + query.size),
            AdapterQuery::REGISTRY => return self.registry_answer(query.data, out),
            kind if ZERO_ANSWERS.contains(&kind) => pad_to(out, STATUS_SIZE + query.size),
            AdapterQuery::CURRENT_DISPLAY_MODE if query.size >= DisplayMode::ANSWER_SIZE => {
                // Only the display driver's open ring can answer. Saying what Basic Display has makes
                // Windows present to it far less often, and a viewer of that screen goes short of frames.
                let Some(mode) = self.display_mode() else {
                    log::debug!("no display mode to give");
                    return Ok(Status::NOT_SUPPORTED);
                };
                let source = u32_at(query.data, 0).unwrap_or(0);
                log::debug!("display mode for source {source}: {mode:?}");
                mode.answer(source, out);
                pad_to(out, STATUS_SIZE + query.size);
            }
            kind => {
                log::debug!("adapter query {kind} has no answer");
                return Ok(Status::NOT_SUPPORTED);
            }
        }
        Ok(Status::SUCCESS)
    }

    fn registry_answer(&mut self, buffer: &[u8], out: &mut Vec<u8>) -> Result<Status, super::Failure> {
        let query = RegistryQuery::parse(buffer)?;
        let lookup = match &query.name {
            RegistryName::Value(name) => RegistryName::Value(name.to_lowercase()),
            path => path.clone(),
        };
        // Windows fails the lookup of a value that isn't there. A success with the buffer as it came
        // reads as a value found, which is how Direct3D 9 took it.
        let Some(value) = self.backend.registry_value(&lookup) else {
            log::debug!("no registry value for {:?}", query.name);
            query.unchanged(out);
            return Ok(Status::NOT_FOUND);
        };
        // A loader that asks for the wrong type tries the other when this fails.
        if value.value_type.is_some_and(|value_type| value_type != query.value_type) {
            query.unchanged(out);
            return Ok(Status::INVALID_PARAMETER);
        }
        query.answer(&value.bytes, out)?;
        Ok(Status::SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocation(owner: u32, size: u64) -> Allocation {
        Allocation { owner, size, ..Allocation::default() }
    }

    #[test]
    fn memory_is_added_up_by_guest_pid_and_a_nameless_process_is_left_out() {
        let allocations = HashMap::from([
            (1, allocation(0x10, 4096)),
            (2, allocation(0x10, 8192)),
            (3, allocation(0x20, 1)),
            (4, allocation(0x30, 7)),
        ]);
        let pids = HashMap::from([(0x10, 6600), (0x20, 1532)]);
        assert_eq!(
            memory_by_process(&allocations, &pids),
            [ProcessMemory { pid: 1532, bytes: 1 }, ProcessMemory { pid: 6600, bytes: 12288 }]
        );
    }
}
