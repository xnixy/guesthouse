//! What happens when the GPU stops answering: who loses it, and what the guest hears.

use super::*;
use crate::pending::{MOST_FAILED_QUEUES, QUEUE_TIMEOUT};
use crate::{FAULT_WINDOW, MOST_FAULTS};

const FIRST_PROCESS: u32 = 100;
const OTHER_PROCESS: u32 = 200;
const PAST_THE_TIMEOUT: Duration = Duration::from_secs(1);

struct Lane {
    queue: u32,
    fence: u64,
}

fn running_session() -> TestSession {
    let mut session = session();
    session.backend_mut().run_work = true;
    session
}

fn lane_of(session: &mut TestSession, process: u32) -> Lane {
    let mut context = message(Command::CreateContext, 0x34);
    set_u32(&mut context, 8, process);
    let context = u32_at(&send(session, &context), 0x18).unwrap();
    let mut request = message(Command::CreateHardwareQueue, 0x48);
    set_u32(&mut request, 8, process);
    set_u32(&mut request, 0x38, context);
    set_u32(&mut request, 0x40, 4);
    let reply = send(session, &request);
    Lane { queue: u32_at(&reply, 0x1C).unwrap(), fence: u64_at(&reply, 0x28).unwrap() }
}

fn lanes_of(session: &mut TestSession, process: u32, count: u32) -> Vec<Lane> {
    (0..count).map(|_| lane_of(session, process)).collect()
}

fn submit_status(session: &mut TestSession, lane: &Lane, value: u64) -> u32 {
    let mut request = message(Command::SubmitToHardwareQueue, 0x50);
    set_u32(&mut request, 0x18, lane.queue);
    set_u64(&mut request, 0x20, value);
    status_of(&send(session, &request))
}

/// Gives these queues work that never finishes, and lets them all time out.
fn hang(session: &mut TestSession, lanes: &[Lane], at: Instant) {
    for lane in lanes {
        assert_eq!(submit_status(session, lane, 1), SUCCESS);
    }
    session.poll(at + QUEUE_TIMEOUT + PAST_THE_TIMEOUT);
}

fn doorbells(session: &TestSession) -> usize {
    session.backend().doorbells.len()
}

#[test]
fn one_failed_queue_leaves_the_rest_of_its_process_on_the_gpu() {
    let mut session = running_session();
    let lanes = lanes_of(&mut session, FIRST_PROCESS, 2);
    hang(&mut session, &lanes[..1], Instant::now());
    assert!(session.queues[&lanes[0].queue].failed);
    assert!(!session.queues[&lanes[1].queue].failed);
    assert!(session.backend().closed_queues.is_empty());
}

#[test]
fn a_backend_with_a_slow_card_gets_its_own_time_before_a_queue_fails() {
    const SLOW_CARD: Duration = Duration::from_secs(60);
    let mut session = running_session();
    session.backend_mut().work_patience = Some(SLOW_CARD);
    let lane = lane_of(&mut session, FIRST_PROCESS);
    let at = Instant::now();
    assert_eq!(submit_status(&mut session, &lane, 1), SUCCESS);
    session.poll(at + QUEUE_TIMEOUT + PAST_THE_TIMEOUT);
    assert!(!session.queues[&lane.queue].failed);
    session.poll(at + SLOW_CARD + PAST_THE_TIMEOUT);
    assert!(session.queues[&lane.queue].failed);
}

#[test]
fn a_process_whose_queues_keep_failing_comes_off_the_gpu() {
    let mut session = running_session();
    let spare = MOST_FAILED_QUEUES + 1;
    let lanes = lanes_of(&mut session, FIRST_PROCESS, spare);
    hang(&mut session, &lanes[..MOST_FAILED_QUEUES as usize], Instant::now());
    let mut closed = session.backend().closed_queues.clone();
    closed.sort_unstable();
    assert_eq!(closed, lanes.iter().map(|lane| lane.queue).collect::<Vec<_>>());
    assert!(lanes.iter().all(|lane| session.queues[&lane.queue].failed));
    assert!(session.work_allowed());
}

#[test]
fn other_processes_carry_on_when_one_comes_off_the_gpu() {
    let mut session = running_session();
    let failing = lanes_of(&mut session, FIRST_PROCESS, MOST_FAILED_QUEUES);
    let other = lane_of(&mut session, OTHER_PROCESS);
    hang(&mut session, &failing, Instant::now());
    let rung = doorbells(&session);
    assert_eq!(submit_status(&mut session, &other, 1), SUCCESS);
    assert_eq!(doorbells(&session), rung + 1);
    assert!(!session.queues[&other.queue].failed);
}

#[test]
fn a_submission_that_cannot_run_fails_and_still_frees_the_guest() {
    let mut session = running_session();
    let lane = lane_of(&mut session, FIRST_PROCESS);
    hang(&mut session, std::slice::from_ref(&lane), Instant::now());
    let rung = doorbells(&session);
    assert_eq!(submit_status(&mut session, &lane, 2), Status::UNSUCCESSFUL.0);
    assert_eq!(doorbells(&session), rung);
    assert_eq!(session.memory().read_u64(lane.fence).unwrap(), 2);
}

#[test]
fn a_submission_to_a_context_that_cannot_run_fails_too() {
    let mut session = running_session();
    let context = create_context(&mut session, 0);
    let mut request = message(Command::SubmitToContext, 0x198);
    set_u32(&mut request, 0x34, context);
    assert_eq!(status_of(&send(&mut session, &request)), SUCCESS);
    let queue = session.context_queues[&context][0];
    session.queues.get_mut(&queue).unwrap().failed = true;
    assert_eq!(status_of(&send(&mut session, &request)), Status::UNSUCCESSFUL.0);
}

#[test]
fn too_many_processes_faulting_at_once_stop_all_work() {
    let mut session = running_session();
    let now = Instant::now();
    let bystander = lane_of(&mut session, OTHER_PROCESS);
    for process in 0..MOST_FAULTS as u32 {
        assert!(!session.gpu_work_stopped());
        let lanes = lanes_of(&mut session, FIRST_PROCESS + process, 1);
        hang(&mut session, &lanes, now);
    }
    assert!(session.gpu_work_stopped());
    let rung = doorbells(&session);
    assert_eq!(submit_status(&mut session, &bystander, 1), Status::UNSUCCESSFUL.0);
    assert_eq!(doorbells(&session), rung);
    assert_eq!(session.memory().read_u64(bystander.fence).unwrap(), 1);
}

#[test]
fn a_process_counts_once_however_many_queues_it_had() {
    let mut session = running_session();
    let lanes = lanes_of(&mut session, FIRST_PROCESS, 3 * MOST_FAULTS as u32);
    hang(&mut session, &lanes, Instant::now());
    assert!(!session.gpu_work_stopped());
}

#[test]
fn three_faults_with_three_queues_each_stop_only_those_processes() {
    let mut session = running_session();
    let now = Instant::now();
    let bystander = lane_of(&mut session, OTHER_PROCESS);
    for process in 0..3 {
        let lanes = lanes_of(&mut session, FIRST_PROCESS + process, 3);
        hang(&mut session, &lanes, now);
    }
    assert!(!session.gpu_work_stopped());
    assert_eq!(submit_status(&mut session, &bystander, 1), SUCCESS);
}

#[test]
fn faults_spread_over_a_long_run_never_stop_all_work() {
    let mut session = running_session();
    let start = Instant::now();
    for fault in 0..100 {
        let lanes = lanes_of(&mut session, FIRST_PROCESS + fault, 1);
        hang(&mut session, &lanes, start + FAULT_WINDOW * fault);
    }
    assert!(!session.gpu_work_stopped());
}

#[test]
fn a_guest_that_restarts_gets_its_gpu_back() {
    let mut session = running_session();
    let now = Instant::now();
    for process in 0..MOST_FAULTS as u32 {
        let lanes = lanes_of(&mut session, FIRST_PROCESS + process, 1);
        hang(&mut session, &lanes, now);
    }
    assert!(session.gpu_work_stopped());
    session.guest_reset();
    assert!(!session.gpu_work_stopped());
    let lane = lane_of(&mut session, OTHER_PROCESS);
    assert_eq!(submit_status(&mut session, &lane, 1), SUCCESS);
}
