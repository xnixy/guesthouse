//! Controls for the RM inside an escape. Each runs on this daemon's own objects, and only if it is on
//! the list. The guest's handles mean nothing here: a control's number names its class, and so the
//! object it belongs on.

use super::{EscapeFacts, get, processes, pstate, put, time_pairs};
use crate::rm::classes::{
    BUS_ENTRIES_THAT_CLEAR, CONTROL_GET_CLASS_ENGINE, CONTROL_GET_PROBED_IDS, ENGINE_GRAPHICS,
};
use crate::rm::device::Device;
use crate::rm::{Rm, RmError, STATUS_INVALID_PARAM_STRUCT, STATUS_NOT_SUPPORTED};

// An RM control inside an escape.
const CONTROL_COMMAND: usize = 0x38;
const CONTROL_SIZE: usize = 0x3C;
const CONTROL_STATUS: usize = 0x40;
const CONTROL_PARAMS: usize = 0x44;

const CLASS_PARAM_ANSWER: usize = 4;

/// These ask about one GPU by ID, and a guest has no business asking about the host's others.
const CONTROLS_ABOUT_ONE_GPU: [u32; 7] =
    [0x0000_0203, 0x0000_0205, 0x0000_021B, 0x0000_0275, 0x0000_0279, 0x0000_0289, 0x0000_0290];
/// Lists the GPUs in use. The guest hears of its own card and no other.
const CONTROL_GET_ATTACHED_IDS: u32 = 0x0000_0201;

/// Our rule, not the driver's: a sensor control that isn't on the list is refused. Success with an
/// empty buffer would tell the guest its card is at 0 degrees and 0 MHz, and a refusal is honest.
/// Real readings reach the guest another way, from the host's sampler.
const SENSOR_CONTROL_CLASS: u32 = 0x2080;
/// `NV2080_CTRL_THERMAL`, `_CLK`, `_PERF` and `_VOLT` in `ctrl2080base.h`, each with its
/// `_LEGACY_NON_PRIVILEGED` and `_LEGACY_PRIVILEGED` numbers.
const SENSOR_CONTROL_CATEGORIES: [u32; 12] =
    [0x05, 0x85, 0xC5, 0x10, 0x90, 0xD0, 0x20, 0xA0, 0xE0, 0x32, 0xB2, 0xF2];

const CONTROL_BUS_INFO: u32 = crate::record::card::BUS_INFO.0;
const BUS_LIST_SIZE: usize = 0;
const BUS_LIST: usize = 4;
const BUS_ENTRY_SIZE: usize = 8;
const BUS_LIST_LONGEST: u32 = 0x34;

const MAX_PROBED_GPUS: usize = 32;
const INVALID_GPU_ID: u32 = 0xFFFF_FFFF;

/// The answer to the list of probed GPUs: this card, and invalid entries, not zeros.
fn probed_ids(gpu_id: u32, params: &mut [u8]) {
    let mut words = [INVALID_GPU_ID; 2 * MAX_PROBED_GPUS].to_vec();
    words[0] = gpu_id;
    words.extend([0; MAX_PROBED_GPUS]);
    for (index, word) in words.into_iter().enumerate() {
        put(params, index * 4, word);
    }
}

/// The answer to the list of attached GPUs: this card, and invalid entries after it.
fn attached_ids(gpu_id: u32, params: &mut [u8]) {
    for at in (0..params.len()).step_by(4) {
        put(params, at, if at == 0 { gpu_id } else { INVALID_GPU_ID });
    }
}

fn is_sensor_control(command: u32) -> bool {
    command >> 16 == SENSOR_CONTROL_CLASS && SENSOR_CONTROL_CATEGORIES.contains(&(command >> 8 & 0xFF))
}

/// Whether a list of bus entries asks for one that clears a counter as it is read, which would
/// change the host's own error counts. The RM refuses a list longer than this, so that's all it reads.
fn clears_counters(params: &[u8]) -> bool {
    let Some(size) = get(params, BUS_LIST_SIZE) else { return false };
    (0..size.min(BUS_LIST_LONGEST) as usize).any(|entry| {
        get(params, BUS_LIST + entry * BUS_ENTRY_SIZE)
            .is_some_and(|index| BUS_ENTRIES_THAT_CLEAR.contains(&index))
    })
}

/// Whether this listed control must stop here, on what the guest put in it.
fn refused<R: Rm>(device: &mut Device<R>, command: u32, params: &[u8]) -> Result<bool, RmError> {
    if CONTROLS_ABOUT_ONE_GPU.contains(&command) {
        return Ok(get(params, 0) != Some(device.gpu_id()?));
    }
    Ok(command == CONTROL_BUS_INFO && clears_counters(params))
}

/// The parameters, if the guest sized them as the layout says. The RM says the same of a wrong
/// size, so the guest can't tell.
fn params_of(data: &mut [u8], size: u32, wanted: usize) -> Option<&mut [u8]> {
    let params = data.get_mut(CONTROL_PARAMS..).and_then(|rest| rest.get_mut(..wanted));
    params.filter(|_| size as usize == wanted)
}

/// Answers the controls this side keeps the books for, and says whether this was one of them.
fn answered_here(facts: &EscapeFacts<'_>, command: u32, size: u32, data: &mut [u8]) -> bool {
    let status = match command {
        processes::CONTROL_GET_PIDS => {
            let Some(params) = params_of(data, size, processes::PIDS_SIZE) else { return false };
            let listed = processes::list(&(facts.books.processes)(), params);
            if listed { 0 } else { STATUS_NOT_SUPPORTED }
        }
        processes::CONTROL_GET_PID_INFO => {
            let Some(params) = params_of(data, size, processes::INFO_SIZE) else { return false };
            processes::describe(&(facts.books.processes)(), params);
            0
        }
        pstate::CONTROL => {
            let Some(params) = params_of(data, size, pstate::SIZE) else { return false };
            if pstate::answer(facts.live.as_ref(), params) { 0 } else { STATUS_NOT_SUPPORTED }
        }
        _ => return false,
    };
    put(data, CONTROL_STATUS, status);
    true
}

pub(super) fn run<R: Rm>(
    device: &mut Device<R>,
    facts: &EscapeFacts<'_>,
    data: &mut [u8],
) -> Result<(), RmError> {
    let (Some(command), Some(size)) = (get(data, CONTROL_COMMAND), get(data, CONTROL_SIZE)) else {
        return Ok(());
    };
    if answered_here(facts, command, size, data) {
        return Ok(());
    }
    if command == CONTROL_GET_CLASS_ENGINE
        && let Some(handle) = get(data, CONTROL_PARAMS)
        && let Some(&(_, class)) = facts.channel_classes.iter().find(|object| object.0 == handle)
    {
        let at = CONTROL_PARAMS + CLASS_PARAM_ANSWER;
        put(data, at, class);
        put(data, at + 4, class);
        put(data, at + 8, ENGINE_GRAPHICS);
        return Ok(());
    }
    if command == CONTROL_GET_PROBED_IDS {
        // The guest gets the one card it uses, whatever else the host has.
        let gpu_id = device.gpu_id()?;
        if data.len() > CONTROL_PARAMS {
            probed_ids(gpu_id, &mut data[CONTROL_PARAMS..]);
        }
        put(data, CONTROL_STATUS, 0);
        return Ok(());
    }
    let Some(params_size) = device.guest_control_size(command) else {
        log::debug!("control {command:#x} is not run for a guest");
        if is_sensor_control(command) {
            put(data, CONTROL_STATUS, STATUS_NOT_SUPPORTED);
        }
        return Ok(());
    };
    // Only parameters of the right shape are ever handed over.
    let Some(params) = params_of(data, size, params_size) else {
        put(data, CONTROL_STATUS, STATUS_INVALID_PARAM_STRUCT);
        return Ok(());
    };
    if refused(device, command, params)? {
        log::debug!("control {command:#x} asked for more than a guest may have");
        put(data, CONTROL_STATUS, STATUS_NOT_SUPPORTED);
        return Ok(());
    }
    if command == CONTROL_GET_ATTACHED_IDS {
        attached_ids(device.gpu_id()?, params);
        put(data, CONTROL_STATUS, 0);
        return Ok(());
    }
    if command == time_pairs::CONTROL && time_pairs::answer(&device.rm, &mut device.guest_clock, params) {
        put(data, CONTROL_STATUS, 0);
        return Ok(());
    }
    let target = device.control_target(command);
    let status = match device.guest_control(command).and_then(|control| control.host) {
        Some(layout) => {
            let mut host = layout.to_host(params);
            let status = device.rm.control(target, command, &mut host)?;
            layout.to_guest(&host, params);
            status
        }
        None => device.rm.control(target, command, params)?,
    };
    put(data, CONTROL_STATUS, status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::FUNCTION_RM_CONTROL;
    use super::super::tests::{NO_FACTS, device, escape};
    use super::*;
    use crate::profile::{BLACKWELL, GUEST_617, HOST_615, guest_channel_classes};
    use crate::rm::fake::{Call, FakeRm};
    use crate::sensors::Snapshot;
    use guesthouse_backend::{GuestBooks, ProcessMemory};

    const GET_NAME_STRING: u32 = 0x2080_0110;
    const NAME_SIZE: usize = 68;

    fn control(command: u32, params: usize) -> Vec<u8> {
        let mut data = escape(FUNCTION_RM_CONTROL, CONTROL_PARAMS + params);
        put(&mut data, CONTROL_COMMAND, command);
        put(&mut data, CONTROL_SIZE, params as u32);
        data
    }

    #[test]
    fn a_control_not_on_the_list_never_reaches_the_rm() {
        let mut device = device();
        let mut data = control(0x2080_9999, 8);
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        assert!(!device.rm.calls.iter().any(|call| matches!(call, Call::Control { .. })));
    }

    #[test]
    fn a_control_on_the_list_runs_on_the_object_its_number_names() {
        let mut device = device();
        let mut data = control(GET_NAME_STRING, NAME_SIZE);
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        let ran = device.rm.calls.iter().find_map(|call| match call {
            Call::Control { object, command, .. } => Some((*object, *command)),
            _ => None,
        });
        assert_eq!(ran, Some((device.subdevice, GET_NAME_STRING)));
        assert_eq!(get(&data, CONTROL_STATUS), Some(crate::rm::STATUS_NOT_SUPPORTED));
    }

    #[test]
    fn a_control_of_the_wrong_size_never_reaches_the_rm() {
        for size in [0, NAME_SIZE - 1, NAME_SIZE + 1, 0xFFFF_FFFF] {
            let mut device = device();
            let mut data = control(GET_NAME_STRING, 0x100);
            put(&mut data, CONTROL_SIZE, size as u32);
            run(&mut device, &NO_FACTS, &mut data).unwrap();
            assert!(!device.rm.calls.iter().any(|call| matches!(call, Call::Control { .. })), "{size}");
            assert_eq!(get(&data, CONTROL_STATUS), Some(crate::rm::STATUS_INVALID_PARAM_STRUCT));
        }
    }

    #[test]
    fn a_control_cut_short_by_its_escape_never_reaches_the_rm() {
        let mut device = device();
        let mut data = control(GET_NAME_STRING, NAME_SIZE - 4);
        put(&mut data, CONTROL_SIZE, NAME_SIZE as u32);
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        assert!(!device.rm.calls.iter().any(|call| matches!(call, Call::Control { .. })));
        assert_eq!(get(&data, CONTROL_STATUS), Some(crate::rm::STATUS_INVALID_PARAM_STRUCT));
    }

    #[test]
    fn a_guest_channel_object_is_answered_from_the_generation() {
        let classes = guest_channel_classes(&GUEST_617, &BLACKWELL);
        let mut data = control(CONTROL_GET_CLASS_ENGINE, 16);
        put(&mut data, CONTROL_PARAMS, classes[2].0);
        let facts = EscapeFacts { channel_classes: &classes, ..NO_FACTS };
        run(&mut device(), &facts, &mut data).unwrap();
        assert_eq!(get(&data, CONTROL_PARAMS + 4), Some(classes[2].1));
        assert_eq!(get(&data, CONTROL_PARAMS + 12), Some(ENGINE_GRAPHICS));
    }

    #[test]
    fn the_probed_list_names_this_card_and_marks_the_rest_invalid() {
        let mut data = control(CONTROL_GET_PROBED_IDS, 3 * MAX_PROBED_GPUS * 4);
        run(&mut device(), &NO_FACTS, &mut data).unwrap();
        assert_eq!(get(&data, CONTROL_PARAMS), Some(0x300));
        assert_eq!(get(&data, CONTROL_PARAMS + 4), Some(INVALID_GPU_ID));
        assert_eq!(get(&data, CONTROL_PARAMS + 2 * MAX_PROBED_GPUS * 4), Some(0));
    }

    fn reached_the_rm(device: &Device<FakeRm>, wanted: u32) -> bool {
        device.rm.calls.iter().any(|call| match call {
            Call::Control { command, .. } => *command == wanted,
            _ => false,
        })
    }

    const ECC_STATUS: u32 = 0x2080_012F;

    #[test]
    fn a_control_that_grew_on_the_hosts_branch_is_carried_across() {
        let mut rm = FakeRm::new();
        rm.answer(crate::rm::classes::CONTROL_GET_GPU_ID, |params| {
            params[..4].copy_from_slice(&0x300u32.to_le_bytes());
            0
        });
        rm.answer(ECC_STATUS, |params| {
            params[1800] = 1;
            params[1804..1808].copy_from_slice(&7u32.to_le_bytes());
            0
        });
        let mut device = Device::open(rm, &crate::profile::HOST_620).unwrap();
        let mut data = control(ECC_STATUS, 1664);
        put(&mut data, CONTROL_PARAMS + 1648, 0xABCD);
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        let sent = device.rm.calls.iter().find_map(|call| match call {
            Call::Control { command: ECC_STATUS, params, .. } => Some(params.clone()),
            _ => None,
        });
        let sent = sent.expect("it reached the RM");
        assert_eq!(sent.len(), 1824);
        assert_eq!(get(&sent, 1808), Some(0xABCD));
        assert_eq!(data[CONTROL_PARAMS + 1640], 1);
        assert_eq!(get(&data, CONTROL_PARAMS + 1644), Some(7));
        assert_eq!(get(&data, CONTROL_STATUS), Some(0));
    }

    #[test]
    fn every_listed_control_is_listed_once() {
        let mut sorted: Vec<u32> = HOST_615.guest_controls.iter().map(|control| control.command).collect();
        sorted.sort_unstable();
        assert!(sorted.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn the_attached_list_names_this_card_alone_and_never_reaches_the_rm() {
        let mut device = device();
        let size = device.guest_control_size(CONTROL_GET_ATTACHED_IDS).unwrap();
        let mut data = control(CONTROL_GET_ATTACHED_IDS, size);
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        assert_eq!(get(&data, CONTROL_PARAMS), Some(0x300));
        assert_eq!(get(&data, CONTROL_PARAMS + 4), Some(INVALID_GPU_ID));
        assert!(!reached_the_rm(&device, CONTROL_GET_ATTACHED_IDS));
    }

    #[test]
    fn a_question_about_this_card_reaches_the_rm() {
        for command in CONTROLS_ABOUT_ONE_GPU {
            let mut device = device();
            let mut data = control(command, device.guest_control_size(command).unwrap());
            put(&mut data, CONTROL_PARAMS, 0x300);
            run(&mut device, &NO_FACTS, &mut data).unwrap();
            assert!(reached_the_rm(&device, command), "{command:#x}");
        }
    }

    #[test]
    fn a_question_about_another_gpu_stops_here() {
        for command in CONTROLS_ABOUT_ONE_GPU {
            let mut device = device();
            let mut data = control(command, device.guest_control_size(command).unwrap());
            put(&mut data, CONTROL_PARAMS, 0x400);
            run(&mut device, &NO_FACTS, &mut data).unwrap();
            assert!(!reached_the_rm(&device, command), "{command:#x}");
            assert_eq!(get(&data, CONTROL_STATUS), Some(STATUS_NOT_SUPPORTED));
        }
    }

    fn bus_list(entries: &[u32]) -> Vec<u8> {
        let mut data = control(CONTROL_BUS_INFO, crate::record::card::BUS_INFO.1);
        put(&mut data, CONTROL_PARAMS + BUS_LIST_SIZE, entries.len() as u32);
        for (slot, &index) in entries.iter().enumerate() {
            put(&mut data, CONTROL_PARAMS + BUS_LIST + slot * BUS_ENTRY_SIZE, index);
        }
        data
    }

    #[test]
    fn plain_bus_entries_reach_the_rm() {
        let mut device = device();
        run(&mut device, &NO_FACTS, &mut bus_list(&[0, 1, 0x1F])).unwrap();
        assert!(reached_the_rm(&device, CONTROL_BUS_INFO));
    }

    #[test]
    fn a_bus_entry_that_clears_a_counter_never_reaches_the_rm() {
        for index in BUS_ENTRIES_THAT_CLEAR {
            let mut device = device();
            let mut data = bus_list(&[1, index]);
            run(&mut device, &NO_FACTS, &mut data).unwrap();
            assert!(!reached_the_rm(&device, CONTROL_BUS_INFO), "{index:#x}");
            assert_eq!(get(&data, CONTROL_STATUS), Some(STATUS_NOT_SUPPORTED));
        }
    }

    #[test]
    fn a_clearing_entry_past_the_stated_list_is_not_read_by_the_rm_either() {
        let mut data = bus_list(&[1, BUS_ENTRIES_THAT_CLEAR[0]]);
        put(&mut data, CONTROL_PARAMS + BUS_LIST_SIZE, 1);
        let mut device = device();
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        assert!(reached_the_rm(&device, CONTROL_BUS_INFO));
    }

    #[test]
    fn time_pairs_are_answered_here_once_the_timer_can_be_read() {
        let mut device = device();
        device.rm.gpu_timer = Some(5_000);
        let size = device.guest_control_size(time_pairs::CONTROL).unwrap();
        let mut data = control(time_pairs::CONTROL, size);
        data[CONTROL_PARAMS] = 1;
        data[CONTROL_PARAMS + 1] = 1;
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        assert!(!reached_the_rm(&device, time_pairs::CONTROL));
        assert_eq!(get(&data, CONTROL_STATUS), Some(0));
    }

    #[test]
    fn time_pairs_go_to_the_rm_while_the_timer_cannot_be_read() {
        let mut device = device();
        let size = device.guest_control_size(time_pairs::CONTROL).unwrap();
        let mut data = control(time_pairs::CONTROL, size);
        data[CONTROL_PARAMS] = 1;
        data[CONTROL_PARAMS + 1] = 1;
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        assert!(reached_the_rm(&device, time_pairs::CONTROL));
    }

    #[test]
    fn attaching_stays_off_the_list() {
        const CONTROL_ATTACH_IDS: u32 = 0x0000_0215;
        let mut device = device();
        let mut data = control(CONTROL_ATTACH_IDS, 0x84);
        run(&mut device, &NO_FACTS, &mut data).unwrap();
        assert!(!reached_the_rm(&device, CONTROL_ATTACH_IDS));
        assert_eq!(get(&data, CONTROL_STATUS), Some(0), "the guest's one card is attached already");
    }

    #[test]
    fn a_sensor_control_is_refused_and_not_answered_with_zeros() {
        for command in [0x2080_852A, 0x2080_0522, 0x2080_9009, 0x2080_A040, 0x2080_B201, 0x2080_2096] {
            let mut device = device();
            let mut data = control(command, 8);
            run(&mut device, &NO_FACTS, &mut data).unwrap();
            assert_eq!(get(&data, CONTROL_STATUS), Some(STATUS_NOT_SUPPORTED), "{command:#x}");
            assert!(!reached_the_rm(&device, command), "{command:#x}");
        }
    }

    #[test]
    fn controls_the_guests_drivers_need_to_succeed_keep_their_status() {
        // The first two are NV0080_CTRL_CMD_PERF_CUDA_LIMIT_SET_CONTROL and
        // NV0090_CTRL_CMD_SET_LG_SECTOR_PROMOTION, which would change the host's card, so they
        // don't run. Guests send all three as their drivers start, and each one gets success.
        for command in [0x0080_1909, 0x0090_010B, 0x0080_9908] {
            let mut device = device();
            let mut data = control(command, 16);
            run(&mut device, &NO_FACTS, &mut data).unwrap();
            assert_eq!(get(&data, CONTROL_STATUS), Some(0), "{command:#x}");
            assert!(!reached_the_rm(&device, command), "{command:#x}");
        }
    }

    fn books_with(processes: Vec<ProcessMemory>) -> EscapeFacts<'static> {
        let processes: &'static Vec<ProcessMemory> = Box::leak(Box::new(processes));
        let list: &'static dyn Fn() -> Vec<ProcessMemory> = Box::leak(Box::new(move || processes.clone()));
        let books: &'static GuestBooks<'static> =
            Box::leak(Box::new(GuestBooks { memory: &super::super::tests::NO_MEMORY, processes: list }));
        EscapeFacts { books, ..NO_FACTS }
    }

    #[test]
    fn the_process_list_is_the_guests_own_and_never_reaches_the_rm() {
        let facts = books_with(vec![ProcessMemory { pid: 1532, bytes: 1 << 20 }]);
        let mut device = device();
        let mut data = control(processes::CONTROL_GET_PIDS, processes::PIDS_SIZE);
        run(&mut device, &facts, &mut data).unwrap();
        assert!(!reached_the_rm(&device, processes::CONTROL_GET_PIDS));
        assert_eq!(get(&data, CONTROL_STATUS), Some(0));
        assert_eq!(get(&data, CONTROL_PARAMS + 8), Some(1));
        assert_eq!(get(&data, CONTROL_PARAMS + 12), Some(1532));
        let mut data = control(processes::CONTROL_GET_PID_INFO, processes::INFO_SIZE);
        put(&mut data, CONTROL_PARAMS, 1);
        put(&mut data, CONTROL_PARAMS + 8, 1532);
        run(&mut device, &facts, &mut data).unwrap();
        assert!(!reached_the_rm(&device, processes::CONTROL_GET_PID_INFO));
        assert_eq!(get(&data, CONTROL_PARAMS + 8 + 0x10), Some(1 << 20));
    }

    #[test]
    fn a_process_list_of_the_wrong_size_is_not_answered() {
        let facts = books_with(vec![ProcessMemory { pid: 1532, bytes: 1 << 20 }]);
        let mut device = device();
        let mut data = control(processes::CONTROL_GET_PIDS, processes::PIDS_SIZE - 4);
        run(&mut device, &facts, &mut data).unwrap();
        assert!(!reached_the_rm(&device, processes::CONTROL_GET_PIDS));
        assert_eq!(get(&data, CONTROL_PARAMS + 8), Some(0));
    }

    #[test]
    fn the_performance_state_comes_from_the_sampler_or_is_refused() {
        let mut data = control(pstate::CONTROL, pstate::SIZE);
        run(&mut device(), &NO_FACTS, &mut data).unwrap();
        assert_eq!(get(&data, CONTROL_STATUS), Some(STATUS_NOT_SUPPORTED));
        let live = Snapshot { performance_state: Some(0), ..Snapshot::default() };
        let mut data = control(pstate::CONTROL, pstate::SIZE);
        let mut device = device();
        run(&mut device, &EscapeFacts { live: Some(live), ..NO_FACTS }, &mut data).unwrap();
        assert_eq!(get(&data, CONTROL_STATUS), Some(0));
        assert_eq!(get(&data, CONTROL_PARAMS), Some(1));
        assert!(!reached_the_rm(&device, pstate::CONTROL));
    }

    #[test]
    fn other_unlisted_controls_keep_their_status() {
        let mut data = control(0x2080_1399, 8);
        run(&mut device(), &NO_FACTS, &mut data).unwrap();
        assert_eq!(get(&data, CONTROL_STATUS), Some(0));
    }
}
