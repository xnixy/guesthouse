//! Whole messages through a session with the stand-in backend.

use super::*;
use crate::{FENCE_AREA, MemoryLimits, Waiting};
use guesthouse_backend_standin::{REGISTRY_NAME, SparseMemory, StandIn, VENDOR_ANSWER, VENDOR_ONLY_QUERY};
use guesthouse_messages::adapter::AdapterQuery;
use guesthouse_messages::allocation::{CACHED_FOR_CPU, PRIMARY};
use guesthouse_messages::notice::Notice;
use guesthouse_messages::wire::{put_utf16_terminated, set_u32, set_u64, u32_at, u64_at};
use guesthouse_messages::{HEADER_SIZE, standard};
use std::time::{Duration, Instant};

const IO_SPACE: u64 = 1 << 40;
const WINDOW_END: u64 = IO_SPACE + (1 << 30);
const PROCESS: u32 = 0x4000_2000;
const SUCCESS: u32 = Status::SUCCESS.0;

type TestSession = Session<StandIn, SparseMemory>;

/// A message with room for `size` bytes, and its command and process filled in.
fn message(command: Command, size: usize) -> Vec<u8> {
    let mut bytes = vec![0; size.max(HEADER_SIZE)];
    set_u32(&mut bytes, 8, PROCESS);
    set_u32(&mut bytes, 16, command.to_wire());
    bytes
}

fn session_with(cap: Option<u64>) -> TestSession {
    let backend = StandIn::new();
    let limits = MemoryLimits::new(backend.facts(), cap, 16 << 30);
    let mut config = Config::new(limits);
    config.frame_wait = Duration::ZERO;
    let mut session = Session::new(backend, SparseMemory::new(WINDOW_END), config);
    let mut request = message(Command::SetIoSpace, HEADER_SIZE + 16);
    set_u64(&mut request, HEADER_SIZE, IO_SPACE);
    set_u64(&mut request, HEADER_SIZE + 8, 8 << 30);
    assert_eq!(send(&mut session, &request), SUCCESS.to_le_bytes());
    session
}

fn session() -> TestSession {
    session_with(None)
}

fn send(session: &mut TestSession, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let answer = session.handle_message(message, Some(ReplyTicket::default()), &mut out);
    assert_eq!(answer, Answer::Now, "use send_held for what may be held");
    out
}

fn status_of(reply: &[u8]) -> u32 {
    u32_at(reply, 0).unwrap()
}

/// Stand-in private data asking for `size` bytes.
fn private(size: u64) -> Vec<u8> {
    let mut data = size.to_le_bytes().to_vec();
    data.resize(17, 0);
    data
}

/// As much memory as the surface needs.
fn surface_private(width: u32, height: u32) -> Vec<u8> {
    let mut data = private((u64::from(width) * u64::from(height) * 4).max(0x10000));
    set_u32(&mut data, 8, width);
    set_u32(&mut data, 12, height);
    data
}

fn create_message(flags: u32, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut request = message(Command::CreateAllocation, 0x40);
    set_u32(&mut request, 0x28, blocks.len() as u32);
    set_u32(&mut request, 0x2C, flags);
    for block in blocks {
        request.extend_from_slice(&[0; 4]);
        request.extend_from_slice(&(block.len() as u32).to_le_bytes());
        request.extend_from_slice(&[0; 4]);
    }
    blocks.iter().for_each(|block| request.extend_from_slice(block));
    request
}

/// A request whose one entry claims to be a screen's primary surface.
fn primary_request(block: Vec<u8>) -> Vec<u8> {
    let mut request = create_message(0, &[block]);
    set_u32(&mut request, 0x40, PRIMARY);
    request
}

/// Makes one allocation in a resource of its own. Returns (resource, allocation).
fn allocate(session: &mut TestSession, block: Vec<u8>) -> (u32, u32) {
    let reply = send(session, &create_message(0, &[block]));
    assert_eq!(status_of(&reply), SUCCESS);
    (u32_at(&reply, 4).unwrap(), u32_at(&reply, 0x10).unwrap())
}

fn map(session: &mut TestSession, allocation: u32, address: u64, first_page: u64, pages: u64) -> (u64, u32) {
    let mut request = message(Command::MapGpuAddresses, 0x50);
    set_u64(&mut request, 0x20, address);
    set_u32(&mut request, 0x38, allocation);
    set_u64(&mut request, 0x40, first_page);
    set_u64(&mut request, 0x48, pages);
    let reply = send(session, &request);
    (u64_at(&reply, 0).unwrap(), u32_at(&reply, 16).unwrap())
}

fn lock(session: &mut TestSession, allocation: u32) -> (u32, u64) {
    let mut request = message(Command::LockForCpu, 0x20);
    set_u32(&mut request, 0x1C, allocation);
    let reply = send(session, &request);
    (status_of(&reply), u64_at(&reply, 8).unwrap())
}

fn destroy(session: &mut TestSession, resource: u32, handles: &[u32]) {
    let mut request = message(Command::DestroyAllocation, 0x28);
    set_u32(&mut request, 0x1C, resource);
    set_u32(&mut request, 0x20, handles.len() as u32);
    handles.iter().for_each(|handle| request.extend_from_slice(&handle.to_le_bytes()));
    assert_eq!(status_of(&send(session, &request)), SUCCESS);
}

/// Returns (handle, GPU address, fence).
fn create_fence(session: &mut TestSession, initial: u64) -> (u32, u64, u64) {
    let mut request = message(Command::CreateSyncObject, 0x30);
    set_u32(&mut request, 0x20, 5);
    set_u64(&mut request, 0x28, initial);
    let reply = send(session, &request);
    assert_eq!(u32_at(&reply, 0), u32_at(&reply, 4));
    (u32_at(&reply, 0).unwrap(), u64_at(&reply, 8).unwrap(), u64_at(&reply, 16).unwrap())
}

fn destroy_sync(session: &mut TestSession, handle: u32) {
    let mut request = message(Command::DestroySyncObject, 0x20);
    set_u32(&mut request, 0x18, handle);
    send(session, &request);
}

fn create_context(session: &mut TestSession, node: u32) -> u32 {
    let mut request = message(Command::CreateContext, 0x34);
    set_u32(&mut request, 0x20, node);
    u32_at(&send(session, &request), 0x18).unwrap()
}

/// Returns (queue, progress sync object, fence).
fn create_queue(session: &mut TestSession, context: u32) -> (u32, u32, u64) {
    let mut request = message(Command::CreateHardwareQueue, 0x48);
    set_u32(&mut request, 0x38, context);
    set_u32(&mut request, 0x40, 4);
    let reply = send(session, &request);
    assert_eq!(u32_at(&reply, 0x18), Ok(SUCCESS));
    (u32_at(&reply, 0x1C).unwrap(), u32_at(&reply, 0x20).unwrap(), u64_at(&reply, 0x28).unwrap())
}

fn submit(session: &mut TestSession, queue: u32, value: u64) {
    let mut request = message(Command::SubmitToHardwareQueue, 0x50);
    set_u32(&mut request, 0x18, queue);
    set_u64(&mut request, 0x20, value);
    assert_eq!(send(session, &request), SUCCESS.to_le_bytes());
}

fn signal_message(objects: &[u32], contexts: &[u32], values: &[u64]) -> Vec<u8> {
    let mut request = message(Command::SignalSyncObject, 0x38);
    set_u32(&mut request, 0x18, objects.len() as u32);
    set_u32(&mut request, 0x20, contexts.len() as u32);
    objects.iter().chain(contexts).for_each(|word| request.extend_from_slice(&word.to_le_bytes()));
    values.iter().for_each(|value| request.extend_from_slice(&value.to_le_bytes()));
    request
}

mod copy_bounds;
mod failing;
mod frame_ring;
mod indirect;

mod dispatch {
    use super::*;

    #[test]
    fn unknown_commands_are_not_supported() {
        let reply = send(&mut session(), &message(Command::Unknown(68), HEADER_SIZE));
        assert_eq!(status_of(&reply), Status::NOT_SUPPORTED.0);
    }

    #[test]
    fn something_too_short_to_read_is_not_supported() {
        assert_eq!(status_of(&send(&mut session(), &[0; 8])), Status::NOT_SUPPORTED.0);
    }

    #[test]
    fn a_message_cut_short_fails_plainly() {
        let reply = send(&mut session(), &message(Command::CreateSyncObject, HEADER_SIZE));
        assert_eq!(reply, Status::UNSUCCESSFUL.0.to_le_bytes());
    }

    #[test]
    fn plain_commands_answer_success() {
        for command in [Command::FlushDevice, Command::UnlockForCpu, Command::IsSyncObjectSignaled] {
            assert_eq!(send(&mut session(), &message(command, HEADER_SIZE)), SUCCESS.to_le_bytes());
        }
    }

    #[test]
    fn residency_answers_have_two_values_and_a_status() {
        let reply = send(&mut session(), &message(Command::MakeResident, HEADER_SIZE));
        assert_eq!(reply, [0; 24]);
    }

    #[test]
    fn registry_keys_answer_is_a_count_then_a_status() {
        assert_eq!(send(&mut session(), &message(Command::GetRegistryKeys, HEADER_SIZE)), [0; 8]);
    }

    #[test]
    fn each_object_gets_its_own_handle() {
        let mut session = session();
        let first = send(&mut session, &message(Command::CreateDevice, HEADER_SIZE));
        let second = send(&mut session, &message(Command::CreateDevice, HEADER_SIZE));
        assert_eq!(status_of(&second) - status_of(&first), 0x40);
    }

    #[test]
    fn a_device_answers_the_queued_limit_it_was_given() {
        use guesthouse_messages::queue::{
            DEFAULT_QUEUED_PRESENT_LIMIT, GET_QUEUED_PRESENT_LIMIT, SET_QUEUED_PRESENT_LIMIT,
        };
        const DEVICE: usize = HEADER_SIZE;
        const KIND: usize = HEADER_SIZE + 4;
        const LIMIT: usize = HEADER_SIZE + 8;
        let mut session = session();
        let device = status_of(&send(&mut session, &message(Command::CreateDevice, HEADER_SIZE)));
        let mut read = message(Command::SetQueuedLimit, HEADER_SIZE + 0x10);
        set_u32(&mut read, DEVICE, device);
        set_u32(&mut read, KIND, GET_QUEUED_PRESENT_LIMIT);
        set_u32(&mut read, LIMIT, 0xDEAD_BEEF);
        let answer = send(&mut session, &read);
        assert_eq!(
            u32_at(&answer, LIMIT - DEVICE),
            Ok(DEFAULT_QUEUED_PRESENT_LIMIT),
            "not the guest's field"
        );
        assert_eq!(u32_at(&answer, 0x10), Ok(SUCCESS));
        let mut set = read.clone();
        set_u32(&mut set, KIND, SET_QUEUED_PRESENT_LIMIT);
        set_u32(&mut set, LIMIT, 1);
        assert_eq!(send(&mut session, &set)[..0x10], set[DEVICE..DEVICE + 0x10]);
        assert_eq!(u32_at(&send(&mut session, &read), LIMIT - DEVICE), Ok(1));
        let mut other = read.clone();
        set_u32(&mut other, DEVICE, device + 1);
        assert_eq!(u32_at(&send(&mut session, &other), LIMIT - DEVICE), Ok(DEFAULT_QUEUED_PRESENT_LIMIT));
        let mut destroy = message(Command::DestroyDevice, HEADER_SIZE + 4);
        set_u32(&mut destroy, DEVICE, device);
        assert_eq!(status_of(&send(&mut session, &destroy)), SUCCESS);
        assert_eq!(u32_at(&send(&mut session, &read), LIMIT - DEVICE), Ok(DEFAULT_QUEUED_PRESENT_LIMIT));
    }

    #[test]
    fn a_32_bit_process_is_remembered_for_the_vendor() {
        let mut session = session();
        let mut request = message(Command::CreateProcess, 0x233);
        request[0x232] = 4;
        let process = status_of(&send(&mut session, &request));
        assert!(session.processes_32_bit.contains(&process));
        let mut request = message(Command::DestroyProcess, HEADER_SIZE);
        set_u32(&mut request, 8, process);
        send(&mut session, &request);
        assert!(!session.processes_32_bit.contains(&process));
    }

    #[test]
    fn a_process_is_known_by_its_pid_in_the_guest_until_it_goes() {
        let mut session = session();
        let mut request = message(Command::CreateProcess, 0x233);
        set_u32(&mut request, 0x20, 6600);
        let process = status_of(&send(&mut session, &request));
        assert_eq!(session.guest_pids.get(&process), Some(&6600));
        let mut request = message(Command::DestroyProcess, HEADER_SIZE);
        set_u32(&mut request, 8, process);
        send(&mut session, &request);
        assert_eq!(session.guest_pids.get(&process), None);
    }
}

mod adapter {
    use super::*;

    fn query(session: &mut TestSession, kind: u32, size: u32, data: &[u8]) -> Vec<u8> {
        let mut request = message(Command::QueryAdapter, HEADER_SIZE + 8);
        set_u32(&mut request, HEADER_SIZE, kind);
        set_u32(&mut request, HEADER_SIZE + 4, size);
        request.extend_from_slice(data);
        send(session, &request)
    }

    #[test]
    fn the_vendor_is_asked_first() {
        let reply = query(&mut session(), VENDOR_ONLY_QUERY, 4, &[0; 4]);
        assert_eq!(&reply[4..], VENDOR_ANSWER);
    }

    #[test]
    fn neutral_queries_need_no_vendor() {
        let mut session = session();
        assert_eq!(u32_at(&query(&mut session, AdapterQuery::PHYSICAL_ADAPTER_COUNT, 4, &[]), 4), Ok(1));
        assert_eq!(u32_at(&query(&mut session, AdapterQuery::DRIVER_MODEL, 4, &[]), 4), Ok(3000));
        assert_eq!(query(&mut session, AdapterQuery::DIRECT_FLIP, 12, &[]), [0; 16]);
    }

    #[test]
    fn the_guest_driver_version_comes_from_the_facts() {
        let mut session = session();
        session.backend_mut();
        let facts = guesthouse_backend::AdapterFacts {
            guest_driver_version: 0x1234,
            ..session.backend().facts().clone()
        };
        session.backend = StandIn::with_facts(facts);
        for kind in [AdapterQuery::DRIVER_VERSION, AdapterQuery::USER_DRIVER_VERSION] {
            assert_eq!(u64_at(&query(&mut session, kind, 8, &[]), 4), Ok(0x1234));
        }
    }

    #[test]
    fn unknown_queries_are_a_bare_status() {
        assert_eq!(query(&mut session(), 9999, 4, &[0; 4]), Status::NOT_SUPPORTED.0.to_le_bytes());
    }

    #[test]
    fn segment_sizes_follow_the_cap() {
        let reply = query(&mut session_with(Some(4 << 30)), AdapterQuery::SEGMENT_SIZES, 24, &[]);
        assert_eq!(u64_at(&reply, 4), Ok(4 << 30));
        assert_eq!(u64_at(&reply, 20), Ok(16 << 30));
    }

    fn registry_buffer(name: &str, value_type: u32) -> Vec<u8> {
        let mut buffer = vec![0; 544 + 64];
        let mut text = Vec::new();
        put_utf16_terminated(&mut text, name);
        buffer[8..8 + text.len()].copy_from_slice(&text);
        set_u32(&mut buffer, 528, value_type);
        buffer
    }

    #[test]
    fn registry_names_ignore_case() {
        let buffer = registry_buffer(&REGISTRY_NAME.to_uppercase(), 1);
        let reply = query(&mut session(), AdapterQuery::REGISTRY, buffer.len() as u32, &buffer);
        assert_eq!(status_of(&reply), SUCCESS);
        assert_eq!(u32_at(&reply, 4 + 536), Ok(10));
    }

    #[test]
    fn the_wrong_type_fails_so_a_loader_tries_the_other() {
        let buffer = registry_buffer(REGISTRY_NAME, 7);
        let reply = query(&mut session(), AdapterQuery::REGISTRY, buffer.len() as u32, &buffer);
        assert_eq!(status_of(&reply), Status::INVALID_PARAMETER.0);
        assert_eq!(&reply[4..], &buffer);
    }

    #[test]
    fn an_unknown_value_is_not_found_and_comes_back_unchanged() {
        let buffer = registry_buffer("nothing", 1);
        let reply = query(&mut session(), AdapterQuery::REGISTRY, buffer.len() as u32, &buffer);
        assert_eq!(status_of(&reply), Status::NOT_FOUND.0);
        assert_eq!(&reply[4..], &buffer);
    }

    #[test]
    fn identity_says_whether_messages_may_be_async() {
        let mut session = session();
        session.config.async_messages = true;
        let reply = send(&mut session, &message(Command::GetAdapterIdentity, HEADER_SIZE));
        assert_eq!(u32_at(&reply, 12), Ok(0x40));
    }

    #[test]
    fn node_count_reaches_the_physical_adapter_caps() {
        let mut request = message(Command::QueryDriverCaps, HEADER_SIZE + 12);
        set_u32(&mut request, HEADER_SIZE, 15);
        set_u32(&mut request, HEADER_SIZE + 8, 8);
        assert_eq!(u32_at(&send(&mut session(), &request), 0), Ok(8));
    }

    #[test]
    fn escapes_go_to_the_vendor_and_pad_to_eight_bytes() {
        let mut request = message(Command::DriverEscape, HEADER_SIZE + 24);
        set_u32(&mut request, HEADER_SIZE + 16, 3);
        request.extend_from_slice(&[1, 2, 3]);
        assert_eq!(send(&mut session(), &request), [0xFE, 0xFD, 0xFC, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn video_memory_usage_counts_allocations() {
        let mut session = session();
        allocate(&mut session, private(0x20000));
        let reply = send(&mut session, &message(Command::QueryVideoMemory, 0x20));
        assert_eq!(u64_at(&reply, 0), Ok(6 << 30));
        assert_eq!(u64_at(&reply, 8), Ok(0x20000));
        assert_eq!(u64_at(&reply, 24), Ok(3 << 30));
    }

    #[test]
    fn the_system_group_reports_guest_ram() {
        let mut request = message(Command::QueryVideoMemory, 0x20);
        set_u32(&mut request, 0x1C, 1);
        let reply = send(&mut session(), &request);
        assert_eq!(u64_at(&reply, 0), Ok(16 << 30));
        assert_eq!(u64_at(&reply, 8), Ok(0));
    }
}

mod allocations {
    use super::*;

    #[test]
    fn a_resource_holds_its_allocations() {
        let mut session = session();
        let reply = send(&mut session, &create_message(0, &[private(0x1000), private(0x2000)]));
        assert_eq!(reply.len(), 0x10 + 2 * 0x48 + 2 * 17 + 6);
        let (first, second) = (u32_at(&reply, 0x10).unwrap(), u32_at(&reply, 0x58).unwrap());
        assert_eq!(session.resources[&u32_at(&reply, 4).unwrap()], [first, second]);
        assert_eq!(u64_at(&reply, 0x20), Ok(0x1000));
        assert_eq!(u64_at(&reply, 0x68), Ok(0x2000));
    }

    #[test]
    fn the_vendor_writes_back_into_the_private_data() {
        let reply = send(&mut session(), &create_message(0, &[private(0x1000)]));
        assert_eq!(reply[0x58], 0xFF);
    }

    #[test]
    fn every_allocation_is_cached_for_the_cpu_and_primaries_say_so() {
        let mut request = create_message(0, &[private(0x1000)]);
        set_u32(&mut request, 0x40, 1);
        let reply = send(&mut session(), &request);
        assert_eq!(u32_at(&reply, 0x18), Ok(1 | 0x80000));
    }

    #[test]
    fn a_surface_shape_reaches_the_answer() {
        let reply = send(&mut session(), &create_message(0, &[surface_private(640, 480)]));
        assert_eq!(u32_at(&reply, 0x30), Ok(640));
        assert_eq!(u32_at(&reply, 0x34), Ok(480));
        assert_eq!(u32_at(&reply, 0x3C), Ok(1));
        assert_eq!(u32_at(&reply, 0x44), Ok(60));
    }

    #[test]
    fn a_standard_record_is_ours_and_not_the_vendors() {
        let mut session = session();
        let mut describe = message(Command::DescribeStandardAllocation, 0x40);
        set_u32(&mut describe, 0x18, 1);
        set_u32(&mut describe, 0x1C, 32);
        set_u32(&mut describe, 0x28, 100);
        set_u32(&mut describe, 0x2C, 10);
        let answer = send(&mut session, &describe);
        let record = answer[36..68].to_vec();
        let reply = send(&mut session, &create_message(0, std::slice::from_ref(&record)));
        assert_eq!(u64_at(&reply, 0x20), Ok(0x10000));
        assert_eq!(&reply[0x58..0x58 + 32], record);
        assert_eq!(standard::StandardRecord::parse(&record).unwrap().pitch, 512);
    }

    #[test]
    fn a_primary_stays_one_on_the_basic_display() {
        let reply = send(&mut session(), &primary_request(surface_private(1920, 1080)));
        assert_eq!(status_of(&reply), SUCCESS);
        assert_eq!(u32_at(&reply, 0x18), Ok(PRIMARY | CACHED_FOR_CPU));
    }

    #[test]
    fn the_video_cap_refuses_all_or_nothing() {
        let mut session = session_with(Some(0x3000));
        let reply = send(&mut session, &create_message(0, &[private(0x2000), private(0x2000)]));
        assert_eq!(status_of(&reply), Status::NO_VIDEO_MEMORY.0);
        assert_eq!(reply.len(), 0x10 + 2 * 0x48);
        assert!(session.allocations.is_empty());
        assert_eq!(session.backend().live_memory_blocks(), 0);
    }

    #[test]
    fn private_data_the_vendor_refuses_fails_the_whole_call() {
        let mut session = session();
        session.backend_mut().refuse_private_data = true;
        let reply = send(&mut session, &create_message(0, &[private(0x2000), private(0x2000)]));
        assert_eq!(status_of(&reply), Status::INVALID_PARAMETER.0);
        assert!(session.allocations.is_empty());
        assert_eq!(session.backend().live_memory_blocks(), 0);
    }

    #[test]
    fn our_own_records_are_not_offered_to_the_vendor() {
        let mut session = session();
        session.backend_mut().refuse_private_data = true;
        let mut describe = message(Command::DescribeStandardAllocation, 0x40);
        set_u32(&mut describe, 0x18, 1);
        set_u32(&mut describe, 0x1C, 32);
        set_u32(&mut describe, 0x28, 100);
        set_u32(&mut describe, 0x2C, 10);
        let record = send(&mut session, &describe)[36..68].to_vec();
        let reply = send(&mut session, &create_message(0, &[record]));
        assert_eq!(status_of(&reply), SUCCESS);
    }

    #[test]
    fn locked_memory_stops_counting_against_the_cap() {
        let mut session = session_with(Some(0x3000));
        let (_, handle) = allocate(&mut session, private(0x2000));
        lock(&mut session, handle);
        let reply = send(&mut session, &create_message(0, &[private(0x2000)]));
        assert_eq!(status_of(&reply), SUCCESS);
    }

    #[test]
    fn a_failed_allocation_answers_with_a_bare_failure() {
        let mut session = session();
        session.backend_mut().fail_next_allocation = true;
        let reply = send(&mut session, &create_message(0, &[private(0x1000)]));
        assert_eq!(reply, Status::UNSUCCESSFUL.0.to_le_bytes());
    }

    #[test]
    fn destroying_a_resource_frees_its_memory() {
        let mut session = session();
        let (resource, _) = allocate(&mut session, private(0x1000));
        destroy(&mut session, resource, &[]);
        assert_eq!(session.backend().live_memory_blocks(), 0);
        assert!(session.allocations.is_empty());
    }

    #[test]
    fn naming_an_allocation_frees_it_and_its_resource_goes_with_the_last_one() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        destroy(&mut session, 0, &[handle]);
        assert!(session.allocations.is_empty());
        assert!(session.resources.is_empty());
    }

    fn open(session: &mut TestSession, shared: u32, count: u32) -> Vec<u8> {
        let mut request = message(Command::OpenResource, 0x28);
        set_u32(&mut request, 0x20, shared);
        set_u32(&mut request, 0x24, count);
        send(session, &request)
    }

    #[test]
    fn a_shared_resource_lasts_until_its_last_user() {
        let mut session = session();
        let (resource, handle) = allocate(&mut session, private(0x1000));
        assert_eq!(u32_at(&open(&mut session, resource, 1), 8), Ok(handle));
        destroy(&mut session, resource, &[]);
        assert!(session.allocations.contains_key(&handle));
        destroy(&mut session, resource, &[]);
        assert!(session.allocations.is_empty());
    }

    #[test]
    fn a_frame_another_process_opened_survives_its_maker_naming_it() {
        let mut session = session();
        let (resource, handle) = allocate(&mut session, private(0x1000));
        open(&mut session, resource, 1);
        destroy(&mut session, 0, &[handle]);
        assert!(session.allocations.contains_key(&handle));
        // The maker has gone, which leaves one user, and its second message must still free nothing.
        destroy(&mut session, resource, &[]);
        destroy(&mut session, 0, &[handle]);
        assert!(session.allocations.contains_key(&handle));
        destroy(&mut session, resource, &[]);
        assert!(session.allocations.is_empty());
        assert!(session.resources.is_empty() && session.resource_users.is_empty());
    }

    #[test]
    fn a_shared_object_keeps_a_named_allocation_until_it_goes() {
        let mut session = session();
        let (resource, handle) = allocate(&mut session, private(0x1000));
        let mut request = message(Command::CreateSharedObject, 0x20);
        set_u32(&mut request, 0x18, resource);
        let shared = status_of(&send(&mut session, &request));
        destroy(&mut session, 0, &[handle]);
        assert!(session.allocations.contains_key(&handle));
        let mut request = message(Command::DestroySharedObject, 0x20);
        set_u32(&mut request, 0x18, shared);
        send(&mut session, &request);
        destroy(&mut session, 0, &[handle]);
        assert!(session.allocations.is_empty());
    }

    #[test]
    fn opening_works_through_a_shared_handle() {
        let mut session = session();
        let (resource, handle) = allocate(&mut session, private(0x1000));
        let mut request = message(Command::CreateSharedObject, 0x20);
        set_u32(&mut request, 0x18, resource);
        let shared = status_of(&send(&mut session, &request));
        assert_eq!(u32_at(&open(&mut session, shared, 1), 8), Ok(handle));
    }

    #[test]
    fn opening_more_than_a_resource_holds_is_refused() {
        let mut session = session();
        let (resource, _) = allocate(&mut session, private(0x1000));
        let reply = open(&mut session, resource, 2);
        assert_eq!(u32_at(&reply, 4), Ok(Status::INVALID_PARAMETER.0));
        assert!(session.resource_users.is_empty());
    }

    #[test]
    fn guest_backed_allocations_wait_for_their_pages() {
        let mut session = session();
        let block = private(0x2000);
        let reply = send(&mut session, &create_message(0x20, &[block]));
        let handle = u32_at(&reply, 0x10).unwrap();
        assert_eq!(session.backend().live_memory_blocks(), 0);
        assert_eq!(lock(&mut session, handle).0, Status::INVALID_PARAMETER.0);

        let pages = |first: u32, list: &[u64]| {
            let mut request = message(Command::SetGuestPages, 0x28);
            set_u32(&mut request, 0x1C, handle);
            set_u32(&mut request, 0x20, list.len() as u32);
            set_u32(&mut request, 0x24, first);
            list.iter().for_each(|page| request.extend_from_slice(&page.to_le_bytes()));
            request
        };
        assert_eq!(status_of(&send(&mut session, &pages(0, &[7]))), SUCCESS);
        assert!(session.memory().views.is_empty());
        assert_eq!(status_of(&send(&mut session, &pages(1, &[9]))), SUCCESS);
        assert_eq!(session.memory().views.len(), 1);
        assert_eq!(session.backend().live_memory_blocks(), 1);

        let resource = u32_at(&reply, 4).unwrap();
        destroy(&mut session, resource, &[]);
        assert!(session.memory().views.is_empty());
    }

    /// A picture setup is 0x2800 bytes, and the guest maps it in whole pages, which is more. Those
    /// pages already hold what the guest wrote, so the map must keep them.
    #[test]
    fn guest_pages_stay_when_the_map_is_larger_than_the_size() {
        let mut session = session();
        let reply = send(&mut session, &create_message(0x20, &[private(0x2800)]));
        assert_eq!(status_of(&reply), SUCCESS);
        let handle = u32_at(&reply, 0x10).unwrap();
        let mut request = message(Command::SetGuestPages, 0x28);
        set_u32(&mut request, 0x1C, handle);
        set_u32(&mut request, 0x20, 3);
        set_u32(&mut request, 0x24, 0);
        for page in [7u64, 8, 9] {
            request.extend_from_slice(&page.to_le_bytes());
        }
        assert_eq!(status_of(&send(&mut session, &request)), SUCCESS);
        let memory = session.allocations[&handle].memory;
        assert!(memory.is_some());

        let (address, status) = map(&mut session, handle, 0, 0, 16);
        assert_eq!(status, SUCCESS);
        assert_ne!(address, 0);
        let allocation = &session.allocations[&handle];
        assert_eq!(allocation.memory, memory);
        assert_eq!(allocation.size, 0x2800);
        assert_eq!(allocation.mappings[0].length, 0x3000);
    }

    #[test]
    fn pages_for_an_unknown_allocation_are_refused() {
        let mut request = message(Command::SetGuestPages, 0x28);
        set_u32(&mut request, 0x1C, 0x999);
        assert_eq!(status_of(&send(&mut session(), &request)), Status::INVALID_PARAMETER.0);
    }

    #[test]
    fn locking_moves_memory_to_shared_pages_at_the_same_address() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x10000));
        let (mapped, _) = map(&mut session, handle, 0, 0, 0);
        let before = session.backend().mapping_count();
        let (status, address) = lock(&mut session, handle);
        assert_eq!(status, SUCCESS);
        assert_eq!(address, IO_SPACE + FENCE_AREA);
        assert_eq!(session.allocations[&handle].mappings[0].address, mapped);
        assert_eq!(session.backend().mapping_count(), before + 1, "only the daemon's space is new");
        assert_eq!(session.backend().live_memory_blocks(), 2, "the shared pages and the fence block");
    }

    #[test]
    fn a_second_lock_answers_the_same_address() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        assert_eq!(lock(&mut session, handle), lock(&mut session, handle));
    }

    #[test]
    fn locking_an_unknown_allocation_is_refused() {
        assert_eq!(lock(&mut session(), 0x999).0, Status::INVALID_PARAMETER.0);
    }

    #[test]
    fn locking_past_the_window_runs_out_of_memory() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(2 << 30));
        assert_eq!(lock(&mut session, handle), (Status::NO_MEMORY.0, 0));
    }

    #[test]
    fn freed_shared_memory_is_used_again_and_cleared() {
        let mut session = session();
        let (resource, handle) = allocate(&mut session, private(0x1000));
        let (_, first) = lock(&mut session, handle);
        session.memory_mut().write_u64(first, 0xDEAD).unwrap();
        destroy(&mut session, resource, &[]);
        let (_, handle) = allocate(&mut session, private(0x1000));
        let (_, second) = lock(&mut session, handle);
        assert_eq!(second, first);
        assert_eq!(session.memory().read_u64(second).unwrap(), 0);
    }

    #[test]
    fn a_first_lock_waits_for_the_gpu_to_finish_in_its_space() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let (_, mapped) = allocate(&mut session, private(0x1000));
        map(&mut session, mapped, 0, 0, 0);
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        submit(&mut session, queue, 1);
        let mut request = message(Command::LockForCpu, 0x20);
        set_u32(&mut request, 0x1C, mapped);
        let mut out = Vec::new();
        let answer = session.handle_message(&request, Some(ReplyTicket::default()), &mut out);
        assert_eq!(answer, Answer::Held);
        assert!(session.backend().preserved.is_empty(), "the engine may still be writing it");
        session.memory_mut().write_u64(fence, 1).unwrap();
        let _ = session.poll(Instant::now());
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().preserved.len(), 1);
        assert_eq!(session.take_replies().count(), 1);
    }

    #[test]
    fn contents_move_only_once_work_has_run() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let (_, unmapped) = allocate(&mut session, private(0x1000));
        let (_, mapped) = allocate(&mut session, private(0x1000));
        map(&mut session, mapped, 0, 0, 0);
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        lock(&mut session, mapped);
        assert!(session.backend().preserved.is_empty());
        let (_, again) = allocate(&mut session, private(0x1000));
        map(&mut session, again, 0, 0, 0);
        submit(&mut session, queue, 1);
        session.memory_mut().write_u64(fence, 1).unwrap();
        lock(&mut session, unmapped);
        assert!(session.backend().preserved.is_empty(), "nothing was mapped, so nothing was written");
        lock(&mut session, again);
        assert_eq!(session.backend().preserved.len(), 1);
    }
}

mod mappings {
    use super::*;

    #[test]
    fn a_range_with_no_allocation_echoes_its_address() {
        assert_eq!(map(&mut session(), 0, 0x5000_0000, 0, 0), (0x5000_0000, SUCCESS));
    }

    #[test]
    fn an_unknown_allocation_is_refused() {
        assert_eq!(map(&mut session(), 0x999, 0, 0, 0), (0, Status::INVALID_PARAMETER.0));
    }

    #[test]
    fn the_host_picks_an_address_after_the_fence_block() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        let (address, status) = map(&mut session, handle, 0, 0, 0);
        assert_eq!(status, SUCCESS);
        assert_eq!(address, crate::GPU_RANGES[0].start + FENCE_AREA);
    }

    #[test]
    fn big_allocations_get_a_large_alignment() {
        let mut session = session();
        let (_, small) = allocate(&mut session, private(0x1000));
        map(&mut session, small, 0, 0, 0);
        let (_, big) = allocate(&mut session, private(2 << 20));
        let (address, _) = map(&mut session, big, 0, 0, 0);
        assert_eq!(address % (2 << 20), 0);
    }

    #[test]
    fn later_maps_of_an_allocation_land_inside_the_first() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x10000));
        let (base, _) = map(&mut session, handle, 0, 0, 1);
        assert_eq!(map(&mut session, handle, 0, 3, 1), (base + 0x3000, SUCCESS));
        assert_eq!(session.allocations[&handle].mappings.len(), 1);
    }

    #[test]
    fn an_allocation_too_small_for_the_map_grows_while_it_is_empty() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        map(&mut session, handle, 0, 0, 4);
        assert_eq!(session.allocations[&handle].size, 0x4000);
        assert_eq!(session.backend().live_memory_blocks(), 2, "the grown block and the fence block");
    }

    #[test]
    fn a_chosen_address_maps_only_what_was_asked() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x10000));
        assert_eq!(map(&mut session, handle, 0x5000_0000, 2, 1), (0x5000_0000, SUCCESS));
        let mapping = session.allocations[&handle].mappings[0];
        assert_eq!((mapping.first, mapping.length), (0x2000, 0x1000));
    }

    #[test]
    fn a_chosen_address_outside_every_range_fails() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        assert_eq!(map(&mut session, handle, 0x1_0000_0000, 0, 1), (0, Status::UNSUCCESSFUL.0));
    }

    #[test]
    fn a_taken_chosen_address_is_cleared_first() {
        let mut session = session();
        let (_, first) = allocate(&mut session, private(0x4000));
        map(&mut session, first, 0x5000_0000, 0, 4);
        let (_, second) = allocate(&mut session, private(0x1000));
        assert_eq!(map(&mut session, second, 0x5000_1000, 0, 1), (0x5000_1000, SUCCESS));
        let mut pieces: Vec<_> =
            session.allocations[&first].mappings.iter().map(|m| (m.first, m.length)).collect();
        pieces.sort();
        assert_eq!(pieces, [(0, 0x1000), (0x2000, 0x2000)], "the old mapping keeps both sides");
    }

    #[test]
    fn reserving_rounds_and_advances() {
        let mut session = session();
        let mut request = message(Command::ReserveGpuAddresses, 0x40);
        set_u64(&mut request, 0x38, 1);
        send(&mut session, &request);
        let first = u64_at(&send(&mut session, &request), 0).unwrap();
        let second = u64_at(&send(&mut session, &request), 0).unwrap();
        assert_eq!(second - first, crate::ALLOCATION_ALIGN);
    }

    fn operation(kind: u32, address: u64, size: u64, third: u64, offset: u64) -> Vec<u8> {
        let mut bytes = vec![0; 64];
        set_u32(&mut bytes, 0, kind);
        set_u64(&mut bytes, 8, address);
        set_u64(&mut bytes, 16, size);
        set_u64(&mut bytes, 24, third);
        set_u64(&mut bytes, 32, offset);
        bytes
    }

    fn update(session: &mut TestSession, fence: u32, value: u64, operations: &[Vec<u8>]) -> Vec<u8> {
        let mut request = message(Command::UpdateGpuAddresses, 0x38);
        set_u64(&mut request, 0x18, value);
        set_u32(&mut request, 0x28, fence);
        set_u32(&mut request, 0x2C, operations.len() as u32);
        operations.iter().for_each(|operation| request.extend_from_slice(operation));
        send(session, &request)
    }

    #[test]
    fn an_update_maps_and_raises_the_fence() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x4000));
        let (sync, _, fence) = create_fence(&mut session, 5);
        let operations = [operation(0, 0x5000_0000, 0x2000, u64::from(handle), 0x1000)];
        assert_eq!(status_of(&update(&mut session, sync, 5, &operations)), SUCCESS);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 6);
        let mapping = session.allocations[&handle].mappings[0];
        assert_eq!((mapping.address, mapping.first, mapping.length), (0x5000_0000, 0x1000, 0x2000));
    }

    #[test]
    fn mapping_what_is_already_there_changes_nothing() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x4000));
        let (sync, _, _) = create_fence(&mut session, 0);
        let whole = operation(0, 0x5000_0000, 0x4000, u64::from(handle), 0);
        update(&mut session, sync, 0, &[whole]);
        let inner = operation(3, 0x5000_1000, 0x1000, u64::from(handle), 0x1000);
        update(&mut session, sync, 1, &[inner]);
        assert_eq!(session.allocations[&handle].mappings.len(), 1);
    }

    #[test]
    fn an_update_unmaps_the_middle_of_a_mapping() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x3000));
        let (sync, _, _) = create_fence(&mut session, 0);
        update(&mut session, sync, 0, &[operation(0, 0x5000_0000, 0x3000, u64::from(handle), 0)]);
        update(&mut session, sync, 1, &[operation(1, 0x5000_1000, 0x1000, 0, 0)]);
        let mut pieces: Vec<_> =
            session.allocations[&handle].mappings.iter().map(|m| (m.address, m.first)).collect();
        pieces.sort();
        assert_eq!(pieces, [(0x5000_0000, 0), (0x5000_2000, 0x2000)]);
    }

    #[test]
    fn an_update_copies_mappings_and_skips_holes() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x2000));
        let (sync, _, _) = create_fence(&mut session, 0);
        update(&mut session, sync, 0, &[operation(0, 0x5000_1000, 0x1000, u64::from(handle), 0x1000)]);
        update(&mut session, sync, 1, &[operation(2, 0x5000_0000, 0x2000, 0x6000_0000, 0)]);
        let copied = session.allocations[&handle].mappings.iter().find(|m| m.address == 0x6000_1000);
        assert_eq!(copied.map(|m| (m.first, m.length)), Some((0x1000, 0x1000)));
    }

    #[test]
    fn an_update_of_an_unknown_allocation_fails_and_frees_the_guest() {
        let mut session = session();
        let (sync, _, fence) = create_fence(&mut session, 0);
        let reply = update(&mut session, sync, 0, &[operation(0, 0x5000_0000, 0x1000, 0x999, 0)]);
        assert_eq!(reply, Status::UNSUCCESSFUL.0.to_le_bytes());
        assert_eq!(session.memory().read_u64(fence).unwrap(), 1);
    }

    /// A queue with work on the GPU, and an update that will run once its fence gets to 3.
    fn queue_behind_an_update(session: &mut TestSession) -> (u32, u64, u64) {
        session.backend_mut().run_work = true;
        let (_, handle) = allocate(session, private(0x1000));
        let context = create_context(session, 0);
        let (queue, _, queue_fence) = create_queue(session, context);
        submit(session, queue, 7);
        let (sync, _, fence) = create_fence(session, 1);
        let operations = [operation(0, 0x5000_0000, 0x1000, u64::from(handle), 0)];
        assert_eq!(status_of(&update(session, sync, 3, &operations)), SUCCESS);
        (queue, queue_fence, fence)
    }

    #[test]
    fn a_waiting_update_that_fails_takes_the_process_off_the_gpu_before_freeing_it() {
        let mut session = session();
        let (queue, queue_fence, fence) = queue_behind_an_update(&mut session);
        session.backend_mut().fail_next_map = true;
        session.memory_mut().write_u64(fence, 3).unwrap();
        session.poll(Instant::now());
        assert_eq!(session.backend().closed_queues, [queue]);
        assert!(session.queues[&queue].failed);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 4);
        assert_eq!(session.memory().read_u64(queue_fence).unwrap(), 7);
    }

    #[test]
    fn nothing_reaches_the_gpu_after_a_failed_update() {
        let mut session = session();
        let (queue, queue_fence, fence) = queue_behind_an_update(&mut session);
        session.backend_mut().fail_next_map = true;
        session.memory_mut().write_u64(fence, 3).unwrap();
        session.poll(Instant::now());
        let rung = session.backend().doorbells.len();
        let mut request = message(Command::SubmitToHardwareQueue, 0x50);
        set_u32(&mut request, 0x18, queue);
        set_u64(&mut request, 0x20, 8);
        assert_eq!(status_of(&send(&mut session, &request)), Status::UNSUCCESSFUL.0);
        assert_eq!(session.backend().doorbells.len(), rung);
        assert_eq!(session.memory().read_u64(queue_fence).unwrap(), 8);
    }

    #[test]
    fn a_failed_update_does_not_count_towards_stopping_all_work() {
        let mut session = session();
        let (_, _, fence) = queue_behind_an_update(&mut session);
        session.backend_mut().fail_next_map = true;
        session.memory_mut().write_u64(fence, 3).unwrap();
        session.poll(Instant::now());
        assert!(session.failed_queues.is_empty());
        assert!(session.work_allowed());
    }

    #[test]
    fn an_update_that_fails_at_once_stops_the_queues_too() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let (_, handle) = allocate(&mut session, private(0x1000));
        let context = create_context(&mut session, 0);
        let (queue, _, _) = create_queue(&mut session, context);
        let (sync, _, fence) = create_fence(&mut session, 0);
        session.backend_mut().fail_next_map = true;
        let reply = update(&mut session, sync, 0, &[operation(0, 0x5000_0000, 0x1000, u64::from(handle), 0)]);
        assert_eq!(reply, Status::UNSUCCESSFUL.0.to_le_bytes());
        assert!(session.queues[&queue].failed);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 1);
    }

    #[test]
    fn an_update_waits_for_its_fence() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        let (sync, _, fence) = create_fence(&mut session, 1);
        let operations = [operation(0, 0x5000_0000, 0x1000, u64::from(handle), 0)];
        assert_eq!(status_of(&update(&mut session, sync, 3, &operations)), SUCCESS);
        assert!(session.poll(Instant::now()).any());
        assert!(session.allocations[&handle].mappings.is_empty());
        session.memory_mut().write_u64(fence, 3).unwrap();
        assert!(!session.poll(Instant::now()).any());
        assert_eq!(session.allocations[&handle].mappings.len(), 1);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 4);
    }

    #[test]
    fn clearing_addresses_finishes_a_free_that_was_waiting_on_them() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, _) = create_queue(&mut session, context);
        let (resource, old) = allocate(&mut session, private(0x1000));
        map(&mut session, old, 0x5000_0000, 0, 1);
        submit(&mut session, queue, 1);
        destroy(&mut session, resource, &[]);
        assert_eq!(session.deferred.len(), 1);
        let (_, new) = allocate(&mut session, private(0x1000));
        assert_eq!(map(&mut session, new, 0x5000_0000, 0, 1), (0x5000_0000, SUCCESS));
    }
}

mod declared_sizes {
    use super::*;
    use guesthouse_messages::wire::MAX_ANSWER_SIZE;

    const TOO_MANY: u32 = MAX_ANSWER_SIZE as u32 + 1;

    /// A message whose field at `at` asks for more than an answer can hold.
    fn asking_too_much(command: Command, size: usize, at: usize) -> Vec<u8> {
        let mut request = message(command, size);
        set_u32(&mut request, at, TOO_MANY);
        request
    }

    #[test]
    fn a_size_past_the_most_an_answer_can_hold_is_refused() {
        let requests = [
            ("adapter query", asking_too_much(Command::QueryAdapter, 0x30, HEADER_SIZE + 4)),
            ("caps", asking_too_much(Command::QueryDriverCaps, 0x30, HEADER_SIZE + 8)),
            ("escape", asking_too_much(Command::DriverEscape, 0x40, HEADER_SIZE + 16)),
            ("context", asking_too_much(Command::CreateContext, 0x34, 0x30)),
            ("queue", asking_too_much(Command::CreateHardwareQueue, 0x44, 0x40)),
            ("open", asking_too_much(Command::OpenResource, 0x28, 0x24)),
            ("standard", asking_too_much(Command::DescribeStandardAllocation, 0x40, HEADER_SIZE + 4)),
        ];
        for (name, request) in requests {
            let reply = send(&mut session(), &request);
            assert_eq!(reply, Status::UNSUCCESSFUL.0.to_le_bytes(), "{name}");
        }
    }

    #[test]
    fn the_size_of_a_description_query_is_still_honoured_up_to_the_limit() {
        let mut request = message(Command::QueryAdapter, 0x30);
        set_u32(&mut request, HEADER_SIZE, AdapterQuery::DESCRIPTION);
        set_u32(&mut request, HEADER_SIZE + 4, MAX_ANSWER_SIZE as u32);
        assert_eq!(send(&mut session(), &request).len(), 4 + MAX_ANSWER_SIZE);
    }

    #[test]
    fn a_guest_backed_surface_can_not_be_larger_than_the_guest() {
        let mut record = b"GPVS".to_vec();
        record.resize(32, 0);
        set_u32(&mut record, 12, u32::MAX);
        set_u32(&mut record, 20, u32::MAX);
        let reply = send(&mut session(), &create_message(0x20, &[record]));
        assert_eq!(status_of(&reply), Status::NO_MEMORY.0);
    }

    #[test]
    fn a_video_surface_too_big_to_map_is_refused() {
        let mut record = b"GPVS".to_vec();
        record.resize(32, 0);
        set_u32(&mut record, 12, u32::MAX);
        set_u32(&mut record, 20, u32::MAX);
        let reply = send(&mut session(), &create_message(0, &[record]));
        assert_eq!(status_of(&reply), Status::NO_VIDEO_MEMORY.0);
    }

    #[test]
    fn page_lists_stop_at_the_size_of_the_allocation() {
        let mut session = session();
        let reply = send(&mut session, &create_message(0x20, &[private(0x2000)]));
        let handle = u32_at(&reply, 0x10).unwrap();
        let mut request = message(Command::SetGuestPages, 0x28 + 3 * 8);
        set_u32(&mut request, HEADER_SIZE + 4, handle);
        set_u32(&mut request, HEADER_SIZE + 8, 3);
        assert_eq!(status_of(&send(&mut session, &request)), Status::INVALID_PARAMETER.0);
    }
}

mod io_space {
    use super::*;
    use crate::FENCE_PAGE;

    fn set_io_space(session: &mut TestSession, start: u64) -> u32 {
        let mut request = message(Command::SetIoSpace, HEADER_SIZE + 16);
        set_u64(&mut request, HEADER_SIZE, start);
        status_of(&send(session, &request))
    }

    fn monitored_fence(session: &mut TestSession) -> Vec<u8> {
        let mut request = message(Command::CreateSyncObject, 0x30);
        set_u32(&mut request, 0x20, 5);
        send(session, &request)
    }

    #[test]
    fn the_fence_area_must_lie_inside_the_window_on_a_page() {
        let bad = [
            0,
            FENCE_PAGE,
            IO_SPACE - FENCE_PAGE,
            IO_SPACE + 8,
            WINDOW_END - FENCE_AREA + FENCE_PAGE,
            WINDOW_END,
            u64::MAX - FENCE_PAGE + 1,
        ];
        for start in bad {
            let mut session = session();
            assert_eq!(set_io_space(&mut session, start), Status::INVALID_PARAMETER.0, "{start:#x}");
            assert_eq!(u64_at(&monitored_fence(&mut session), 16), Ok(IO_SPACE), "{start:#x}");
        }
    }

    #[test]
    fn the_fence_area_may_end_where_the_window_does() {
        let mut session = session();
        assert_eq!(set_io_space(&mut session, WINDOW_END - FENCE_AREA), SUCCESS);
        assert_eq!(u64_at(&monitored_fence(&mut session), 16), Ok(WINDOW_END - FENCE_AREA));
    }

    #[test]
    fn nothing_reaches_the_gpu_before_the_guest_places_its_io_space() {
        let backend = StandIn::new();
        let limits = MemoryLimits::new(backend.facts(), None, 1 << 30);
        let memory = SparseMemory::new(WINDOW_END);
        let mut session = Session::new(backend, memory, Config::new(limits));
        let reply = monitored_fence(&mut session);
        assert_eq!(status_of(&reply), Status::UNSUCCESSFUL.0);
        assert_eq!(reply.len(), 4);
        assert_eq!(session.backend.live_memory_blocks(), 0);
    }

    #[test]
    fn a_repeat_of_the_same_place_is_fine_but_a_move_comes_too_late_once_fences_exist() {
        let mut session = session();
        monitored_fence(&mut session);
        assert_eq!(set_io_space(&mut session, IO_SPACE), SUCCESS);
        assert_eq!(set_io_space(&mut session, IO_SPACE + FENCE_AREA), Status::INVALID_PARAMETER.0);
    }
}

mod fences {
    use super::*;

    #[test]
    fn monitored_fences_start_at_the_value_asked() {
        let mut session = session();
        let (_, gpu_address, fence) = create_fence(&mut session, 7);
        assert_eq!(fence, IO_SPACE);
        assert_eq!(gpu_address, crate::GPU_RANGES[0].start);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 7);
    }

    #[test]
    fn each_fence_gets_its_own_page_in_the_block() {
        let mut session = session();
        let (_, first_gpu, first) = create_fence(&mut session, 0);
        let (_, second_gpu, second) = create_fence(&mut session, 0);
        assert_eq!(second - first, crate::FENCE_PAGE);
        assert_eq!(second_gpu - first_gpu, crate::FENCE_PAGE);
    }

    #[test]
    fn a_freed_fence_comes_back_zeroed() {
        let mut session = session();
        let (handle, _, fence) = create_fence(&mut session, 9);
        destroy_sync(&mut session, handle);
        let (_, _, again) = create_fence(&mut session, 0);
        assert_eq!(again, fence);
        assert_eq!(session.memory().read_u64(again).unwrap(), 0);
    }

    #[test]
    fn a_fence_stays_until_its_last_handle() {
        let mut session = session();
        let (handle, _, fence) = create_fence(&mut session, 0);
        let mut open = message(Command::OpenSyncObject, 0x20);
        set_u32(&mut open, 0x1C, handle);
        let reply = send(&mut session, &open);
        assert_eq!(u32_at(&reply, 4), Ok(SUCCESS));
        assert_eq!(u64_at(&reply, 16), Ok(fence));
        destroy_sync(&mut session, handle);
        assert_ne!(create_fence(&mut session, 0).2, fence);
        destroy_sync(&mut session, u32_at(&reply, 0).unwrap());
        assert_eq!(create_fence(&mut session, 0).2, fence);
    }

    #[test]
    fn opening_an_unknown_sync_object_is_refused() {
        let mut open = message(Command::OpenSyncObject, 0x20);
        set_u32(&mut open, 0x1C, 0x999);
        assert_eq!(u32_at(&send(&mut session(), &open), 4), Ok(Status::INVALID_PARAMETER.0));
    }

    #[test]
    fn fences_only_go_up() {
        let mut session = session();
        let (handle, _, fence) = create_fence(&mut session, 0);
        send(&mut session, &signal_message(&[handle], &[], &[5]));
        assert_eq!(session.memory().read_u64(fence).unwrap(), 5);
        send(&mut session, &signal_message(&[handle], &[], &[3]));
        assert_eq!(session.memory().read_u64(fence).unwrap(), 5);
    }

    #[test]
    fn a_signal_reaches_a_fence_through_a_shared_handle() {
        let mut session = session();
        let (handle, _, fence) = create_fence(&mut session, 0);
        let mut request = message(Command::CreateSharedObject, 0x20);
        set_u32(&mut request, 0x18, handle);
        let shared = status_of(&send(&mut session, &request));
        let mut signal = message(Command::SignalFence, 0x28);
        set_u32(&mut signal, 0x18, shared);
        set_u64(&mut signal, 0x20, 8);
        send(&mut session, &signal);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 8);
    }

    fn notification(session: &mut TestSession, event: u64) -> u32 {
        let mut request = message(Command::CreateSyncObject, 0x30);
        set_u32(&mut request, 0x20, 4);
        set_u64(&mut request, 0x28, event);
        status_of(&send(session, &request))
    }

    #[test]
    fn a_notification_wakes_its_event_even_with_no_values() {
        let mut session = session();
        let handle = notification(&mut session, 0xE1);
        send(&mut session, &signal_message(&[handle], &[], &[]));
        let notices: Vec<_> = session.take_notices().collect();
        assert_eq!(notices, [Notice::WakeEvent { event: 0xE1, release: 0 }]);
    }

    fn wait_message(objects: &[u32], values: &[u64], any: bool, event: u64) -> Vec<u8> {
        let mut request = message(Command::WaitOnCpu, 0x38);
        set_u32(&mut request, 0x1C, objects.len() as u32);
        set_u32(&mut request, 0x20, u32::from(any));
        set_u64(&mut request, 0x28, event);
        request[0x30] = 1;
        objects.iter().for_each(|object| request.extend_from_slice(&object.to_le_bytes()));
        values.iter().for_each(|value| request.extend_from_slice(&value.to_le_bytes()));
        request
    }

    #[test]
    fn a_wait_already_satisfied_wakes_at_once() {
        let mut session = session();
        let (handle, _, _) = create_fence(&mut session, 5);
        send(&mut session, &wait_message(&[handle], &[5], false, 0xE));
        let notices: Vec<_> = session.take_notices().collect();
        assert_eq!(notices, [Notice::WakeEvent { event: 0xE, release: 1 }]);
    }

    #[test]
    fn a_wait_on_all_needs_every_fence() {
        let mut session = session();
        let (first, _, first_fence) = create_fence(&mut session, 0);
        let (second, _, second_fence) = create_fence(&mut session, 0);
        send(&mut session, &wait_message(&[first, second], &[1, 1], false, 0xE));
        session.memory_mut().write_u64(first_fence, 1).unwrap();
        assert!(session.poll(Instant::now()).any());
        assert_eq!(session.take_notices().count(), 0);
        session.memory_mut().write_u64(second_fence, 1).unwrap();
        assert!(!session.poll(Instant::now()).any());
        assert_eq!(session.take_notices().count(), 1);
    }

    #[test]
    fn a_wait_on_any_needs_one_fence() {
        let mut session = session();
        let (first, _, fence) = create_fence(&mut session, 0);
        let (second, _, _) = create_fence(&mut session, 0);
        send(&mut session, &wait_message(&[first, second], &[1, 1], true, 0xE));
        assert_eq!(session.take_notices().count(), 0);
        session.memory_mut().write_u64(fence, 1).unwrap();
        let _ = session.poll(Instant::now());
        assert_eq!(session.take_notices().count(), 1);
    }

    #[test]
    fn an_unknown_object_counts_as_reached() {
        let mut session = session();
        send(&mut session, &wait_message(&[0x999], &[100], false, 0xE));
        assert_eq!(session.take_notices().count(), 1);
    }

    #[test]
    fn a_queues_progress_object_can_be_waited_on() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, progress, fence) = create_queue(&mut session, context);
        submit(&mut session, queue, 2);
        send(&mut session, &wait_message(&[progress], &[2], false, 0xE));
        assert_eq!(session.take_notices().count(), 0);
        session.memory_mut().write_u64(fence, 2).unwrap();
        let _ = session.poll(Instant::now());
        assert_eq!(session.take_notices().count(), 1);
    }

    #[test]
    fn paging_queues_reuse_their_fences_apart_from_the_others() {
        let mut session = session();
        let reply = send(&mut session, &message(Command::CreatePagingQueue, HEADER_SIZE));
        assert_eq!(reply.len(), 24);
        let (queue, fence) = (status_of(&reply), u64_at(&reply, 8).unwrap());
        session.memory_mut().write_u64(fence, 3).unwrap();
        let mut destroy = message(Command::DestroyPagingQueue, 0x20);
        set_u32(&mut destroy, 0x18, queue);
        send(&mut session, &destroy);
        assert_ne!(create_fence(&mut session, 0).2, fence);
        let reply = send(&mut session, &message(Command::CreatePagingQueue, HEADER_SIZE));
        assert_eq!(u64_at(&reply, 8), Ok(fence));
        assert_eq!(session.memory().read_u64(fence).unwrap(), 0);
    }
}

mod queues {
    use super::*;

    #[test]
    fn a_queue_the_vendor_refuses_leaves_nothing_behind() {
        let mut session = session();
        let context = create_context(&mut session, 0);
        session.backend_mut().refuse_private_data = true;
        let mut request = message(Command::CreateHardwareQueue, 0x48);
        set_u32(&mut request, 0x38, context);
        set_u32(&mut request, 0x40, 4);
        let reply = send(&mut session, &request);
        assert_ne!(u32_at(&reply, 0), Ok(SUCCESS));
        assert!(session.queues.is_empty());
        assert!(session.queue_progress.is_empty());
        assert!(session.context_queues.values().all(Vec::is_empty));
        session.backend_mut().refuse_private_data = false;
        create_queue(&mut session, context);
        assert_eq!(session.queues.len(), 1);
    }

    #[test]
    fn the_context_answer_is_its_message_with_a_handle() {
        let mut session = session();
        let context = create_context(&mut session, 3);
        assert_eq!(session.context_nodes[&context], 3);
    }

    #[test]
    fn a_queue_answer_has_its_handles_and_vendor_data() {
        let mut session = session();
        let context = create_context(&mut session, 0);
        let mut request = message(Command::CreateHardwareQueue, 0x48);
        set_u32(&mut request, 0x38, context);
        set_u32(&mut request, 0x40, 4);
        let reply = send(&mut session, &request);
        assert_eq!(u32_at(&reply, 0x20).unwrap() - u32_at(&reply, 0x1C).unwrap(), 0x40);
        assert_eq!(reply[0x44], 1, "the vendor saw the first queue's serial");
        assert_eq!(u32_at(&reply, 0x24), Ok(0));
    }

    #[test]
    fn work_that_does_not_reach_the_gpu_completes_its_fence() {
        let mut session = session();
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        submit(&mut session, queue, 9);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 9);
    }

    #[test]
    fn work_on_the_gpu_leaves_the_fence_to_it() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        submit(&mut session, queue, 9);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 0);
        assert_eq!(session.backend().doorbells, [(queue, 9)]);
    }

    #[test]
    fn a_backend_can_take_gpu_addresses_for_its_channels() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, _) = create_queue(&mut session, context);
        submit(&mut session, queue, 1);
        let taken = session.backend().channel_address.unwrap();
        let (_, handle) = allocate(&mut session, private(0x1000));
        assert_eq!(map(&mut session, handle, 0, 0, 0).0, taken + 0x10000);
    }

    #[test]
    fn submitting_to_an_unknown_queue_succeeds_and_does_nothing() {
        submit(&mut session(), 0x999, 1);
    }

    #[test]
    fn a_context_without_a_queue_gets_one() {
        let mut session = session();
        let context = create_context(&mut session, 0);
        let mut request = message(Command::SubmitToContext, 0x198);
        set_u32(&mut request, 0x34, context);
        assert_eq!(status_of(&send(&mut session, &request)), SUCCESS);
        let queue = session.context_queues[&context][0];
        assert_eq!(session.memory().read_u64(session.queues[&queue].fence).unwrap(), 1);
        assert_eq!(status_of(&send(&mut session, &request)), SUCCESS);
        assert_eq!(session.context_queues[&context], [queue]);
    }

    fn gpu_wait(session: &mut TestSession, handle: u32, object: u32, value: u64) {
        let mut request = message(Command::WaitOnGpu, 0x28);
        set_u32(&mut request, 0x18, handle);
        set_u32(&mut request, 0x1C, 1);
        request.extend_from_slice(&value.to_le_bytes());
        request.extend_from_slice(&object.to_le_bytes());
        assert_eq!(status_of(&send(session, &request)), SUCCESS);
    }

    #[test]
    fn a_doorbell_that_fails_stands_in_at_the_fence_and_stops_the_queue() {
        let mut session = session();
        session.backend_mut().run_work = true;
        session.backend_mut().fail_rings = true;
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        submit(&mut session, queue, 3);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 3);
        assert!(session.queues[&queue].failed);
        assert!(session.backend().doorbells.is_empty());
        session.backend_mut().fail_rings = false;
        let mut request = message(Command::SubmitToHardwareQueue, 0x50);
        set_u32(&mut request, 0x18, queue);
        set_u64(&mut request, 0x20, 4);
        send(&mut session, &request);
        assert!(session.backend().doorbells.is_empty(), "a failed queue runs nothing more");
        assert_eq!(session.memory().read_u64(fence).unwrap(), 4);
    }

    #[test]
    fn a_gate_holds_doorbells_until_its_fence_moves() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, _) = create_queue(&mut session, context);
        let (sync, _, fence) = create_fence(&mut session, 0);
        gpu_wait(&mut session, context, sync, 4);
        submit(&mut session, queue, 1);
        assert!(session.backend().doorbells.is_empty());
        assert!(session.poll(Instant::now()).any());
        session.memory_mut().write_u64(fence, 4).unwrap();
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().doorbells, [(queue, 1)]);
        assert!(session.gates.is_empty());
    }

    /// A mutex that starts free. Returns its handle.
    fn create_mutex(session: &mut TestSession) -> u32 {
        let mut request = message(Command::CreateSyncObject, 0x30);
        set_u32(&mut request, 0x20, guesthouse_messages::sync::KIND_MUTEX);
        set_u64(&mut request, 0x28, 1);
        u32_at(&send(session, &request), 0).unwrap()
    }

    fn signal_mutex(session: &mut TestSession, mutex: u32) {
        let mut request = message(Command::SignalSyncObject, 0x38);
        set_u32(&mut request, 0x18, 1);
        request.extend_from_slice(&mutex.to_le_bytes());
        assert_eq!(status_of(&send(session, &request)), SUCCESS);
    }

    #[test]
    fn a_mutex_lets_one_context_through_until_it_is_signalled() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let (first, second) = (create_context(&mut session, 6), create_context(&mut session, 7));
        let (first_queue, _, _) = create_queue(&mut session, first);
        let (second_queue, _, _) = create_queue(&mut session, second);
        let mutex = create_mutex(&mut session);
        gpu_wait(&mut session, first, mutex, 0);
        gpu_wait(&mut session, second, mutex, 0);
        submit(&mut session, first_queue, 1);
        submit(&mut session, second_queue, 1);
        assert_eq!(session.backend().doorbells, [(first_queue, 1)], "the second is held");
        signal_mutex(&mut session, mutex);
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().doorbells, [(first_queue, 1), (second_queue, 1)]);
    }

    #[test]
    fn engines_get_a_mutex_in_the_order_they_asked() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let contexts = [6, 7, 8].map(|node| create_context(&mut session, node));
        let queues = contexts.map(|context| create_queue(&mut session, context).0);
        let mutex = create_mutex(&mut session);
        contexts.iter().for_each(|&context| gpu_wait(&mut session, context, mutex, 0));
        queues.iter().for_each(|&queue| submit(&mut session, queue, 1));
        assert_eq!(session.backend().doorbells, [(queues[0], 1)]);
        signal_mutex(&mut session, mutex);
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().doorbells, [(queues[0], 1), (queues[1], 1)]);
        signal_mutex(&mut session, mutex);
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().doorbells.last(), Some(&(queues[2], 1)));
    }

    #[test]
    fn a_wait_on_a_context_takes_a_mutex_once_for_all_its_queues() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let queues = [(); 3].map(|_| create_queue(&mut session, context).0);
        let mutex = create_mutex(&mut session);
        gpu_wait(&mut session, context, mutex, 0);
        queues.iter().for_each(|&queue| submit(&mut session, queue, 1));
        assert_eq!(session.backend().doorbells, queues.map(|queue| (queue, 1)));
        assert!(session.gates.is_empty());
        assert!(session.shared_waits.is_empty());
    }

    #[test]
    fn a_context_waiting_on_a_held_mutex_lets_all_its_queues_go_on_one_signal() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let (holder, waiter) = (create_context(&mut session, 6), create_context(&mut session, 0));
        create_queue(&mut session, holder);
        let queues = [(); 2].map(|_| create_queue(&mut session, waiter).0);
        let mutex = create_mutex(&mut session);
        gpu_wait(&mut session, holder, mutex, 0);
        gpu_wait(&mut session, waiter, mutex, 0);
        queues.iter().for_each(|&queue| submit(&mut session, queue, 1));
        assert!(session.backend().doorbells.is_empty());
        signal_mutex(&mut session, mutex);
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().doorbells, queues.map(|queue| (queue, 1)));
        assert!(session.shared_waits.is_empty());
        let object = session.sync_objects.get(mutex).unwrap();
        assert_eq!(object.tokens.unwrap().free, 0, "the waiter holds it once");
    }

    #[test]
    fn a_closed_queue_leaves_a_shared_wait_to_the_others() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let (holder, waiter) = (create_context(&mut session, 6), create_context(&mut session, 0));
        create_queue(&mut session, holder);
        let queues = [(); 2].map(|_| create_queue(&mut session, waiter).0);
        let mutex = create_mutex(&mut session);
        gpu_wait(&mut session, holder, mutex, 0);
        gpu_wait(&mut session, waiter, mutex, 0);
        session.close_queue(queues[0]);
        submit(&mut session, queues[1], 1);
        signal_mutex(&mut session, mutex);
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().doorbells, [(queues[1], 1)]);
        assert!(session.shared_waits.is_empty());
    }

    #[test]
    fn a_held_mutex_is_given_up_on_at_the_gates_time_limit() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 6);
        let (queue, _, _) = create_queue(&mut session, context);
        let mutex = create_mutex(&mut session);
        gpu_wait(&mut session, context, mutex, 0);
        gpu_wait(&mut session, context, mutex, 0);
        submit(&mut session, queue, 1);
        assert!(session.backend().doorbells.is_empty());
        let _ = session.poll(Instant::now() + crate::pending::GATE_TIMEOUT + Duration::from_secs(1));
        assert_eq!(session.backend().doorbells, [(queue, 1)]);
    }

    #[test]
    fn a_wait_on_a_context_with_no_queue_yet_still_holds_its_first_work() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 6);
        let (sync, _, fence) = create_fence(&mut session, 0);
        gpu_wait(&mut session, context, sync, 4);
        assert_eq!(session.context_queues[&context].len(), 1);
        assert!(session.gates.contains_key(&session.context_queues[&context][0]));
        session.memory_mut().write_u64(fence, 4).unwrap();
        let _ = session.poll(Instant::now());
        assert!(session.gates.is_empty());
    }

    #[test]
    fn a_wait_already_reached_shuts_no_gate() {
        let mut session = session();
        let context = create_context(&mut session, 0);
        create_queue(&mut session, context);
        let (sync, _, _) = create_fence(&mut session, 4);
        gpu_wait(&mut session, context, sync, 4);
        assert!(session.gates.is_empty());
    }

    #[test]
    fn a_gate_gives_up_at_its_deadline() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, _) = create_queue(&mut session, context);
        let (sync, _, _) = create_fence(&mut session, 0);
        gpu_wait(&mut session, context, sync, 4);
        submit(&mut session, queue, 1);
        let _ = session.poll(Instant::now() + crate::pending::GATE_TIMEOUT + Duration::from_secs(1));
        assert_eq!(session.backend().doorbells, [(queue, 1)]);
    }

    #[test]
    fn a_signal_after_gpu_work_waits_for_that_work() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, queue_fence) = create_queue(&mut session, context);
        let (sync, _, fence) = create_fence(&mut session, 0);
        submit(&mut session, queue, 1);
        send(&mut session, &signal_message(&[sync], &[context], &[7]));
        assert_eq!(session.memory().read_u64(fence).unwrap(), 0);
        session.memory_mut().write_u64(queue_fence, 1).unwrap();
        let _ = session.poll(Instant::now());
        assert_eq!(session.memory().read_u64(fence).unwrap(), 7);
    }

    #[test]
    fn a_present_token_comes_back_when_the_gpu_gets_there() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, queue_fence) = create_queue(&mut session, context);
        submit(&mut session, queue, 1);
        let mut request = message(Command::PresentToken, 0x38);
        set_u32(&mut request, 0x18, context);
        set_u64(&mut request, 0x20, 1);
        set_u64(&mut request, 0x28, 2);
        set_u64(&mut request, 0x30, 3);
        send(&mut session, &request);
        assert_eq!(session.take_notices().count(), 0);
        session.memory_mut().write_u64(queue_fence, 1).unwrap();
        let _ = session.poll(Instant::now());
        let notices: Vec<_> = session.take_notices().collect();
        assert_eq!(notices, [Notice::PresentDone { token: [1, 2, 3] }]);
    }

    #[test]
    fn a_cpu_event_signal_wakes_with_release_and_leaves_the_objects() {
        let mut session = session();
        let (sync, _, fence) = create_fence(&mut session, 0);
        let mut request = signal_message(&[sync], &[], &[5]);
        set_u32(&mut request, 0x1C, 2);
        set_u64(&mut request, 0x30, 0xE);
        send(&mut session, &request);
        assert_eq!(session.memory().read_u64(fence).unwrap(), 0);
        let notices: Vec<_> = session.take_notices().collect();
        assert_eq!(notices, [Notice::WakeEvent { event: 0xE, release: 1 }]);
    }

    #[test]
    fn freeing_waits_for_the_gpu() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, queue_fence) = create_queue(&mut session, context);
        let (resource, _) = allocate(&mut session, private(0x1000));
        submit(&mut session, queue, 1);
        let blocks = session.backend().live_memory_blocks();
        destroy(&mut session, resource, &[]);
        assert_eq!(session.backend().live_memory_blocks(), blocks, "the GPU may still be using it");
        session.memory_mut().write_u64(queue_fence, 1).unwrap();
        submit(&mut session, queue, 1);
        assert_eq!(session.backend().live_memory_blocks(), blocks - 1);
    }

    #[test]
    fn a_stuck_queue_times_out_and_frees_the_guest() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        submit(&mut session, queue, 3);
        assert!(session.poll(Instant::now()).any());
        assert!(!session.poll(Instant::now() + crate::pending::QUEUE_TIMEOUT + Duration::from_secs(1)).any());
        assert_eq!(session.memory().read_u64(fence).unwrap(), 3);
        assert!(session.queues[&queue].failed);
    }

    #[test]
    fn destroying_a_context_closes_its_queues_and_frees_their_fences() {
        let mut session = session();
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        let mut request = message(Command::DestroyContext, 0x20);
        set_u32(&mut request, 0x18, context);
        send(&mut session, &request);
        assert_eq!(session.backend().closed_queues, [queue]);
        assert_eq!(create_fence(&mut session, 0).2, fence);
    }

    #[test]
    fn a_dead_process_loses_its_address_space_at_the_next_deferred_run() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        map(&mut session, handle, 0, 0, 0);
        assert_eq!(session.backend().live_address_spaces(), 1);
        send(&mut session, &message(Command::DestroyProcess, HEADER_SIZE));
        assert_eq!(session.backend().live_address_spaces(), 1);
        submit(&mut session, 0x999, 1);
        assert_eq!(session.backend().live_address_spaces(), 0);
        assert!(session.allocations[&handle].mappings.is_empty());
    }

    fn copy_message(context: u32, source: u32, target: u32, private_size: u32) -> Vec<u8> {
        let mut request = message(Command::CopyToStaging, 0x50);
        set_u32(&mut request, 0x1C, context);
        set_u32(&mut request, 0x20, source);
        set_u32(&mut request, 0x24, target);
        set_u32(&mut request, 0x30, 640);
        set_u32(&mut request, 0x34, 480);
        set_u32(&mut request, 0x4C, private_size);
        request
    }

    #[test]
    fn a_copy_needs_two_known_surfaces() {
        let mut session = session();
        let (_, buffer) = allocate(&mut session, private(0x1000));
        let (_, surface) = allocate(&mut session, surface_private(640, 480));
        let reply = send(&mut session, &copy_message(0, surface, buffer, 0));
        assert_eq!(status_of(&reply), Status::INVALID_PARAMETER.0);
    }

    #[test]
    fn a_copy_maps_both_surfaces_once_in_the_daemons_space() {
        let mut session = session();
        let (_, source) = allocate(&mut session, surface_private(640, 480));
        let (_, target) = allocate(&mut session, surface_private(640, 480));
        assert_eq!(status_of(&send(&mut session, &copy_message(0, source, target, 0))), SUCCESS);
        assert_eq!(status_of(&send(&mut session, &copy_message(0, source, target, 0))), SUCCESS);
        let copies = &session.backend().copies;
        assert_eq!(copies.len(), 2);
        assert_eq!(copies[0].source_address, copies[1].source_address);
        assert_ne!(copies[0].source_address, copies[0].target_address);
        assert_eq!(session.allocations[&source].mappings.len(), 1);
        assert!(!copies[0].is_screen, "the target is not in the shared window");
    }

    #[test]
    fn a_copy_never_shares_an_address_space_with_a_process() {
        let mut session = session();
        let (_, source) = allocate(&mut session, surface_private(640, 480));
        let (_, target) = allocate(&mut session, surface_private(640, 480));
        let mut request = message(Command::MapGpuAddresses, 0x50);
        set_u32(&mut request, 8, 0);
        set_u64(&mut request, 0x20, 0x2000_0000);
        set_u32(&mut request, 0x38, source);
        set_u64(&mut request, 0x40, 1);
        set_u64(&mut request, 0x48, 1);
        assert_eq!(u32_at(&send(&mut session, &request), 16).unwrap(), SUCCESS);
        assert_eq!(status_of(&send(&mut session, &copy_message(0, source, target, 0))), SUCCESS);
        let address = session.backend().copies[0].source_address;
        let allocation = &session.allocations[&source];
        let mapping = allocation.mappings.iter().find(|mapping| mapping.address == address).unwrap();
        assert_eq!((mapping.first, mapping.length), (0, allocation.size), "the whole surface");
    }

    #[test]
    fn the_screen_is_a_locked_target_with_no_driver_data() {
        let mut session = session();
        let (_, source) = allocate(&mut session, surface_private(640, 480));
        let (_, target) = allocate(&mut session, surface_private(640, 480));
        lock(&mut session, target);
        send(&mut session, &copy_message(0, source, target, 0));
        send(&mut session, &copy_message(0, source, target, 8));
        let copies = &session.backend().copies;
        assert!(copies[0].is_screen);
        assert!(!copies[1].is_screen);
    }
}

mod held_replies {
    use super::*;

    const TICKET: ReplyTicket = ReplyTicket { channel: 1, transaction: 0x77 };

    fn send_with_ticket(session: &mut TestSession, message: &[u8]) -> (Answer, Vec<u8>) {
        let mut out = Vec::new();
        let answer = session.handle_message(message, Some(TICKET), &mut out);
        (answer, out)
    }

    fn submission(queue: u32, value: u64) -> Vec<u8> {
        let mut request = message(Command::SubmitToHardwareQueue, 0x50);
        set_u32(&mut request, 0x18, queue);
        set_u64(&mut request, 0x20, value);
        request
    }

    fn busy_queue(limit: u64) -> (TestSession, u32, u64) {
        let mut session = session();
        let backend = session.backend_mut();
        backend.run_work = true;
        backend.work_per_submission = 100;
        backend.unfinished_limit = Some(limit);
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        (session, queue, fence)
    }

    #[test]
    fn a_ring_with_room_answers_at_once() {
        let (mut session, queue, _) = busy_queue(100);
        assert_eq!(send_with_ticket(&mut session, &submission(queue, 1)).0, Answer::Now);
    }

    #[test]
    fn a_full_ring_holds_the_reply_until_the_gpu_catches_up() {
        let (mut session, queue, fence) = busy_queue(100);
        let _ = send_with_ticket(&mut session, &submission(queue, 1));
        assert_eq!(send_with_ticket(&mut session, &submission(queue, 2)).0, Answer::Held);
        assert_eq!(session.poll(Instant::now()), Waiting::Urgent);
        assert_eq!(session.take_replies().count(), 0);
        session.memory_mut().write_u64(fence, 1).unwrap();
        let _ = session.poll(Instant::now());
        let replies: Vec<_> = session.take_replies().collect();
        assert_eq!(replies, [HeldReply { ticket: TICKET, bytes: SUCCESS.to_le_bytes().to_vec() }]);
    }

    #[test]
    fn a_hold_leaves_other_messages_free_to_run() {
        let (mut session, queue, _) = busy_queue(100);
        let _ = send_with_ticket(&mut session, &submission(queue, 1));
        assert_eq!(send_with_ticket(&mut session, &submission(queue, 2)).0, Answer::Held);
        let (answer, reply) = send_with_ticket(&mut session, &message(Command::CreateDevice, HEADER_SIZE));
        assert_eq!(answer, Answer::Now);
        assert_eq!(reply.len(), 4);
    }

    #[test]
    fn a_hold_is_bounded() {
        let (mut session, queue, _) = busy_queue(100);
        let _ = send_with_ticket(&mut session, &submission(queue, 1));
        let _ = send_with_ticket(&mut session, &submission(queue, 2));
        let later = Instant::now() + crate::pending::RING_ROOM_TIMEOUT + Duration::from_millis(1);
        let _ = session.poll(later);
        assert_eq!(session.take_replies().count(), 1);
    }

    #[test]
    fn a_closed_queue_lets_its_held_reply_go() {
        let (mut session, queue, _) = busy_queue(100);
        let _ = send_with_ticket(&mut session, &submission(queue, 1));
        let _ = send_with_ticket(&mut session, &submission(queue, 2));
        let mut request = message(Command::DestroyHardwareQueue, 0x20);
        set_u32(&mut request, 0x18, queue);
        send(&mut session, &request);
        let _ = session.poll(Instant::now());
        assert_eq!(session.take_replies().count(), 1);
    }

    #[test]
    fn a_guest_that_wants_no_reply_is_not_held() {
        let (mut session, queue, _) = busy_queue(100);
        let mut out = Vec::new();
        let _ = session.handle_message(&submission(queue, 1), None, &mut out);
        assert_eq!(session.handle_message(&submission(queue, 2), None, &mut out), Answer::Now);
    }

    #[test]
    fn only_the_watch_on_the_gpu_is_not_urgent() {
        let (mut session, queue, _) = busy_queue(1000);
        let _ = send_with_ticket(&mut session, &submission(queue, 1));
        assert_eq!(session.poll(Instant::now()), Waiting::Background);
    }

    fn surfaces(session: &mut TestSession) -> (u32, u32, u32, u32) {
        session.set_copy_order(crate::CopyOrder::Held);
        let (source_resource, source) = allocate(session, surface_private(640, 480));
        let (target_resource, target) = allocate(session, surface_private(640, 480));
        (source_resource, source, target_resource, target)
    }

    fn copy(context: u32, source: u32, target: u32) -> Vec<u8> {
        let mut request = message(Command::CopyToStaging, 0x50);
        set_u32(&mut request, 0x1C, context);
        set_u32(&mut request, 0x20, source);
        set_u32(&mut request, 0x24, target);
        request
    }

    #[test]
    fn a_copy_that_takes_time_holds_its_reply() {
        let mut session = session();
        session.backend_mut().slow_copies = true;
        let (_, source, _, target) = surfaces(&mut session);
        assert_eq!(send_with_ticket(&mut session, &copy(0, source, target)).0, Answer::Held);
        assert_eq!(session.poll(Instant::now()), Waiting::Urgent);
        session.backend_mut().copy_done = true;
        let _ = session.poll(Instant::now());
        assert_eq!(session.take_replies().count(), 1);
    }

    #[test]
    fn a_copy_waits_for_its_frame_without_stopping_anything() {
        let mut session = session();
        session.backend_mut().run_work = true;
        session.config.frame_wait = Duration::from_secs(1);
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        let (_, source, _, target) = surfaces(&mut session);
        submit(&mut session, queue, 1);
        assert_eq!(send_with_ticket(&mut session, &copy(context, source, target)).0, Answer::Held);
        assert!(session.backend().copies.is_empty());
        let _ = session.poll(Instant::now());
        assert!(session.backend().copies.is_empty(), "the frame is not done");
        session.memory_mut().write_u64(fence, 1).unwrap();
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().copies.len(), 1);
        assert_eq!(session.take_replies().count(), 1);
    }

    #[test]
    fn a_frame_that_never_finishes_is_copied_anyway() {
        let mut session = session();
        session.backend_mut().run_work = true;
        session.config.frame_wait = Duration::from_millis(1);
        let context = create_context(&mut session, 0);
        let (queue, _, _) = create_queue(&mut session, context);
        let (_, source, _, target) = surfaces(&mut session);
        submit(&mut session, queue, 1);
        let _ = send_with_ticket(&mut session, &copy(context, source, target));
        let _ = session.poll(Instant::now() + Duration::from_secs(1));
        assert_eq!(session.backend().copies.len(), 1);
    }

    #[test]
    fn the_next_frame_waits_behind_a_copy_of_the_last() {
        let mut session = session();
        session.backend_mut().run_work = true;
        session.backend_mut().slow_copies = true;
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        let (_, source, _, target) = surfaces(&mut session);
        submit(&mut session, queue, 1);
        session.memory_mut().write_u64(fence, 1).unwrap();
        let _ = send_with_ticket(&mut session, &copy(context, source, target));
        submit(&mut session, queue, 2);
        assert_eq!(session.backend().doorbells.len(), 1, "the gate holds the second");
        session.backend_mut().copy_done = true;
        let _ = session.poll(Instant::now());
        let _ = session.poll(Instant::now());
        assert_eq!(session.backend().doorbells.len(), 2);
    }

    #[test]
    fn a_surface_in_use_is_freed_only_after_its_copy() {
        let mut session = session();
        session.backend_mut().slow_copies = true;
        let (resource, source, _, target) = surfaces(&mut session);
        let _ = send_with_ticket(&mut session, &copy(0, source, target));
        let mut destroy = message(Command::DestroyAllocation, 0x28);
        set_u32(&mut destroy, 0x1C, resource);
        assert_eq!(send_with_ticket(&mut session, &destroy).0, Answer::Held);
        assert!(session.allocations.contains_key(&source));
        session.backend_mut().copy_done = true;
        let _ = session.poll(Instant::now());
        assert!(!session.allocations.contains_key(&source));
        assert_eq!(session.take_replies().count(), 2, "the copy's and the destroy's");
    }

    #[test]
    fn locking_a_surface_in_use_waits_for_its_copy() {
        let mut session = session();
        session.backend_mut().slow_copies = true;
        let (_, source, _, target) = surfaces(&mut session);
        let _ = send_with_ticket(&mut session, &copy(0, source, target));
        let mut lock = message(Command::LockForCpu, 0x20);
        set_u32(&mut lock, 0x1C, target);
        assert_eq!(send_with_ticket(&mut session, &lock).0, Answer::Held);
        session.backend_mut().copy_done = true;
        let _ = session.poll(Instant::now());
        let replies: Vec<_> = session.take_replies().collect();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1].bytes.len(), 16, "the lock's own answer");
    }

    #[test]
    fn the_blocking_order_answers_at_once_and_runs_on_this_thread() {
        let mut session = session();
        let (_, source, _, target) = surfaces(&mut session);
        session.set_copy_order(crate::CopyOrder::Blocking);
        session.backend_mut().slow_copies = true;
        session.backend_mut().copy_done = true;
        let (answer, reply) = send_with_ticket(&mut session, &copy(0, source, target));
        assert_eq!((answer, reply), (Answer::Now, SUCCESS.to_le_bytes().to_vec()));
    }

    #[test]
    fn a_rectangle_the_wrong_way_up_or_too_big_is_refused() {
        let mut session = session();
        let (_, source, _, target) = surfaces(&mut session);
        session.set_copy_order(crate::CopyOrder::Blocking);
        for (left, right) in [(10i32, 5i32), (-1, 5), (0, 100_000)] {
            let mut request = copy(0, source, target);
            request[0x28..0x2C].copy_from_slice(&left.to_le_bytes());
            request[0x30..0x34].copy_from_slice(&right.to_le_bytes());
            assert_eq!(send(&mut session, &request), Status::UNSUCCESSFUL.0.to_le_bytes(), "{left} {right}");
        }
        assert!(session.backend().copies.is_empty());
    }
}

mod address_reuse {
    use super::*;

    fn reserve(session: &mut TestSession, size: u64) -> u64 {
        let mut request = message(Command::ReserveGpuAddresses, 0x40);
        set_u64(&mut request, 0x38, size);
        u64_at(&send(session, &request), 0).unwrap()
    }

    fn free(session: &mut TestSession, address: u64) {
        let mut request = message(Command::FreeGpuAddresses, 0x30);
        set_u64(&mut request, 0x20, address);
        assert_eq!(status_of(&send(session, &request)), SUCCESS);
    }

    #[test]
    fn a_range_given_back_is_reserved_again() {
        let mut session = session();
        let first = reserve(&mut session, 0x10000);
        free(&mut session, first);
        assert_eq!(reserve(&mut session, 0x10000), first);
    }

    #[test]
    fn a_range_of_another_size_stays_apart() {
        let mut session = session();
        let first = reserve(&mut session, 0x10000);
        free(&mut session, first);
        assert_ne!(reserve(&mut session, 0x20000), first);
    }

    #[test]
    fn freeing_what_was_not_reserved_changes_nothing() {
        let mut session = session();
        free(&mut session, 0x5000_0000);
        assert!(session.reserved_addresses.is_empty());
    }

    #[test]
    fn a_range_comes_back_only_after_the_gpu_has_finished() {
        let mut session = session();
        session.backend_mut().run_work = true;
        let context = create_context(&mut session, 0);
        let (queue, _, fence) = create_queue(&mut session, context);
        let first = reserve(&mut session, 0x10000);
        submit(&mut session, queue, 1);
        free(&mut session, first);
        assert_ne!(reserve(&mut session, 0x10000), first);
        session.memory_mut().write_u64(fence, 1).unwrap();
        submit(&mut session, queue, 1);
        assert_eq!(reserve(&mut session, 0x10000), first);
    }

    #[test]
    fn a_mapping_address_comes_back_when_its_allocation_goes() {
        let mut session = session();
        let (resource, handle) = allocate(&mut session, private(0x1000));
        let (first, _) = map(&mut session, handle, 0, 0, 0);
        destroy(&mut session, resource, &[]);
        let (_, handle) = allocate(&mut session, private(0x1000));
        assert_eq!(map(&mut session, handle, 0, 0, 0).0, first);
    }

    #[test]
    fn a_mapping_address_that_held_locked_memory_stays_out_of_video_memory() {
        let mut session = session();
        let (resource, handle) = allocate(&mut session, private(0x1000));
        let (first, _) = map(&mut session, handle, 0, 0, 0);
        assert_eq!(lock(&mut session, handle).0, SUCCESS);
        destroy(&mut session, resource, &[]);
        let (_, handle) = allocate(&mut session, private(0x1000));
        assert_ne!(map(&mut session, handle, 0, 0, 0).0, first);
    }

    #[test]
    fn a_mapping_address_goes_back_to_the_memory_it_held_when_a_process_closes() {
        let mut session = session();
        let (_, handle) = allocate(&mut session, private(0x1000));
        let (first, _) = map(&mut session, handle, 0, 0, 0);
        assert_eq!(lock(&mut session, handle).0, SUCCESS);
        send(&mut session, &message(Command::DestroyProcess, HEADER_SIZE));
        submit(&mut session, 0x999, 1);
        let (_, handle) = allocate(&mut session, private(0x1000));
        assert_ne!(map(&mut session, handle, 0, 0, 0).0, first);
    }

    #[test]
    fn a_closed_process_gives_back_its_reservations_and_mappings() {
        let mut session = session();
        let reserved = reserve(&mut session, 0x10000);
        let (_, handle) = allocate(&mut session, private(0x1000));
        let (mapped, _) = map(&mut session, handle, 0, 0, 0);
        send(&mut session, &message(Command::DestroyProcess, HEADER_SIZE));
        submit(&mut session, 0x999, 1);
        assert_eq!(reserve(&mut session, 0x10000), reserved);
        let (_, handle) = allocate(&mut session, private(0x1000));
        assert_eq!(map(&mut session, handle, 0, 0, 0).0, mapped);
    }
}

mod statistics {
    use super::*;

    #[test]
    fn asking_for_statistics_is_refused_each_time_and_noted_once() {
        let mut session = session();
        for _ in 0..2 {
            let reply = send(&mut session, &message(Command::QueryStatistics, HEADER_SIZE));
            assert_eq!(status_of(&reply), Status::NOT_SUPPORTED.0);
        }
        assert!(session.statistics_refused);
    }
}

mod reservations {
    use super::*;

    const WANTED: u64 = 0x3_0000_0000;
    const OTHER_PROCESS: u32 = 0x4000_3000;

    fn request(base: u64, lowest: u64, limit: u64, size: u64) -> Vec<u8> {
        let mut bytes = message(Command::ReserveGpuAddresses, 0x40);
        set_u64(&mut bytes, 0x20, base);
        set_u64(&mut bytes, 0x28, lowest);
        set_u64(&mut bytes, 0x30, limit);
        set_u64(&mut bytes, 0x38, size);
        bytes
    }

    fn reserve(session: &mut TestSession, bytes: &[u8]) -> u64 {
        u64_at(&send(session, bytes), 0).unwrap()
    }

    fn from_other_process(mut bytes: Vec<u8>) -> Vec<u8> {
        set_u32(&mut bytes, 8, OTHER_PROCESS);
        bytes
    }

    fn free(process: Option<u32>, address: u64) -> Vec<u8> {
        let mut bytes = message(Command::FreeGpuAddresses, 0x30);
        set_u64(&mut bytes, 0x20, address);
        if let Some(process) = process {
            set_u32(&mut bytes, 8, process);
        }
        bytes
    }

    #[test]
    fn a_named_address_is_given_exactly() {
        let mut session = session();
        assert_eq!(reserve(&mut session, &request(WANTED, 0, 0, 0x1_0000)), WANTED);
    }

    #[test]
    fn a_named_address_costs_the_hand_outs_nothing() {
        let mut session = session();
        let first = reserve(&mut session, &request(0, 0, 0, 0x1_0000));
        reserve(&mut session, &request(WANTED, 0, 0, 64 << 30));
        let second = reserve(&mut session, &request(0, 0, 0, 0x1_0000));
        assert_eq!(second, first + 0x1_0000);
    }

    #[test]
    fn two_processes_can_hold_the_same_range_and_one_in_one_process_cannot() {
        let mut session = session();
        assert_eq!(reserve(&mut session, &request(WANTED, 0, 0, 0x1_0000)), WANTED);
        assert_eq!(reserve(&mut session, &request(WANTED, 0, 0, 0x1_0000)), 0);
        let other = from_other_process(request(WANTED, 0, 0, 0x1_0000));
        assert_eq!(reserve(&mut session, &other), WANTED);
    }

    #[test]
    fn an_address_no_range_holds_is_refused() {
        let mut session = session();
        assert_eq!(reserve(&mut session, &request(0x1_0000_0000, 0, 0, 0x1000)), 0, "in the hole");
        assert_eq!(reserve(&mut session, &request(crate::GPU_RANGES[1].end, 0, 0, 0x1000)), 0);
        let past_the_end = crate::GPU_RANGES[1].end - 0x1000;
        assert_eq!(reserve(&mut session, &request(past_the_end, 0, 0, 0x2000)), 0);
    }

    #[test]
    fn a_size_that_cannot_be_rounded_is_refused() {
        let mut session = session();
        assert_eq!(reserve(&mut session, &request(WANTED, 0, 0, u64::MAX)), 0);
        assert_eq!(reserve(&mut session, &request(0, 0, 0, u64::MAX)), 0);
    }

    #[test]
    fn hand_outs_jump_over_a_range_held_in_any_process() {
        let mut session = session();
        let next = reserve(&mut session, &request(0, 0, 0, 0x1_0000)) + 0x1_0000;
        let holder = from_other_process(request(next, 0, 0, 0x2_0000));
        assert_eq!(reserve(&mut session, &holder), next);
        assert_eq!(reserve(&mut session, &request(0, 0, 0, 0x1_0000)), next + 0x2_0000);
    }

    #[test]
    fn a_window_that_holds_the_next_address_uses_it() {
        let mut session = session();
        let first = reserve(&mut session, &request(0, 0, 0, 0x1_0000));
        let second = reserve(&mut session, &request(0, first, 1 << 40, 0x1_0000));
        assert_eq!(second, first + 0x1_0000);
    }

    #[test]
    fn a_window_ahead_of_the_next_address_is_met() {
        let mut session = session();
        let address = reserve(&mut session, &request(0, WANTED, WANTED + 0x10_0000, 0x1_0000));
        assert_eq!(address, WANTED);
    }

    #[test]
    fn a_window_behind_the_next_address_finds_a_gap() {
        let mut session = session();
        let low = reserve(&mut session, &request(0, 0, 0, 0x1_0000));
        reserve(&mut session, &from_other_process(request(0, 0, 0, 0x1000_0000)));
        let address = reserve(&mut session, &request(0, low, low + 0x10_0000, 0x1_0000));
        assert_eq!(address, low + 0x1_0000, "past this process's own, and over the other's");
    }

    #[test]
    fn a_full_window_is_refused() {
        let mut session = session();
        let low = reserve(&mut session, &request(0, 0, 0, 0x1_0000));
        reserve(&mut session, &request(0, 0, 0, 0x1000_0000));
        assert_eq!(reserve(&mut session, &request(0, low, low + 0x1_0000, 0x1_0000)), 0);
    }

    #[test]
    fn a_window_past_every_range_gets_an_ordinary_place() {
        let mut session = session();
        let address = reserve(&mut session, &request(0, 1 << 40, 2 << 40, 0x40_0000));
        assert!(crate::GpuAddresses::in_a_range(address), "{address:#x}");
    }

    #[test]
    fn freeing_lets_go_only_of_this_processs_range() {
        let mut session = session();
        reserve(&mut session, &request(WANTED, 0, 0, 0x1_0000));
        reserve(&mut session, &from_other_process(request(WANTED, 0, 0, 0x1_0000)));
        send(&mut session, &free(Some(OTHER_PROCESS), WANTED));
        assert_eq!(reserve(&mut session, &request(WANTED, 0, 0, 0x1_0000)), 0, "this process keeps it");
        send(&mut session, &free(None, WANTED));
        assert_eq!(reserve(&mut session, &request(WANTED, 0, 0, 0x1_0000)), WANTED);
    }

    #[test]
    fn a_placed_range_does_not_go_on_the_shelf_for_hand_outs() {
        let mut session = session();
        reserve(&mut session, &request(WANTED, 0, 0, 0x1_0000));
        send(&mut session, &free(None, WANTED));
        assert_ne!(reserve(&mut session, &request(0, 0, 0, 0x1_0000)), WANTED);
    }
}

mod video_guard {
    use super::*;
    use guesthouse_backend_standin::NODE_WITHOUT_CHANNEL;

    #[test]
    fn a_context_on_a_node_with_no_channel_is_refused_with_a_zero_handle() {
        let mut session = session();
        assert_eq!(create_context(&mut session, NODE_WITHOUT_CHANNEL), 0);
        assert!(session.context_nodes.is_empty());
    }

    #[test]
    fn a_context_whose_private_data_is_refused_gets_a_zero_handle() {
        let mut session = session();
        session.backend_mut().refuse_private_data = true;
        assert_eq!(create_context(&mut session, 0), 0);
        assert!(session.context_nodes.is_empty());
    }

    #[test]
    fn a_refusal_makes_no_handle() {
        let mut session = session();
        create_context(&mut session, NODE_WITHOUT_CHANNEL);
        let first = create_context(&mut session, 0);
        let mut fresh = self::session();
        assert_eq!(create_context(&mut fresh, 0), first);
    }
}
