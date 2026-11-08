//! The steps of a guest's boot, as whole messages through the session and the backend, with the
//! pretend RM. This proves the parts join up. It proves nothing about a real card or a real guest.

use guesthouse_backend::{Backend, DriverVersion, GuestMemory, Setup};
use guesthouse_backend_nvidia::NvidiaBackend;
use guesthouse_backend_nvidia::fake_guest::{
    PROCESS, blank_request, fake_backend, message, start_on_fake_card,
};
use guesthouse_backend_nvidia::profile::{BLACKWELL, GUEST_617};
use guesthouse_backend_nvidia::pushbuffer::{increasing, ring_entry};
use guesthouse_backend_nvidia::rm::classes::{
    CHANNEL_GROUP, CONTROL_ENCODE_SLOT_GIVE_BACK, CONTROL_ENCODE_SLOT_TAKE, ENGINE_COPY_0,
};
use guesthouse_backend_nvidia::rm::fake::{Call, FakeRm};
use guesthouse_backend_nvidia::rm::fake_card::{FAKE_CLASSES, FAKE_GPU_ID, FAKE_NAME};
use guesthouse_backend_nvidia::sensors::{Snapshot, SnapshotReader};
use guesthouse_backend_standin::{RandomMessages, SparseMemory};
use guesthouse_core::{Answer, Config, CopyOrder, MemoryLimits, ReplyTicket, Session};
use guesthouse_messages::allocation::CreateAllocationAnswer;
use guesthouse_messages::wire::{set_u32, set_u64, u32_at, u64_at};
use guesthouse_messages::{Command, HEADER_SIZE, Status};
use std::env;
use std::time::{Duration, Instant};

const WINDOW: u64 = 1 << 40;
const WINDOW_END: u64 = WINDOW + (1 << 30);
const TICKET: Option<ReplyTicket> = Some(ReplyTicket { channel: 1, transaction: 9 });
const SUCCESS: u32 = Status::SUCCESS.0;
const DECODE_NODE: u32 = 1;
/// The fake card's first decoder, in the RM's numbers.
const RM_DECODER: u32 = 0x13;
/// The decode and the two encode nodes the fake card has.
const VIDEO_NODES: [u32; 3] = [1, 5, 6];
/// Its security, JPEG and VR nodes.
const NODES_NOT_FOR_VULKAN: [u32; 3] = [4, 7, 8];
const VULKAN_CLIENT: u32 = 4;
const COPY_NODE: u32 = 2;

const DEFAULT_FUZZ_ROUNDS: u64 = 30_000;
const DEFAULT_FUZZ_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
const FUZZ_ROUNDS_PER_SESSION: u64 = 2000;
const FUZZ_ROUNDS_TO_TIME_OUT: u64 = 64;
/// Past every deadline a session has.
const FAR_FUTURE: Duration = Duration::from_secs(3600);

type Guest = Session<NvidiaBackend<FakeRm>, SparseMemory>;

fn guest() -> Guest {
    let backend = fake_backend(FakeRm::new());
    let limits = MemoryLimits::new(backend.facts(), None, 16 << 30);
    let mut config = Config::new(limits);
    config.frame_wait = Duration::ZERO;
    let mut session = Session::new(backend, SparseMemory::new(WINDOW_END), config);
    let mut io_space = message(Command::SetIoSpace, HEADER_SIZE + 16);
    set_u64(&mut io_space, HEADER_SIZE, WINDOW);
    send(&mut session, &io_space);
    session
}

fn send(session: &mut Guest, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    assert_eq!(session.handle_message(message, TICKET, &mut out), Answer::Now);
    out
}

/// A shared allocation of `size` bytes with the driver's own kind of private data. Returns its
/// GPU address and where it is in the window.
fn shared_allocation(session: &mut Guest, size: u32) -> (u64, u64) {
    let layout = &GUEST_617.allocation;
    let mut private = blank_request();
    set_u32(&mut private, layout.size, size);
    let mut create = message(Command::CreateAllocation, 0x40);
    set_u32(&mut create, 0x28, 1);
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&(private.len() as u32).to_le_bytes());
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&private);
    let reply = send(session, &create);
    assert_eq!(u32_at(&reply, 0), Ok(SUCCESS));
    let handle = u32_at(&reply, 0x10).unwrap();

    let mut map = message(Command::MapGpuAddresses, 0x50);
    set_u32(&mut map, 0x38, handle);
    let gpu_address = u64_at(&send(session, &map), 0).unwrap();
    let mut lock = message(Command::LockForCpu, 0x20);
    set_u32(&mut lock, 0x1C, handle);
    (gpu_address, u64_at(&send(session, &lock), 8).unwrap())
}

fn context(session: &mut Guest, node: u32) -> u32 {
    let mut request = message(Command::CreateContext, 0x34);
    set_u32(&mut request, 0x20, node);
    u32_at(&send(session, &request), 0x18).unwrap()
}

/// Returns (queue, its fence).
fn queue(session: &mut Guest, context: u32) -> (u32, u64) {
    let mut request = message(Command::CreateHardwareQueue, 0x44 + 0x80);
    set_u32(&mut request, 0x38, context);
    set_u32(&mut request, 0x40, 0x80);
    let reply = send(session, &request);
    assert_ne!(u32_at(&reply, 0x44 + 0x40), Ok(0), "the guest's driver refuses a token of zero");
    (u32_at(&reply, 0x1C).unwrap(), u64_at(&reply, 0x28).unwrap())
}

fn submission(queue: u32, value: u64, private: &[u8]) -> Vec<u8> {
    let mut request = message(Command::SubmitToHardwareQueue, 0x50);
    set_u32(&mut request, 0x18, queue);
    set_u64(&mut request, 0x20, value);
    request.extend_from_slice(private);
    request
}

fn ring_private(entries: u32, end: u32, ring: u64) -> Vec<u8> {
    let mut private = vec![0; 0x84];
    set_u32(&mut private, 0x14, entries);
    set_u32(&mut private, 0x78, end);
    set_u64(&mut private, 0x7C, ring);
    private
}

#[test]
fn the_guest_is_told_what_the_live_card_and_the_setup_say() {
    let mut session = guest();
    let reply = send(&mut session, &message(Command::GetAdapterIdentity, HEADER_SIZE));
    let name: String = String::from_utf16_lossy(
        &reply[24..24 + 2 * FAKE_NAME.len()]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|u| u16::from_le_bytes(*u))
            .collect::<Vec<_>>(),
    );
    assert_eq!(name, FAKE_NAME);
    let mut query = message(Command::QueryAdapter, HEADER_SIZE + 8);
    set_u32(&mut query, HEADER_SIZE, 49);
    set_u32(&mut query, HEADER_SIZE + 4, 8);
    let reply = send(&mut session, &query);
    assert_eq!(u64_at(&reply, 4), Ok(32 << 48 | 16 << 16 | 1742));
}

#[test]
fn the_record_reaches_the_guest_with_the_discovery_word_set() {
    let mut session = guest();
    let size = GUEST_617.record.size;
    let mut query = message(Command::QueryAdapter, HEADER_SIZE + 8);
    set_u32(&mut query, HEADER_SIZE + 4, size as u32);
    let reply = send(&mut session, &query);
    assert_eq!(reply.len(), 4 + size);
    assert_eq!(&reply[4..8], b"ADVN");
    assert_eq!(u32_at(&reply, 4 + GUEST_617.record.gpu_discovery), Ok(1));
}

#[test]
fn the_records_class_list_starts_with_its_length() {
    let mut session = guest();
    let size = GUEST_617.record.size;
    let mut query = message(Command::QueryAdapter, HEADER_SIZE + 8);
    set_u32(&mut query, HEADER_SIZE + 4, size as u32);
    let reply = send(&mut session, &query);
    let at = 4 + GUEST_617.record.classes;
    assert_eq!(u32_at(&reply, at), Ok(FAKE_CLASSES.len() as u32));
    assert_eq!(u32_at(&reply, at + 4), Ok(FAKE_CLASSES[0]));
}

#[test]
fn the_record_carries_the_live_cards_id() {
    let mut session = guest();
    let size = GUEST_617.record.size;
    let mut query = message(Command::QueryAdapter, HEADER_SIZE + 8);
    set_u32(&mut query, HEADER_SIZE + 4, size as u32);
    let reply = send(&mut session, &query);
    assert_eq!(u32_at(&reply, 4 + GUEST_617.record.gpu_id), Ok(FAKE_GPU_ID));
}

#[test]
fn a_ring_queue_runs_on_the_guests_own_ring() {
    let mut session = guest();
    let (ring, ring_memory) = shared_allocation(&mut session, 0x10000);
    let context = context(&mut session, 0);
    let (queue, _) = queue(&mut session, context);
    let reply = send(&mut session, &submission(queue, 1, &ring_private(1, 2, ring)));
    assert_eq!(reply, SUCCESS.to_le_bytes());

    let prelude = session.memory().read_u64(ring_memory).unwrap();
    assert_ne!(prelude, 0, "the entry before the first holds the prelude");
    assert_eq!(session.backend().device().rm.doorbells().len(), 1);
    let groups =
        session.backend().device().rm.allocated().filter(|(class, _)| *class == CHANNEL_GROUP).count();
    assert_eq!(groups, 1);
    send(&mut session, &submission(queue, 2, &ring_private(1, 3, ring)));
    assert_eq!(session.backend().device().rm.doorbells().len(), 2);
    let groups =
        session.backend().device().rm.allocated().filter(|(class, _)| *class == CHANNEL_GROUP).count();
    assert_eq!(groups, 1, "the channel is made once");

    send(&mut session, &submission(queue, 3, &ring_private(1, 6, ring)));
    assert_eq!(session.memory().read_u64(ring_memory + 3 * 8), Ok(0), "the gap's other entries stay empty");
    assert_ne!(session.memory().read_u64(ring_memory + 4 * 8), Ok(0), "the L2 invalidate goes just ahead");
}

#[test]
fn a_ring_with_no_room_for_the_prelude_falls_back_to_finishing_the_fence() {
    let mut session = guest();
    let (ring, _) = shared_allocation(&mut session, 0x10000);
    let context = context(&mut session, 0);
    let (queue, fence) = queue(&mut session, context);
    send(&mut session, &submission(queue, 5, &ring_private(1, 1, ring)));
    assert!(session.backend().device().rm.doorbells().is_empty());
    assert_eq!(session.memory().read_u64(fence).unwrap(), 5);
}

#[test]
fn work_that_did_not_run_still_writes_its_host_semaphores() {
    let mut session = guest();
    let (ring, ring_memory) = shared_allocation(&mut session, 0x10000);
    let (commands, command_memory) = shared_allocation(&mut session, 0x1000);
    let (target, target_memory) = shared_allocation(&mut session, 0x1000);
    let context = context(&mut session, 0);
    let (queue, _) = queue(&mut session, context);
    let words = [increasing(0, 0x5C, 5), target as u32, (target >> 32) as u32, 0x77, 0, 1];
    for (index, word) in words.iter().enumerate() {
        session.memory_mut().write_u32(command_memory + index as u64 * 4, *word).unwrap();
    }
    session.memory_mut().write_u64(ring_memory, ring_entry(commands, words.len() as u32)).unwrap();
    send(&mut session, &submission(queue, 1, &ring_private(1, 1, ring)));
    assert_eq!(session.memory().read_u64(target_memory).unwrap(), 0x77);
}

#[test]
fn a_list_queue_on_a_copy_node_uses_a_copy_channel_and_fixes_its_semaphores() {
    let mut session = guest();
    let (commands, command_memory) = shared_allocation(&mut session, 0x1000);
    session.memory_mut().write_u32(command_memory, 0x2001_80C1).unwrap();
    session.memory_mut().write_u32(command_memory + 4, 1).unwrap();
    let context = context(&mut session, COPY_NODE);
    let (queue, _) = queue(&mut session, context);
    let mut private = vec![0; 0x84];
    set_u32(&mut private, 0x14, 1);
    set_u32(&mut private, 0x0C, 0x84);
    private.extend_from_slice(&ring_entry(commands, 2).to_le_bytes());
    send(&mut session, &submission(queue, 1, &private));

    let copy_group = session.backend().device().rm.allocated().find(|(class, _)| *class == CHANNEL_GROUP);
    assert_eq!(u32_at(copy_group.unwrap().1, 0x0C), Ok(ENGINE_COPY_0));
    assert_eq!(session.memory().read_u64(command_memory).unwrap() as u32, 0x2001_80C0);
    assert_eq!(session.memory().read_u64(command_memory).unwrap() >> 32, 0xC);
    assert_eq!(session.backend().device().rm.doorbells().len(), 1);
}

/// A ring position, with what looks like a good list behind it where the sender's stack left it.
fn position_private(entries: u32, end: u32, stale_target: u64) -> Vec<u8> {
    let mut private = vec![0; 0x218];
    set_u32(&mut private, 0x0C, 0x118);
    set_u32(&mut private, 0x14, entries);
    set_u32(&mut private, 0x78, end);
    private[0x110] = 0x10;
    private[0x118..0x120].copy_from_slice(&ring_entry(stale_target, 4).to_le_bytes());
    private
}

/// An allocation in the guest's own pages, mapped at `address`. Returns where its bytes are kept.
fn lent_allocation(session: &mut Guest, size: u32, address: u64) -> u64 {
    lent_allocation_of(PROCESS, session, size, address)
}

fn lent_allocation_of(process: u32, session: &mut Guest, size: u32, address: u64) -> u64 {
    let layout = &GUEST_617.allocation;
    let mut private = blank_request();
    set_u32(&mut private, layout.size, size);
    let mut create = message(Command::CreateAllocation, 0x40);
    set_u32(&mut create, 0x28, 1);
    set_u32(&mut create, 0x2C, 0x20);
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&(private.len() as u32).to_le_bytes());
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&private);
    let handle = u32_at(&send(session, &create), 0x10).unwrap();

    let pages = u64::from(size).div_ceil(0x1000);
    let mut lend = message(Command::SetGuestPages, 0x28);
    set_u32(&mut lend, 0x1C, handle);
    set_u32(&mut lend, 0x20, pages as u32);
    (0..pages).for_each(|page| lend.extend_from_slice(&(0x100 + page).to_le_bytes()));
    send(session, &lend);

    let mut map = message(Command::MapGpuAddresses, 0x50);
    set_u32(&mut map, 8, process);
    set_u64(&mut map, 0x20, address);
    set_u32(&mut map, 0x38, handle);
    assert_eq!(u64_at(&send(session, &map), 0), Ok(address));
    session.memory().views.last().unwrap().host_address
}

/// A queue whose private data names a ring.
fn queue_on_ring(session: &mut Guest, context: u32, ring: u64) -> (u32, u64) {
    queue_on_ring_of(PROCESS, session, context, ring)
}

fn queue_on_ring_of(process: u32, session: &mut Guest, context: u32, ring: u64) -> (u32, u64) {
    let mut request = message(Command::CreateHardwareQueue, 0x44 + 0xB4);
    set_u32(&mut request, 8, process);
    set_u32(&mut request, 0x38, context);
    set_u32(&mut request, 0x40, 0xB4);
    set_u64(&mut request, 0x44 + 0x18, ring);
    set_u32(&mut request, 0x44 + 0x20, 0x2000);
    let reply = send(session, &request);
    (u32_at(&reply, 0x1C).unwrap(), u64_at(&reply, 0x28).unwrap())
}

const RUNTIME_RING: u64 = 0x3_0040_0000;
const RUNTIME_COMMANDS: u64 = 0x3_0080_0000;

fn channels_made(session: &Guest) -> usize {
    session.backend().device().rm.allocated().filter(|(class, _)| *class == CHANNEL_GROUP).count()
}

#[test]
fn stale_bytes_after_a_ring_position_never_reach_a_channel() {
    for node in [0, COPY_NODE] {
        let mut session = guest();
        let (commands, _) = shared_allocation(&mut session, 0x1000);
        let context = context(&mut session, node);
        let (queue, fence) = queue(&mut session, context);
        let reply = send(&mut session, &submission(queue, 1, &position_private(1, 1, commands)));
        assert_eq!(reply, Status::UNSUCCESSFUL.0.to_le_bytes(), "the program has to hear of it");
        assert!(session.backend().device().rm.doorbells().is_empty());
        assert_eq!(channels_made(&session), 0);
        assert_eq!(session.memory().read_u64(fence), Ok(1), "nothing is left waiting");
    }
}

#[test]
fn a_ring_position_runs_what_the_guests_ring_holds_and_not_what_the_stack_left() {
    let mut session = guest();
    let ring = lent_allocation(&mut session, 0x10000, RUNTIME_RING);
    lent_allocation(&mut session, 0x1000, RUNTIME_COMMANDS);
    let context = context(&mut session, 0);
    let (queue, _) = queue_on_ring(&mut session, context, RUNTIME_RING);
    session.memory_mut().write_u64(ring, ring_entry(RUNTIME_COMMANDS, 4)).unwrap();

    let reply = send(&mut session, &submission(queue, 1, &position_private(1, 1, 0xBAD_0000)));
    assert_eq!(reply, SUCCESS.to_le_bytes());
    assert_eq!(session.backend().device().rm.doorbells().len(), 1);
    assert_eq!(channels_made(&session), 1);

    let list = session.backend().ring_of_list_queue(queue).unwrap();
    assert!(list.iter().any(|entry| entry.address == RUNTIME_COMMANDS), "{list:x?}");
    assert!(!list.iter().any(|entry| entry.address == 0xBAD_0000), "{list:x?}");
}

#[test]
fn a_ring_position_with_a_ring_the_host_cannot_read_stops_the_queue() {
    let mut session = guest();
    let context = context(&mut session, 0);
    let (queue, fence) = queue_on_ring(&mut session, context, RUNTIME_RING);
    send(&mut session, &submission(queue, 1, &position_private(1, 1, 0xBAD_0000)));
    assert!(session.backend().device().rm.doorbells().is_empty());
    assert_eq!(channels_made(&session), 0);
    assert_eq!(session.memory().read_u64(fence), Ok(1), "nothing is left waiting");
}

#[test]
fn a_ring_position_past_what_was_written_stops_the_queue() {
    let mut session = guest();
    lent_allocation(&mut session, 0x10000, RUNTIME_RING);
    let context = context(&mut session, 0);
    let (queue, _) = queue_on_ring(&mut session, context, RUNTIME_RING);
    send(&mut session, &submission(queue, 1, &position_private(1, 1, 0xBAD_0000)));
    assert!(session.backend().device().rm.doorbells().is_empty());
}

#[test]
fn a_copy_queue_on_a_ring_position_gets_its_semaphores_fixed_in_the_guests_pages() {
    let mut session = guest();
    let ring = lent_allocation(&mut session, 0x10000, RUNTIME_RING);
    let commands = lent_allocation(&mut session, 0x1000, RUNTIME_COMMANDS);
    session.memory_mut().write_u32(commands, 0x2001_80C1).unwrap();
    session.memory_mut().write_u32(commands + 4, 1).unwrap();
    session.memory_mut().write_u64(ring, ring_entry(RUNTIME_COMMANDS, 2)).unwrap();
    let context = context(&mut session, COPY_NODE);
    let (queue, _) = queue_on_ring(&mut session, context, RUNTIME_RING);
    send(&mut session, &submission(queue, 1, &position_private(1, 1, 0xBAD_0000)));
    assert_eq!(session.memory().read_u64(commands).unwrap() as u32, 0x2001_80C0);
    assert_eq!(session.backend().device().rm.doorbells().len(), 1);
}

#[test]
fn a_copy_queue_whose_commands_cannot_be_read_never_starts() {
    let mut session = guest();
    let ring = lent_allocation(&mut session, 0x10000, RUNTIME_RING);
    session.memory_mut().write_u64(ring, ring_entry(RUNTIME_COMMANDS, 2)).unwrap();
    let context = context(&mut session, COPY_NODE);
    let (queue, _) = queue_on_ring(&mut session, context, RUNTIME_RING);
    send(&mut session, &submission(queue, 1, &position_private(1, 1, 0xBAD_0000)));
    assert!(session.backend().device().rm.doorbells().is_empty());
    assert_eq!(channels_made(&session), 0);
}

#[test]
fn two_processes_on_the_same_ring_address_each_run_their_own_entries() {
    const OTHER_PROCESS: u32 = 0x4000_3000;
    let mut session = guest();
    let mine = lent_allocation(&mut session, 0x10000, RUNTIME_RING);
    let theirs = lent_allocation_of(OTHER_PROCESS, &mut session, 0x10000, RUNTIME_RING);
    session.memory_mut().write_u64(mine, ring_entry(0x111_0000, 4)).unwrap();
    session.memory_mut().write_u64(theirs, ring_entry(0x222_0000, 4)).unwrap();
    let context = context(&mut session, 0);
    let (my_queue, _) = queue_on_ring(&mut session, context, RUNTIME_RING);
    let (their_queue, _) = queue_on_ring_of(OTHER_PROCESS, &mut session, context, RUNTIME_RING);
    send(&mut session, &submission(my_queue, 1, &position_private(1, 1, 0xBAD_0000)));
    send(&mut session, &submission(their_queue, 1, &position_private(1, 1, 0xBAD_0000)));

    let ran = |queue| session.backend().ring_of_list_queue(queue).unwrap();
    assert!(ran(my_queue).iter().any(|entry| entry.address == 0x111_0000));
    assert!(ran(their_queue).iter().any(|entry| entry.address == 0x222_0000));
    assert!(!ran(my_queue).iter().any(|entry| entry.address == 0x222_0000));
}

#[test]
fn a_flagged_submission_with_a_ring_address_still_runs_on_that_ring() {
    let mut session = guest();
    let (ring, _) = shared_allocation(&mut session, 0x10000);
    let context = context(&mut session, 0);
    let (queue, _) = queue(&mut session, context);
    let mut private = ring_private(1, 2, ring);
    private.resize(0x118, 0);
    private[0x110] = 0x10;
    send(&mut session, &submission(queue, 1, &private));
    assert_eq!(session.backend().device().rm.doorbells().len(), 1);
}

/// A submission to a context, with one ring entry in its private data as the guest sends it.
fn context_submission(context: u32) -> Vec<u8> {
    const SUBMISSION_CONTEXT: usize = 0x34;
    const SUBMISSION_PRIVATE_SIZE: usize = 0x140;
    const SUBMISSION_PRIVATE: usize = 0x198;
    const LIST_OFFSET: usize = 0x0C;
    const LIST_ENTRIES: usize = 0x18;
    const LIST_AT: usize = 0x84;
    let mut private = vec![0; LIST_AT];
    set_u32(&mut private, LIST_OFFSET, LIST_AT as u32);
    set_u32(&mut private, LIST_ENTRIES, 1);
    private.extend_from_slice(&ring_entry(0x4000_0000, 4).to_le_bytes());
    let mut request = message(Command::SubmitToContext, SUBMISSION_PRIVATE);
    set_u32(&mut request, SUBMISSION_CONTEXT, context);
    set_u32(&mut request, SUBMISSION_PRIVATE_SIZE, private.len() as u32);
    request.extend_from_slice(&private);
    request
}

fn group_engines(session: &Guest) -> Vec<u32> {
    let made = session.backend().device().rm.allocated();
    made.filter(|(class, _)| *class == CHANNEL_GROUP)
        .map(|(_, params)| u32_at(params, 0x0C).unwrap())
        .collect()
}

#[test]
fn a_video_context_runs_on_its_own_engine() {
    let mut session = guest();
    let context = context(&mut session, DECODE_NODE);
    assert_ne!(context, 0);
    send(&mut session, &context_submission(context));
    assert!(group_engines(&session).contains(&RM_DECODER));
    let mut made = session.backend().device().rm.allocated();
    let bound = made.find(|(class, _)| *class == BLACKWELL.video_decode_class).unwrap();
    assert_eq!(bound.1, [12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn a_vulkan_context_on_a_video_node_gets_a_copy_channel() {
    let mut session = guest();
    let context = vulkan_context(&mut session, DECODE_NODE);
    send(&mut session, &context_submission(context));
    assert!(group_engines(&session).contains(&ENGINE_COPY_0));
    assert!(!group_engines(&session).contains(&RM_DECODER));
}

#[test]
fn a_video_context_says_it_can_take_work() {
    const PRIVATE: usize = 0x34;
    const READY: usize = 0xD7C;
    const CONTEXT_PRIVATE_SIZE: usize = 0x30;
    let mut request = message(Command::CreateContext, PRIVATE + READY + 4);
    set_u32(&mut request, 0x20, DECODE_NODE);
    set_u32(&mut request, CONTEXT_PRIVATE_SIZE, READY as u32 + 4);
    let reply = send(&mut guest(), &request);
    assert_eq!(u32_at(&reply, PRIVATE + READY), Ok(1));
}

fn vulkan_context(session: &mut Guest, node: u32) -> u32 {
    let mut request = message(Command::CreateContext, 0x34);
    set_u32(&mut request, 0x20, node);
    set_u32(&mut request, 0x2C, VULKAN_CLIENT);
    u32_at(&send(session, &request), 0x18).unwrap()
}

#[test]
fn a_vulkan_context_gets_past_the_guard_on_every_video_node() {
    let mut session = guest();
    for node in VIDEO_NODES {
        assert_ne!(vulkan_context(&mut session, node), 0, "node {node}");
    }
}

#[test]
fn vulkan_has_no_way_past_the_guard_on_other_engines() {
    let mut session = guest();
    for node in NODES_NOT_FOR_VULKAN {
        assert_eq!(vulkan_context(&mut session, node), 0, "node {node}");
    }
}

#[test]
fn a_frame_copy_holds_its_reply_until_the_copy_engine_says_it_finished() {
    let mut session = guest();
    start_held_frame_copy(&mut session);
    assert_eq!(session.backend().device().rm.doorbells().len(), 1, "the copy has started");
    let flush = send(&mut session, &message(Command::FlushDevice, HEADER_SIZE));
    assert_eq!(flush, SUCCESS.to_le_bytes(), "other messages are not held up");
    let _ = session.poll(Instant::now());
    assert_eq!(session.take_replies().count(), 0);
    // Past its time, the channel is replaced and the copy runs again.
    let _ = session.poll(Instant::now() + Duration::from_secs(3));
    assert_eq!(session.backend().device().rm.doorbells().len(), 2);
    let _ = session.poll(Instant::now() + Duration::from_secs(9));
    let replies: Vec<_> = session.take_replies().collect();
    assert_eq!(replies[0].bytes, Status::UNSUCCESSFUL.0.to_le_bytes());
}

#[test]
fn a_ring_with_no_gap_gets_its_invalidate_from_the_copy_channel() {
    let mut session = guest();
    let (ring, ring_memory) = shared_allocation(&mut session, 0x10000);
    let context = context(&mut session, 0);
    let (queue, _) = queue(&mut session, context);
    send(&mut session, &submission(queue, 1, &ring_private(1, 2, ring)));
    // The copy channel comes with the first frame.
    start_held_frame_copy(&mut session);
    session.backend().pretend_copy_finished();
    let _ = session.poll(Instant::now());
    let before = session.backend().device().rm.doorbells().len();

    // Nothing answers the fake's semaphore either, so this takes the copy timeout.
    send(&mut session, &submission(queue, 2, &ring_private(1, 3, ring)));
    assert_eq!(session.memory().read_u64(ring_memory + 8), Ok(0), "the guest's own entry isn't touched");
    assert_eq!(session.backend().device().rm.doorbells().len(), before + 2, "the invalidate, then the queue");
}

/// Makes a tiled surface of `pitch` by `height`, filling `size` bytes, and a linear target for it.
/// Returns the surface and the target.
fn tiled_and_target(session: &mut Guest, pitch: u32, height: u32, block: u32, size: u32) -> (u32, u32) {
    let (source, target, _) = tiled_target_and_size(session, pitch, height, block, size);
    (source, target)
}

/// As [`tiled_and_target`], and the size the tiled surface really got.
fn tiled_target_and_size(
    session: &mut Guest,
    pitch: u32,
    height: u32,
    block: u32,
    size: u32,
) -> (u32, u32, u64) {
    let layout = &GUEST_617.allocation;
    let mut source = blank_request();
    set_u32(&mut source, layout.size, size);
    set_u32(&mut source, layout.width, pitch / 4);
    set_u32(&mut source, layout.height, height);
    set_u32(&mut source, layout.pitch, pitch);
    set_u32(&mut source, layout.tiling, block);
    set_u32(&mut source, layout.columns, pitch.div_ceil(64));
    let mut target = blank_request();
    set_u32(&mut target, layout.size, pitch * height);
    set_u32(&mut target, layout.width, pitch / 4);
    set_u32(&mut target, layout.height, height);
    set_u32(&mut target, layout.pitch, pitch);
    let mut create = message(Command::CreateAllocation, 0x40);
    set_u32(&mut create, 0x28, 2);
    for private in [&source, &target] {
        create.extend_from_slice(&[0; 4]);
        create.extend_from_slice(&(private.len() as u32).to_le_bytes());
        create.extend_from_slice(&[0; 4]);
    }
    create.extend_from_slice(&source);
    create.extend_from_slice(&target);
    let reply = send(session, &create);
    let given = CreateAllocationAnswer::size_of_entry(&reply, 0).unwrap();
    (u32_at(&reply, 0x10).unwrap(), u32_at(&reply, 0x58).unwrap(), given)
}

fn copy_of(source: u32, target: u32, width: u32, height: u32) -> Vec<u8> {
    let mut copy = message(Command::CopyToStaging, 0x50);
    set_u32(&mut copy, 0x20, source);
    set_u32(&mut copy, 0x24, target);
    set_u32(&mut copy, 0x30, width);
    set_u32(&mut copy, 0x34, height);
    copy
}

#[test]
fn a_tiled_screen_that_fills_its_blocks_starts_a_copy() {
    let mut session = guest();
    // 1080 rows take nine blocks of 128.
    let (source, target) = tiled_and_target(&mut session, 7680, 1080, 4, 7680 * 1152);
    session.set_copy_order(guesthouse_core::CopyOrder::Held);
    let mut out = Vec::new();
    let copy = copy_of(source, target, 1920, 1080);
    assert_eq!(session.handle_message(&copy, TICKET, &mut out), Answer::Held);
    let words = session.backend().last_copy_words();
    assert_eq!(words[16..19], [4 << 4 | 1 << 12, 7680, 1080], "block, pitch and height, as stated");
}

/// The end of a tiled surface, walked to from `clc7b5.h` and not from the code under test: the block
/// holding its last byte, and the end of that block. GOBs are 64 bytes by 8 rows, a block is one GOB
/// wide and `1 << block` high, and blocks run along a row of blocks first.
fn end_of_tiled_surface(pitch: u64, height: u64, block: u32) -> u64 {
    let block_rows = 8 << block;
    let blocks_across = (pitch - 1) / 64 + 1;
    let last_block = (height - 1) / block_rows * blocks_across + blocks_across - 1;
    (last_block + 1) * 64 * block_rows
}

#[test]
fn a_tiled_copy_goes_ahead_only_if_the_headers_sum_fits_its_memory() {
    const SHORT_BY: u32 = 0x1_0000;
    for pitch in [64, 100, 5440, 7680, 15360, 16384] {
        for height in [1, 8, 9, 400, 768, 1080, 2160] {
            for block in 0..=5 {
                let end = end_of_tiled_surface(u64::from(pitch), u64::from(height), block) as u32;
                for stated in [end, end.saturating_sub(SHORT_BY).max(0x1000)] {
                    let mut session = guest();
                    let (source, target, given) =
                        tiled_target_and_size(&mut session, pitch, height, block, stated);
                    session.set_copy_order(CopyOrder::Held);
                    let mut out = Vec::new();
                    let copy = copy_of(source, target, pitch / 4, height);
                    let answer = session.handle_message(&copy, TICKET, &mut out);
                    let wanted = if u64::from(end) <= given { Answer::Held } else { Answer::Now };
                    assert_eq!(answer, wanted, "{pitch} by {height}, block {block}, in {given:#x} bytes");
                }
            }
        }
    }
}

#[test]
fn a_tiled_screen_short_of_its_last_block_is_refused() {
    let mut session = guest();
    let (source, target) = tiled_and_target(&mut session, 7680, 1080, 4, 7680 * 1080);
    assert_eq!(
        send(&mut session, &copy_of(source, target, 1920, 1080)),
        Status::UNSUCCESSFUL.0.to_le_bytes()
    );
}

#[test]
fn a_block_height_past_the_engines_is_refused() {
    let mut session = guest();
    let (source, target) = tiled_and_target(&mut session, 7680, 1080, 6, 7680 * 2048);
    assert_eq!(
        send(&mut session, &copy_of(source, target, 1920, 1080)),
        Status::UNSUCCESSFUL.0.to_le_bytes()
    );
}

/// Starts a copy to a staging surface that nothing will finish.
fn start_held_frame_copy(session: &mut Guest) {
    let layout = &GUEST_617.allocation;
    let mut private = blank_request();
    set_u32(&mut private, layout.size, 0x10000);
    set_u32(&mut private, layout.width, 64);
    set_u32(&mut private, layout.height, 64);
    set_u32(&mut private, layout.pitch, 256);
    let mut create = message(Command::CreateAllocation, 0x40);
    set_u32(&mut create, 0x28, 2);
    for _ in 0..2 {
        create.extend_from_slice(&[0; 4]);
        create.extend_from_slice(&(private.len() as u32).to_le_bytes());
        create.extend_from_slice(&[0; 4]);
    }
    create.extend_from_slice(&private);
    create.extend_from_slice(&private);
    let reply = send(session, &create);
    let (source, target) = (u32_at(&reply, 0x10).unwrap(), u32_at(&reply, 0x58).unwrap());

    session.set_copy_order(guesthouse_core::CopyOrder::Held);
    let mut copy = message(Command::CopyToStaging, 0x50);
    set_u32(&mut copy, 0x20, source);
    set_u32(&mut copy, 0x24, target);
    set_u32(&mut copy, 0x30, 64);
    set_u32(&mut copy, 0x34, 64);
    let mut out = Vec::new();
    assert_eq!(session.handle_message(&copy, TICKET, &mut out), Answer::Held);
}

/// Makes an allocation and maps it. Returns its resource and its handle.
fn video_allocation(session: &mut Guest) -> (u32, u32) {
    let layout = &GUEST_617.allocation;
    let mut private = blank_request();
    set_u32(&mut private, layout.size, 0x1_0000);
    let mut create = message(Command::CreateAllocation, 0x40);
    set_u32(&mut create, 0x28, 1);
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&(private.len() as u32).to_le_bytes());
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&private);
    let reply = send(session, &create);
    let (resource, handle) = (u32_at(&reply, 4).unwrap(), u32_at(&reply, 0x10).unwrap());
    let mut map = message(Command::MapGpuAddresses, 0x50);
    set_u32(&mut map, 0x38, handle);
    send(session, &map);
    (resource, handle)
}

fn destroy_by_name(session: &mut Guest, handle: u32) {
    let mut destroy = message(Command::DestroyAllocation, 0x28);
    set_u32(&mut destroy, 0x20, 1);
    destroy.extend_from_slice(&handle.to_le_bytes());
    send(session, &destroy);
}

fn frees_and_unmaps(session: &Guest) -> (usize, usize) {
    let calls = &session.backend().device().rm.calls;
    let frees = calls.iter().filter(|call| matches!(call, Call::Free { .. })).count();
    let unmaps = calls.iter().filter(|call| matches!(call, Call::UnmapDma { .. })).count();
    (frees, unmaps)
}

#[test]
fn memory_named_by_a_process_goes_back_to_the_card() {
    let mut session = guest();
    let (_, handle) = video_allocation(&mut session);
    let (frees, unmaps) = frees_and_unmaps(&session);
    destroy_by_name(&mut session, handle);
    assert_eq!(frees_and_unmaps(&session), (frees + 1, unmaps + 1));
}

#[test]
fn a_frame_another_process_has_open_stays_on_the_card_until_it_lets_go() {
    let mut session = guest();
    let (resource, handle) = video_allocation(&mut session);
    let mut open = message(Command::OpenResource, 0x28);
    set_u32(&mut open, 0x20, resource);
    set_u32(&mut open, 0x24, 1);
    send(&mut session, &open);
    let before = frees_and_unmaps(&session);
    let mut destroy = message(Command::DestroyAllocation, 0x28);
    set_u32(&mut destroy, 0x1C, resource);
    send(&mut session, &destroy);
    destroy_by_name(&mut session, handle);
    assert_eq!(frees_and_unmaps(&session), before, "the maker left, and the other process hasn't");
    send(&mut session, &destroy);
    assert_eq!(frees_and_unmaps(&session), (before.0 + 1, before.1 + 1));
}

#[test]
fn the_host_hands_out_nothing_in_the_runtimes_range() {
    let mut session = guest();
    let (first, _) = shared_allocation(&mut session, 0x1000_0000);
    let (second, _) = shared_allocation(&mut session, 0x1000_0000);
    for address in [first, second] {
        assert!(!(0x2000_0000..0x1_0000_0000).contains(&address), "{address:#x}");
    }
}

/// Fixed readings, standing in for the host's sampler.
struct FixedReadings(Option<Snapshot>);

impl SnapshotReader for FixedReadings {
    fn latest(&self) -> Option<Snapshot> {
        self.0
    }
}

fn adapter_speed_query() -> Vec<u8> {
    let mut query = message(Command::QueryAdapter, HEADER_SIZE + 8 + 64);
    set_u32(&mut query, HEADER_SIZE, 62);
    set_u32(&mut query, HEADER_SIZE + 4, 64);
    query
}

#[test]
fn performance_queries_use_live_readings_when_there_are_some() {
    let mut session = guest();
    let snapshot = Snapshot { temperature_celsius: Some(40), ..Snapshot::default() };
    session.backend_mut().set_sensors(Box::new(FixedReadings(Some(snapshot))));
    let reply = send(&mut session, &adapter_speed_query());
    assert_eq!(u32_at(&reply, 4 + 0x38), Ok(400));
}

#[test]
fn performance_queries_keep_working_with_no_readings() {
    let mut plain = guest();
    let before = send(&mut plain, &adapter_speed_query());
    let mut session = guest();
    session.backend_mut().set_sensors(Box::new(FixedReadings(None)));
    assert_eq!(send(&mut session, &adapter_speed_query()), before);
    assert_eq!(u32_at(&before, 0), Ok(SUCCESS));
}

const ENCODE_NODES: [u32; 2] = [5, 6];
const ENCODE_ASKS: [u32; 3] = [0x0100_0128, 0x0100_0119, 0x0100_011A];
const ENCODE_ANSWER: usize = 0x30;
/// What the host's driver says when the card has no slot left, as measured.
const NO_SLOT: u32 = 0x69;

/// A guest on a card whose driver has slots.
fn encoding_guest() -> Guest {
    let mut session = guest();
    let rm = &mut session.backend_mut().device_mut().rm;
    rm.answer(CONTROL_ENCODE_SLOT_TAKE, |_| 0);
    rm.answer(CONTROL_ENCODE_SLOT_GIVE_BACK, |_| 0);
    session
}

fn ask_to_encode(session: &mut Guest, function: u32) -> u8 {
    let mut escape = message(Command::DriverEscape, HEADER_SIZE + 24);
    set_u32(&mut escape, HEADER_SIZE + 16, 0x40);
    let mut data = [0; 0x40];
    data[0x10..0x14].copy_from_slice(&function.to_le_bytes());
    escape.extend_from_slice(&data);
    send(session, &escape)[ENCODE_ANSWER]
}

fn slots_given_back(session: &Guest) -> usize {
    let calls = session.backend().device().rm.calls.iter();
    calls.filter(|call| matches!(call, Call::Control { command: CONTROL_ENCODE_SLOT_GIVE_BACK, .. })).count()
}

#[test]
fn every_way_of_asking_to_encode_gets_the_drivers_answer() {
    for function in ENCODE_ASKS {
        let mut session = encoding_guest();
        assert_eq!(ask_to_encode(&mut session, function), 1, "{function:#x}");
        session.backend_mut().device_mut().rm.answer(CONTROL_ENCODE_SLOT_TAKE, |_| NO_SLOT);
        assert_eq!(ask_to_encode(&mut session, function), 0, "{function:#x}");
    }
}

#[test]
fn a_session_ending_gives_its_slot_back() {
    let mut session = encoding_guest();
    ask_to_encode(&mut session, ENCODE_ASKS[0]);
    let contexts = ENCODE_NODES.map(|node| context(&mut session, node));
    for context in contexts {
        let mut request = message(Command::DestroyContext, HEADER_SIZE + 8);
        set_u32(&mut request, HEADER_SIZE, context);
        send(&mut session, &request);
    }
    assert_eq!(slots_given_back(&session), 1);
}

#[test]
fn a_process_ending_gives_its_slots_back() {
    let mut session = encoding_guest();
    ask_to_encode(&mut session, ENCODE_ASKS[0]);
    ask_to_encode(&mut session, ENCODE_ASKS[0]);
    for node in ENCODE_NODES {
        context(&mut session, node);
    }
    send(&mut session, &message(Command::DestroyProcess, HEADER_SIZE));
    assert_eq!(slots_given_back(&session), 2);
}

#[test]
fn decoding_takes_no_slot() {
    let mut session = guest();
    context(&mut session, DECODE_NODE);
    let calls = session.backend().device().rm.calls.iter();
    assert!(
        !calls
            .into_iter()
            .any(|call| matches!(call, Call::Control { command: CONTROL_ENCODE_SLOT_TAKE, .. }))
    );
}

#[test]
fn escapes_reach_the_vendor_side() {
    let mut session = guest();
    let mut escape = message(Command::DriverEscape, HEADER_SIZE + 24);
    set_u32(&mut escape, HEADER_SIZE + 16, 0x40);
    escape.extend_from_slice(&[0; 0x40]);
    let reply = send(&mut session, &escape);
    assert_eq!(reply.len(), 0x40);
    assert_eq!(u32_at(&reply, 0x14), Ok(1));
}

#[test]
fn the_memory_escape_reports_the_card_less_what_the_guest_has_taken() {
    const FUNCTION_VIDEO_MEMORY: u32 = 0x0100_0012;
    const SIZE: usize = 0x54;
    const FUNCTION: usize = 0x10;
    const DEDICATED: usize = 0x30;
    const AVAILABLE: usize = 0x38;
    let mut session = guest();
    let mut escape = message(Command::DriverEscape, HEADER_SIZE + 24);
    set_u32(&mut escape, HEADER_SIZE + 16, SIZE as u32);
    let data = escape.len();
    escape.resize(data + SIZE, 0);
    set_u32(&mut escape, data + FUNCTION, FUNCTION_VIDEO_MEMORY);
    let reply = send(&mut session, &escape);
    let dedicated = u32_at(&reply, DEDICATED).unwrap();
    assert_ne!(dedicated, 0);
    assert_eq!(u32_at(&reply, AVAILABLE), Ok(dedicated));
}

#[test]
fn a_card_that_is_not_the_first_is_refused_in_plain_words() {
    let Err(refusal) = start_on_fake_card(FakeRm::new(), 0x9999) else { panic!("it should refuse") };
    assert!(refusal.0.contains("Only the first NVIDIA GPU"), "{}", refusal.0);
}

#[test]
fn nothing_in_the_backend_can_be_opened_with_a_fake() {
    let setup = Setup {
        guest_driver_version: DriverVersion::parse("32.0.16.1742").unwrap(),
        guest_branch: "617".into(),
        driver_folder: "/nonexistent".into(),
        card: None,
        card_address: None,
    };
    let survey = guesthouse_backend_nvidia::startup::survey_versions("615.78.08", &setup);
    assert!(NvidiaBackend::<FakeRm>::open(&setup, &survey).is_err());
}

/// A guest with a queue and memory in place, and messages that use them, to change at random.
fn guest_with_templates() -> (Guest, Vec<Vec<u8>>) {
    let mut session = guest();
    let (ring, _) = shared_allocation(&mut session, 0x10000);
    let (list, _) = shared_allocation(&mut session, 0x10000);
    let graphics = context(&mut session, 0);
    let copy = context(&mut session, 1);
    let (graphics_queue, _) = queue(&mut session, graphics);
    let (copy_queue, _) = queue(&mut session, copy);

    let layout = &GUEST_617.allocation;
    let mut image = blank_request();
    set_u32(&mut image, layout.size, 0x10000);
    set_u32(&mut image, layout.columns, 4);
    set_u32(&mut image, layout.rows, 4);
    set_u32(&mut image, layout.width, 256);
    set_u32(&mut image, layout.height, 64);
    let mut create = message(Command::CreateAllocation, 0x40);
    set_u32(&mut create, 0x28, 1);
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&(image.len() as u32).to_le_bytes());
    create.extend_from_slice(&[0; 4]);
    create.extend_from_slice(&image);

    let mut list_private = ring_private(1, 2, 0);
    set_u32(&mut list_private, 0x14, 2);
    set_u64(&mut list_private, 0x1C, list);

    let mut escape = message(Command::DriverEscape, HEADER_SIZE + 24 + 0x90);
    set_u32(&mut escape, HEADER_SIZE + 16, 0x90);
    set_u32(&mut escape, HEADER_SIZE + 24 + 0x10, 0x0500_002B);
    set_u32(&mut escape, HEADER_SIZE + 24 + 0x38, 0x2080_0110);
    set_u32(&mut escape, HEADER_SIZE + 24 + 0x3C, 68);

    let mut record = message(Command::QueryAdapter, HEADER_SIZE + 8);
    set_u32(&mut record, HEADER_SIZE + 4, 0x100);

    let source = u32_at(&send(&mut session, &create), 0x10).unwrap();
    let target = u32_at(&send(&mut session, &create), 0x10).unwrap();
    let mut lock = message(Command::LockForCpu, 0x20);
    set_u32(&mut lock, 0x1C, target);
    send(&mut session, &lock);
    let mut frame = message(Command::CopyToStaging, 0x50);
    set_u32(&mut frame, 0x20, source);
    set_u32(&mut frame, 0x24, target);
    set_u32(&mut frame, 0x30, 64);
    set_u32(&mut frame, 0x34, 64);
    // A copy that waited on this thread would spin until it timed out, as nothing here finishes it.
    session.set_copy_order(CopyOrder::Held);

    let templates = vec![
        submission(graphics_queue, 1, &ring_private(1, 2, ring)),
        submission(graphics_queue, 2, &ring_private(2, 4, ring)),
        submission(copy_queue, 1, &list_private),
        create,
        escape,
        record,
        frame,
    ];
    (session, templates)
}

/// Random messages reach the backend's own parsing here, as a whole guest's would.
#[test]
fn no_random_message_panics_the_backend() {
    let rounds =
        env::var("FUZZ_ROUNDS").ok().and_then(|text| text.parse().ok()).unwrap_or(DEFAULT_FUZZ_ROUNDS);
    let seed = env::var("FUZZ_SEED").ok().and_then(|text| text.parse().ok()).unwrap_or(DEFAULT_FUZZ_SEED);
    let mut messages = RandomMessages::new(seed, RandomMessages::all_commands());
    let mut session = None;
    let mut out = Vec::new();
    for round in 0..rounds {
        if round % FUZZ_ROUNDS_PER_SESSION == 0 {
            let (fresh, templates) = guest_with_templates();
            messages.set_templates(templates);
            session = Some(fresh);
        }
        let Some(session) = session.as_mut() else { continue };
        let message = messages.message();
        let _ = session.handle_message(&message, TICKET, &mut out);
        messages.learn(&out);
        let now = Instant::now();
        let _ = session.poll(if round % FUZZ_ROUNDS_TO_TIME_OUT == 0 { now + FAR_FUTURE } else { now });
    }
}

/// A list of `words` commands at `commands`, in the submission's own private data.
fn list_of(commands: u64, words: u32) -> Vec<u8> {
    let mut private = vec![0; 0x84];
    set_u32(&mut private, 0x14, 1);
    set_u32(&mut private, 0x0C, 0x84);
    private.extend_from_slice(&ring_entry(commands, words).to_le_bytes());
    private
}

fn pools_mapped(session: &Guest) -> usize {
    let pool = guesthouse_backend_nvidia::channel::LOCAL_MEMORY_BYTES;
    let calls = &session.backend().device().rm.calls;
    calls.iter().filter(|call| matches!(call, Call::MapDma(map) if map.length == pool)).count()
}

#[test]
fn a_pool_waits_for_the_window_a_guest_sets_and_then_reaches_every_graphics_channel() {
    const GUEST_WINDOW: u64 = 0x1234_5600_0000;
    let mut session = guest();
    let (commands, command_memory) = shared_allocation(&mut session, 0x1000);
    let (with_window, window_memory) = shared_allocation(&mut session, 0x1000);
    let window_words = [increasing(1, 0x7B0, 2), (GUEST_WINDOW >> 32) as u32, GUEST_WINDOW as u32];
    for (at, word) in window_words.iter().enumerate() {
        session.memory_mut().write_u32(window_memory + at as u64 * 4, *word).unwrap();
    }
    session.memory_mut().write_u32(command_memory, 0).unwrap();
    let graphics = context(&mut session, 0);
    let (early, _) = queue(&mut session, graphics);
    let (setter, _) = queue(&mut session, graphics);

    send(&mut session, &submission(early, 1, &list_of(commands, 1)));
    assert_eq!(pools_mapped(&session), 0, "no window has been set, so no pool is made up");

    send(&mut session, &submission(setter, 1, &list_of(with_window, 3)));
    assert_eq!(pools_mapped(&session), 1, "the queue that set it has a pool in its prelude");
    let setter_ring = session.backend().ring_of_list_queue(setter).unwrap();
    let early_ring = session.backend().ring_of_list_queue(early).unwrap();
    assert!(setter_ring[0].length > early_ring[0].length);

    send(&mut session, &submission(early, 2, &list_of(commands, 1)));
    assert_eq!(pools_mapped(&session), 2, "the queue made first gets one late");
    let early_ring = session.backend().ring_of_list_queue(early).unwrap();
    let (first_ahead, late, third) = (early_ring[1], early_ring[4], early_ring.len());
    assert!(late.length > first_ahead.length, "the late pool runs ahead of the next list");

    send(&mut session, &submission(early, 3, &list_of(commands, 1)));
    let early_ring = session.backend().ring_of_list_queue(early).unwrap();
    assert_eq!(early_ring[third], first_ahead, "and only once");
    assert_eq!(pools_mapped(&session), 2);
}

#[test]
fn a_queue_with_its_own_pool_is_given_none() {
    let mut session = guest();
    let (own, own_memory) = shared_allocation(&mut session, 0x1000);
    let words = [increasing(1, 0x790, 2), 1, 0, increasing(1, 0x7B0, 2), 0x1234, 0x5600_0000];
    for (at, word) in words.iter().enumerate() {
        session.memory_mut().write_u32(own_memory + at as u64 * 4, *word).unwrap();
    }
    let graphics = context(&mut session, 0);
    let (queue, _) = queue(&mut session, graphics);
    send(&mut session, &submission(queue, 1, &list_of(own, 0)));
    send(&mut session, &submission(queue, 2, &list_of(own, words.len() as u32)));
    send(&mut session, &submission(queue, 3, &list_of(own, 0)));
    assert_eq!(pools_mapped(&session), 0);
}

#[test]
fn on_a_guests_ring_the_window_is_read_and_a_late_pool_goes_in_the_gap() {
    let mut session = guest();
    let (early_ring, early_memory) = shared_allocation(&mut session, 0x10000);
    let (setter_ring, setter_memory) = shared_allocation(&mut session, 0x10000);
    let (with_window, window_memory) = shared_allocation(&mut session, 0x1000);
    let window_words = [increasing(1, 0x7B0, 2), 0x1234, 0x5600_0000];
    for (at, word) in window_words.iter().enumerate() {
        session.memory_mut().write_u32(window_memory + at as u64 * 4, *word).unwrap();
    }
    let graphics = context(&mut session, 0);
    let (early, _) = queue(&mut session, graphics);
    let (setter, _) = queue(&mut session, graphics);

    send(&mut session, &submission(early, 1, &ring_private(1, 2, early_ring)));
    assert_eq!(pools_mapped(&session), 0);
    let entry = ring_entry(with_window, window_words.len() as u32);
    session.memory_mut().write_u64(setter_memory + 8, entry).unwrap();
    send(&mut session, &submission(setter, 1, &ring_private(1, 2, setter_ring)));
    assert_eq!(pools_mapped(&session), 1, "the submission that set the window brings a pool with it");
    let prelude = |memory| session.memory().read_u64(memory).unwrap() >> 42;
    assert!(prelude(setter_memory) > prelude(early_memory));

    send(&mut session, &submission(early, 2, &ring_private(1, 6, early_ring)));
    assert_eq!(pools_mapped(&session), 2);
    send(&mut session, &submission(early, 3, &ring_private(1, 9, early_ring)));
    let late = session.memory().read_u64(early_memory + 4 * 8).unwrap() >> 42;
    let invalidate = session.memory().read_u64(early_memory + 7 * 8).unwrap() >> 42;
    assert!(late > invalidate && invalidate > 0, "the late pool goes once, then the invalidate alone");
}
