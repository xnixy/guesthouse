//! A guest's first steps on an Intel card, as whole messages through the core's session and this
//! backend: the adapter opened and its record read, a context, a hardware queue, an allocation, a
//! submission and an escape, each with private data laid out as Intel's driver lays it out.
//!
//! This proves the parts join up and that a submission comes back finished. It proves nothing
//! about a real card or a real guest.

mod common;

use common::*;
use guesthouse_backend::GuestMemory;
use guesthouse_backend_intel::protocol::{context, escape, record};
use guesthouse_messages::wire::{set_u32, set_u64, u32_at, u64_at};
use guesthouse_messages::{Command, HEADER_SIZE, Status};

#[test]
fn the_adapter_opens_and_the_record_says_what_the_guest_picks_its_driver_by() {
    let mut session = guest();
    let mut open = message(Command::OpenAdapter, HEADER_SIZE + 16);
    set_u64(&mut open, HEADER_SIZE + 8, 0x6949);
    assert_eq!(u32_at(&send(&mut session, &open), 4), Ok(SUCCESS));

    let count = query(&mut session, record::QUERY_TYPE, &[0; record::COUNT_SIZE]);
    assert_eq!(count, [SUCCESS.to_le_bytes(), 1u32.to_le_bytes()].concat());

    let mut ask = vec![0u8; record::RECORD_SIZE];
    set_u32(&mut ask, 4, 0x000F_0001);
    let reply = query(&mut session, record::QUERY_TYPE, &ask);
    assert_eq!(u32_at(&reply, 0), Ok(SUCCESS));
    assert_eq!(reply.len(), 4 + record::RECORD_SIZE);
    let summary = record::summary(&reply[4..]);
    assert_eq!((summary.product_family, summary.render_core, summary.device_id), (1275, 0xC09, 0x64A0));
    assert_eq!((summary.virtual_machine, summary.compression), (1, false));
    assert_eq!(u32_at(&reply, 4 + 4), Ok(0x000F_0001), "the guest's build goes back as it came");
}

#[test]
fn a_record_of_the_wrong_size_is_refused() {
    let mut session = guest();
    let reply = query(&mut session, record::QUERY_TYPE, &vec![0; record::RECORD_SIZE - 4]);
    assert_eq!(u32_at(&reply, 0), Ok(Status::INVALID_PARAMETER.0));
}

#[test]
fn the_direct3d_12_driver_is_named_in_the_driver_store() {
    let mut session = guest();
    let mut ask = DIRECT3D_12.to_le_bytes().to_vec();
    ask.resize(520, 0);
    let reply = query(&mut session, 1, &ask);
    assert_eq!(u32_at(&reply, 0), Ok(SUCCESS));
    let units: Vec<u16> = reply[8..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|unit| u16::from_le_bytes(*unit))
        .take_while(|&unit| unit != 0)
        .collect();
    assert!(String::from_utf16_lossy(&units).ends_with("\\igd12umd64.dll"));
}

#[test]
fn a_context_a_queue_an_allocation_and_a_submission_go_through_and_the_fence_moves() {
    let mut session = guest();
    let context = context(&mut session, 0, &direct3d_12_context());
    assert_ne!(context, 0);
    let (queue, fence) = queue(&mut session, context);

    let reply = allocate(&mut session, &render_target());
    assert_eq!(u32_at(&reply, 0), Ok(SUCCESS));
    let handle = u32_at(&reply, 0x10).unwrap();
    let mut map = message(Command::MapGpuAddresses, 0x50);
    set_u32(&mut map, 0x38, handle);
    assert_ne!(u64_at(&send(&mut session, &map), 0), Ok(0));

    for value in 1..=3 {
        let reply = send(&mut session, &submission(queue, value, &direct3d_12_header()));
        assert_eq!(reply, SUCCESS.to_le_bytes());
        assert_eq!(session.memory().read_u64(fence), Ok(value), "finished unrun, so the fence moves at once");
    }
}

#[test]
fn a_context_with_short_private_data_is_refused_and_one_with_none_is_taken() {
    let mut session = guest();
    assert_eq!(context(&mut session, 0, &[0; context::SIZE - 1]), 0);
    assert_ne!(context(&mut session, 0, &[]), 0);
}

#[test]
fn an_allocation_whose_data_the_kernel_driver_would_refuse_fails_the_call() {
    let mut session = guest();
    let reply = allocate(&mut session, &[0; 0x100]);
    assert_eq!(u32_at(&reply, 0), Ok(Status::INVALID_PARAMETER.0));
    assert_eq!(session.backend().live_memory_blocks(), 0);
}

#[test]
fn a_header_of_the_wrong_size_stops_the_queue_and_leaves_nothing_waiting() {
    let mut session = guest();
    let context = context(&mut session, 0, &direct3d_12_context());
    let (queue, fence) = queue(&mut session, context);
    let reply = send(&mut session, &submission(queue, 1, &[0; 0x18]));
    assert_eq!(reply, Status::UNSUCCESSFUL.0.to_le_bytes(), "the program has to hear of it");
    assert_eq!(session.memory().read_u64(fence), Ok(1));
}

#[test]
fn an_escape_comes_back_as_it_was_sent() {
    let mut session = guest();
    const PAYLOAD: usize = 0xC0;
    let mut data = vec![0u8; escape::HEADER_SIZE + PAYLOAD];
    set_u32(&mut data, 0, PAYLOAD as u32);
    set_u32(&mut data, 8, escape::GMM_CONTROL);
    set_u32(&mut data, escape::HEADER_SIZE, 0x1D);
    let mut request = message(Command::DriverEscape, HEADER_SIZE + 24);
    set_u32(&mut request, HEADER_SIZE + 16, data.len() as u32);
    request.extend_from_slice(&data);
    let reply = send(&mut session, &request);
    assert_eq!(reply[..data.len()], data);
}
