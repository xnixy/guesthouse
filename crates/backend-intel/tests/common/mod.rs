//! What the tests share: a card like the Arc 140V, a session over it, and guest messages carrying
//! private data laid out as Intel's driver lays it out.

#![allow(dead_code)]

use guesthouse_backend::Backend;
use guesthouse_backend_intel::IntelBackend;
use guesthouse_backend_intel::card::CardFacts;
use guesthouse_backend_intel::hwconfig::{self, Hwconfig};
use guesthouse_backend_intel::protocol::{allocation, command_header, context, hw_queue};
use guesthouse_backend_intel::sysfs::{Frequencies, PciIdentity};
use guesthouse_backend_intel::xe_query::{Config, Engine, Gt, IpVersion, MemoryRegion, TopologyMask};
use guesthouse_backend_standin::SparseMemory;
use guesthouse_core::{Answer, Config as SessionConfig, MemoryLimits, ReplyTicket, Session};
use guesthouse_messages::wire::{set_u32, set_u64, u32_at, u64_at};
use guesthouse_messages::{Command, HEADER_SIZE, Status};
use std::time::Duration;

pub const WINDOW: u64 = 1 << 40;
pub const WINDOW_END: u64 = WINDOW + (1 << 30);
pub const PROCESS: u32 = 0x4000_2000;
pub const SUCCESS: u32 = Status::SUCCESS.0;
pub const TICKET: Option<ReplyTicket> = Some(ReplyTicket { channel: 1, transaction: 3 });
/// KMTUMDVERSION for Direct3D 12.
pub const DIRECT3D_12: u32 = 3;

pub type Guest = Session<IntelBackend, SparseMemory>;

/// A card like the Arc 140V, with numbers chosen for this test.
pub fn card() -> CardFacts {
    let gt = |kind, id, minor| Gt {
        kind,
        tile: 0,
        id,
        reference_clock: 19_200_000,
        near_memory: 1,
        far_memory: 0,
        ip: IpVersion { major: 20, minor, revision: 4 },
    };
    let table: Vec<u8> = [(hwconfig::MAX_SLICES_SUPPORTED, 1), (hwconfig::NUM_THREADS_PER_EU, 8)]
        .iter()
        .flat_map(|&(key, value)| [key, 1, value])
        .flat_map(u32::to_le_bytes)
        .collect();
    CardFacts {
        driver: "xe".into(),
        pci: PciIdentity { vendor: 0x8086, device: 0x64A0, revision: 4, ..PciIdentity::default() },
        config: Config { device_id: 0x64A0, revision: 4, va_bits: 48, ..Config::default() },
        regions: vec![MemoryRegion {
            class: MemoryRegion::SYSTEM,
            instance: 0,
            min_page_size: 0x1000,
            total: 32 << 30,
            used: 0,
            cpu_visible: 0,
        }],
        engines: vec![
            Engine { class: Engine::RENDER, instance: 0, gt: 0 },
            Engine { class: Engine::COPY, instance: 0, gt: 0 },
        ],
        gts: vec![gt(Gt::MAIN, 0, 4), gt(Gt::MEDIA, 1, 0)],
        topology: vec![
            TopologyMask { gt: 0, kind: TopologyMask::DSS_GEOMETRY, mask: vec![0xFF] },
            TopologyMask { gt: 0, kind: TopologyMask::SIMD16_EU_PER_DSS, mask: vec![0xFF] },
        ],
        hwconfig: Hwconfig::parse(&table),
        frequencies: Frequencies { highest: Some(2050), lowest: Some(400), ..Frequencies::default() },
        ..CardFacts::default()
    }
}

pub fn guest() -> Guest {
    let backend = IntelBackend::from_card(card()).unwrap();
    let limits = MemoryLimits::new(backend.facts(), None, 16 << 30);
    let mut config = SessionConfig::new(limits);
    config.frame_wait = Duration::ZERO;
    let mut session = Session::new(backend, SparseMemory::new(WINDOW_END), config);
    let mut io_space = message(Command::SetIoSpace, HEADER_SIZE + 16);
    set_u64(&mut io_space, HEADER_SIZE, WINDOW);
    set_u64(&mut io_space, HEADER_SIZE + 8, 8 << 30);
    assert_eq!(send(&mut session, &io_space), SUCCESS.to_le_bytes());
    session
}

pub fn message(command: Command, size: usize) -> Vec<u8> {
    let mut bytes = vec![0; size.max(HEADER_SIZE)];
    set_u32(&mut bytes, 8, PROCESS);
    set_u32(&mut bytes, 16, command.to_wire());
    bytes
}

pub fn send(session: &mut Guest, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    assert_eq!(session.handle_message(message, TICKET, &mut out), Answer::Now);
    out
}

/// An adapter query of `kind`, with the guest's buffer of `size` bytes, which it fills first.
pub fn query(session: &mut Guest, kind: u32, data: &[u8]) -> Vec<u8> {
    let mut request = message(Command::QueryAdapter, HEADER_SIZE + 8);
    set_u32(&mut request, HEADER_SIZE, kind);
    set_u32(&mut request, HEADER_SIZE + 4, data.len() as u32);
    request.extend_from_slice(data);
    send(session, &request)
}

pub fn context(session: &mut Guest, node: u32, private: &[u8]) -> u32 {
    const NODE: usize = 0x20;
    const PRIVATE_SIZE: usize = 0x30;
    let mut request = message(Command::CreateContext, 0x34);
    set_u32(&mut request, NODE, node);
    set_u32(&mut request, PRIVATE_SIZE, private.len() as u32);
    request.extend_from_slice(private);
    u32_at(&send(session, &request), 0x18).unwrap()
}

/// Returns the queue and its fence.
pub fn queue(session: &mut Guest, context: u32) -> (u32, u64) {
    let mut request = message(Command::CreateHardwareQueue, 0x44);
    set_u32(&mut request, 0x38, context);
    set_u32(&mut request, 0x40, hw_queue::SIZE as u32);
    request.extend_from_slice(&[0; hw_queue::SIZE]);
    let reply = send(session, &request);
    assert_eq!(u32_at(&reply, 0x18), Ok(SUCCESS));
    (u32_at(&reply, 0x1C).unwrap(), u64_at(&reply, 0x28).unwrap())
}

/// Makes one allocation. Returns the reply.
pub fn allocate(session: &mut Guest, private: &[u8]) -> Vec<u8> {
    let mut request = message(Command::CreateAllocation, 0x40);
    set_u32(&mut request, 0x28, 1);
    request.extend_from_slice(&[0; 4]);
    request.extend_from_slice(&(private.len() as u32).to_le_bytes());
    request.extend_from_slice(&[0; 4]);
    request.extend_from_slice(private);
    send(session, &request)
}

pub fn submission(queue: u32, value: u64, private: &[u8]) -> Vec<u8> {
    const QUEUE: usize = 0x18;
    const FENCE_VALUE: usize = 0x20;
    const COMMANDS: usize = 0x28;
    const COMMANDS_LENGTH: usize = 0x30;
    let mut request = message(Command::SubmitToHardwareQueue, 0x50);
    set_u32(&mut request, QUEUE, queue);
    set_u64(&mut request, FENCE_VALUE, value);
    set_u64(&mut request, COMMANDS, 0x1_0000);
    set_u32(&mut request, COMMANDS_LENGTH, 0x40);
    request.extend_from_slice(private);
    request
}

/// What the Direct3D 12 driver sends for a context: one ID and no pointer, as the record says
/// virtual machine, its process, the GPU address model and its context type.
pub fn direct3d_12_context() -> Vec<u8> {
    let mut data = vec![0u8; context::SIZE];
    set_u32(&mut data, 0x08, 1);
    set_u32(&mut data, 0x0C, 4242);
    data[0x24] = 1;
    set_u32(&mut data, 0x50, 7);
    data
}

/// What the Direct3D 12 driver puts in a compact header: its stamp and context type, mid-batch
/// preemption, and its performance tag.
pub fn direct3d_12_header() -> Vec<u8> {
    let mut data = vec![0u8; command_header::COMPACT_SIZE];
    set_u32(&mut data, 0x00, 7 << 16 | 0x23D7);
    set_u32(&mut data, 0x08, command_header::flag::MID_BATCH_PREEMPTION);
    set_u32(&mut data, 0x1C, 0x2000);
    data
}

/// A 256 by 64 render target in rows, of 64 KiB.
pub fn render_target() -> Vec<u8> {
    let mut data = vec![0u8; allocation::OBJECT_SIZE];
    set_u32(&mut data, 0x10, 2);
    set_u32(&mut data, 0x18, 32);
    set_u64(&mut data, 0x24, allocation::info_flag::LINEAR);
    set_u64(&mut data, 0x30, 256);
    set_u32(&mut data, 0x38, 64);
    set_u64(&mut data, 0x108, 1024);
    set_u64(&mut data, 0x118, 0x1_0000);
    data
}
