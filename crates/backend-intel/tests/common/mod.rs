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

/// A hwconfig table with the most slices, the most dual subslices and the threads per EU.
pub fn hwconfig_of(values: &[u32; 3]) -> Hwconfig {
    let pairs = [
        (hwconfig::MAX_SLICES_SUPPORTED, values[0]),
        (hwconfig::MAX_DUAL_SUBSLICES_SUPPORTED, values[1]),
        (hwconfig::NUM_THREADS_PER_EU, values[2]),
    ];
    let table: Vec<u8> =
        pairs.iter().flat_map(|&(key, value)| [key, 1, value]).flat_map(u32::to_le_bytes).collect();
    Hwconfig::parse(&table)
}

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
            Engine { class: Engine::COMPUTE, instance: 0, gt: 0 },
        ],
        gts: vec![gt(Gt::MAIN, 0, 4), gt(Gt::MEDIA, 1, 0)],
        topology: vec![
            TopologyMask { gt: 0, kind: TopologyMask::DSS_GEOMETRY, mask: vec![0xFF] },
            TopologyMask { gt: 0, kind: TopologyMask::SIMD16_EU_PER_DSS, mask: vec![0xFF] },
        ],
        hwconfig: hwconfig_of(&[1, 8, 8]),
        frequencies: Frequencies { highest: Some(2050), lowest: Some(400), ..Frequencies::default() },
        ..CardFacts::default()
    }
}

pub fn guest() -> Guest {
    guest_on(card())
}

pub fn guest_on(card: CardFacts) -> Guest {
    let backend = IntelBackend::from_card(card).unwrap();
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

/// Context data with a few arbitrary fields set: a count, a process, the GPU address flag and a
/// context type.
pub fn context_data() -> Vec<u8> {
    let mut data = vec![0u8; context::SIZE];
    set_u32(&mut data, 0x08, 1);
    set_u32(&mut data, 0x0C, 4242);
    data[0x24] = 1;
    set_u32(&mut data, 0x50, 3);
    data
}

/// A compact header with a few arbitrary fields set: a stamp, a context type, one named flag and a
/// performance tag.
pub fn compact_header() -> Vec<u8> {
    let mut data = vec![0u8; command_header::COMPACT_SIZE];
    set_u32(&mut data, 0x00, 3 << 16 | 0x1234);
    set_u32(&mut data, 0x08, command_header::flag::MID_BATCH_PREEMPTION);
    set_u32(&mut data, 0x1C, 0x55);
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

/// What compute-runtime's topology reader makes of a record: the slices that are enabled, the
/// subslice indices of the single slice if there is one, and the totals it counts.
#[derive(Debug, PartialEq, Eq)]
pub struct Mapping {
    pub slice_indices: Vec<usize>,
    pub subslice_indices: Vec<i32>,
    pub slice_count: u32,
    pub subslice_count: u32,
    pub eu_count: u32,
}

/// The reader's walk, with the record's places in words: the slice count and the most slices at
/// 0xB3C and 0xB98, the slices from 0xBA8 on in steps of 0x14C, in each the flat subslices at +4
/// in steps of 12 and the dual subslices at +0x64 in steps of 0x1C holding two subslices each.
/// Fails as the reader fails: with no slice, no subslice or no EU.
pub fn translate_topology(record: &[u8]) -> Option<Mapping> {
    const MAX_SLICES: usize = 0xB98;
    const SLICE: usize = 0xBA8;
    const SLICE_STRIDE: usize = 0x14C;
    const FLAT: usize = 4;
    const DUAL: usize = 0x64;
    const DUAL_STRIDE: usize = 0x1C;
    const SUBSLICE_STRIDE: usize = 12;
    const DUALS_READ: usize = 6;
    const SUBSLICES_PER_DUAL: usize = 2;
    const FLAT_READ: usize = 8;
    let dword = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());
    let flag = |at: usize| record[at] != 0;
    // The record does not mark slices as dynamically populated, so the reader goes to the most.
    let slices = dword(MAX_SLICES) as usize;
    let mut mapping = Mapping {
        slice_indices: Vec::new(),
        subslice_indices: Vec::new(),
        slice_count: 0,
        subslice_count: 0,
        eu_count: 0,
    };
    for x in 0..slices {
        let slice = SLICE + SLICE_STRIDE * x;
        if !flag(slice) {
            continue;
        }
        mapping.slice_indices.push(x);
        mapping.slice_count += 1;
        let mut indices = Vec::new();
        let mut index = -1i32;
        let mut dual_enabled = false;
        for dual in 0..DUALS_READ {
            let at = slice + DUAL + DUAL_STRIDE * dual;
            if !flag(at) {
                index += 2;
                continue;
            }
            for y in 0..SUBSLICES_PER_DUAL {
                index += 1;
                let sub = at + 4 + SUBSLICE_STRIDE * y;
                if !flag(sub) {
                    continue;
                }
                dual_enabled = true;
                mapping.subslice_count += 1;
                indices.push(index);
                mapping.eu_count += dword(sub + 4);
            }
        }
        if !dual_enabled {
            for sub in 0..FLAT_READ {
                let at = slice + FLAT + SUBSLICE_STRIDE * sub;
                if flag(at) {
                    mapping.subslice_count += 1;
                    indices.push(sub as i32);
                    mapping.eu_count += dword(at + 4);
                }
            }
        }
        if mapping.slice_count == 1 {
            mapping.subslice_indices = indices;
        }
    }
    if mapping.slice_count != 1 {
        mapping.subslice_indices.clear();
    }
    let worked = mapping.slice_count != 0 && mapping.subslice_count != 0 && mapping.eu_count != 0;
    worked.then_some(mapping)
}
