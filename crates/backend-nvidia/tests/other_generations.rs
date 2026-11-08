//! A pretend Ada card and a pretend Turing card, and what a guest should be told about each chip.
//! This shows the tables and the code around them agree with those values, and nothing about a real
//! card. The engines the pretend cards list are only there to make the nodes come out.

use guesthouse_backend::{Backend, Status, VendorQuery};
use guesthouse_backend_nvidia::NvidiaBackend;
use guesthouse_backend_nvidia::channel::{AREA_SIZE, Channel, ChannelPlace, Engine};
use guesthouse_backend_nvidia::fake_guest::{message, start_on_card};
use guesthouse_backend_nvidia::profile::{
    ADA, GUEST_617, Generation, HOST_615, TURING, generation_for, guest_channel_classes,
};
use guesthouse_backend_nvidia::pushbuffer::{RingEntry, ring_entry};
use guesthouse_backend_nvidia::record::card::COPY_ENGINE_CAPS;
use guesthouse_backend_nvidia::rm::device::Device;
use guesthouse_backend_nvidia::rm::fake::FakeRm;
use guesthouse_backend_nvidia::rm::fake_card::{FAKE_ADA, FAKE_TURING, FakeCard};
use guesthouse_backend_standin::SparseMemory;
use guesthouse_core::{Answer, Config, MemoryLimits, ReplyTicket, Session};
use guesthouse_messages::wire::{set_u32, set_u64, u32_at, u64_at};
use guesthouse_messages::{Command, HEADER_SIZE};
use std::time::Duration;

const LAYOUT: &guesthouse_backend_nvidia::profile::RecordLayout = &GUEST_617.record;
const RECORD_QUERY: u32 = 0;
const NODE_QUERY: u32 = 25;
const NODE_ANSWER_SIZE: usize = 78;
const NODE_ENGINE_FIELD: usize = 4;
const NODE_NAME_FIELD: usize = 8;
const NODE_NAME_BYTES: usize = 64;
const MOST_NODES: u32 = 40;
const GRAPHICS_TABLE_SM: usize = 0x0C * 8 + 4;
const PCI_DEVICE_FIELD: usize = 12;
const IMPLEMENTATION_FIELD: usize = 4;

const WINDOW: u64 = 1 << 40;
const WINDOW_END: u64 = WINDOW + (1 << 30);
const TICKET: Option<ReplyTicket> = Some(ReplyTicket { channel: 1, transaction: 9 });
const AREA_ADDRESS: u64 = 0x800_0000;

// The fields of a submission's private data, with a list inside it.
const LIST_OFFSET_FIELD: usize = 0x0C;
const ENTRIES_FIELD: usize = 0x14;
const LIST_AT: usize = 0x84;
const VULKAN_CLIENT: u32 = 4;
const NODE_ENTRY_SIZE: usize = 40;
const NODE_RECORD_SIZE: usize = 0x110;
const NODE_COPY_ENGINE_FIELD: usize = 0x0C;

/// What a guest should be told about a card.
struct Expected {
    device: u16,
    architecture: u32,
    implementation: u32,
    sm_version: u32,
    shader_set: u32,
    classes: usize,
    /// The classes this backend makes, with the generation's own first.
    used_classes: [u32; 7],
    /// The engine nodes Windows lists, in order.
    nodes: &'static [&'static str],
    /// The same nodes as the record has them.
    entries: &'static [Entry],
    /// What the pretend card's copy engines say, as a record has it.
    copy_caps: u32,
}

/// One node in the record.
struct Entry {
    kind: u32,
    /// The word after the type, which is one on all but the overlay.
    listed: u32,
    scheduled: u32,
    in_flight: u32,
    /// The bits of a copy node's flags that don't come from the card, and its engine.
    copy: Option<(u32, u32)>,
}

const fn plain(kind: u32) -> Entry {
    Entry { kind, listed: 1, scheduled: 0, in_flight: 4, copy: None }
}

const fn copy(kind: u32, scheduled: u32, in_flight: u32, flags: u32, engine: u32) -> Entry {
    Entry { kind, listed: 1, scheduled, in_flight, copy: Some((flags, engine)) }
}

const GRAPHICS: Entry =
    Entry { kind: 0x01, listed: 1, scheduled: 1, in_flight: 12, copy: Some((0x14, 0x09)) };
const OVERLAY: Entry = Entry { kind: 0x02, listed: 0, scheduled: 0, in_flight: 4, copy: None };
const NO_TYPE: Entry = plain(0);
const SECURITY: Entry = plain(0x0A);
const VR: Entry = plain(0x1C);
const SECURITY_VIRTUAL: Entry = plain(0x3B);

const RTX_4080: Expected = Expected {
    device: 0x2704,
    architecture: 0x190,
    implementation: 0x3,
    sm_version: 0x809,
    shader_set: 0x7005,
    classes: 127,
    used_classes: [0xC997, 0xC9C0, 0xC56F, 0xC7B5, 0xC561, 0xA140, 0x902D],
    nodes: &[
        "3D",
        "overlay",
        "decode",
        "copy",
        "copy",
        "Security",
        "encode",
        "encode",
        "OFA_0",
        "VR",
        "copy",
        "copy",
        "copy",
        "copy",
        "Security_1",
    ],
    entries: &[
        GRAPHICS,
        OVERLAY,
        NO_TYPE,
        copy(0x0B, 0, 4, 0x10, 0x0D),
        copy(0x0C, 1, 32, 0x10, 0x0E),
        SECURITY,
        NO_TYPE,
        NO_TYPE,
        plain(0x21),
        VR,
        copy(0x1D, 0, 4, 0x02, 0x0D),
        copy(0x0E, 0, 4, 0x10, 0x0D),
        copy(0x0F, 0, 4, 0x10, 0x0D),
        copy(0x10, 0, 4, 0x10, 0x0D),
        SECURITY_VIRTUAL,
    ],
    copy_caps: 0x1A0,
};

const GTX_1660_SUPER: Expected = Expected {
    device: 0x21C4,
    architecture: 0x160,
    implementation: 0x8,
    sm_version: 0x703,
    shader_set: 0x6000,
    classes: 110,
    used_classes: [0xC597, 0xC5C0, 0xC46F, 0xC5B5, 0xC461, 0xA140, 0x902D],
    nodes: &[
        "3D",
        "overlay",
        "decode",
        "copy",
        "copy",
        "copy",
        "Security",
        "encode",
        "VR",
        "copy",
        "copy",
        "copy",
        "copy",
        "Security_1",
    ],
    entries: &[
        GRAPHICS,
        OVERLAY,
        NO_TYPE,
        copy(0x0B, 0, 4, 0x10, 0x0A),
        copy(0x0C, 1, 32, 0x10, 0x0B),
        copy(0x0D, 1, 32, 0x10, 0x0C),
        SECURITY,
        plain(0x13),
        VR,
        copy(0x1D, 0, 4, 0x02, 0x0A),
        copy(0x0E, 0, 4, 0x10, 0x0A),
        copy(0x0F, 0, 4, 0x10, 0x0A),
        copy(0x10, 0, 4, 0x10, 0x0A),
        SECURITY_VIRTUAL,
    ],
    copy_caps: 0x120,
};

const CARDS: [(&FakeCard, &Generation, &Expected); 2] =
    [(&FAKE_ADA, &ADA, &RTX_4080), (&FAKE_TURING, &TURING, &GTX_1660_SUPER)];

fn started(card: &FakeCard) -> NvidiaBackend<FakeRm> {
    let backend = start_on_card(FakeRm::new(), card, card.device as u16);
    backend.map_err(|refusal| refusal.0).expect("the pretend card passes")
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn answer(backend: &NvidiaBackend<FakeRm>, kind: u32, size: usize, data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let query = VendorQuery { kind, size, data, process_is_32_bit: false };
    (backend.answers().answer(&query, &mut out) == Some(Status::SUCCESS)).then_some(out)
}

fn record(backend: &NvidiaBackend<FakeRm>) -> Vec<u8> {
    answer(backend, RECORD_QUERY, LAYOUT.size, &[]).unwrap()
}

fn classes_in(record: &[u8]) -> Vec<u32> {
    let count = word(record, LAYOUT.classes) as usize;
    (0..count).map(|index| word(record, LAYOUT.classes + 4 + index * 4)).collect()
}

/// A node as Windows names it.
fn node_label(answer: &[u8]) -> String {
    let name = &answer[NODE_NAME_FIELD..NODE_NAME_FIELD + NODE_NAME_BYTES];
    let units: Vec<u16> = name.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit)).collect();
    let name = String::from_utf16_lossy(&units);
    match word(answer, NODE_ENGINE_FIELD) {
        1 => "3D".to_owned(),
        2 => "decode".to_owned(),
        3 => "encode".to_owned(),
        6 => "copy".to_owned(),
        7 => "overlay".to_owned(),
        _ => name.trim_end_matches('\0').to_owned(),
    }
}

#[test]
fn each_architecture_finds_its_generation() {
    for (_, generation, expected) in CARDS {
        assert_eq!(generation_for(expected.architecture), Some(generation));
    }
}

#[test]
fn the_guest_is_told_what_it_should_be() {
    for (card, _, expected) in CARDS {
        let backend = started(card);
        let record = record(&backend);
        assert_eq!(word(&record, LAYOUT.pci + PCI_DEVICE_FIELD) >> 16, u32::from(expected.device));
        assert_eq!(word(&record, LAYOUT.architecture), expected.architecture);
        assert_eq!(word(&record, LAYOUT.architecture + IMPLEMENTATION_FIELD), expected.implementation);
        assert_eq!(word(&record, LAYOUT.graphics_table + GRAPHICS_TABLE_SM), expected.sm_version);
        assert_eq!(word(&record, LAYOUT.shader_set), expected.shader_set);
        assert_eq!(backend.facts().card.device, expected.device);
    }
}

#[test]
fn the_class_list_fits_and_holds_every_class_the_backend_makes() {
    for (card, generation, expected) in CARDS {
        let classes = classes_in(&record(&started(card)));
        assert_eq!(classes.len(), expected.classes);
        for class in expected.used_classes {
            assert!(classes.contains(&class), "{class:#x}");
        }
        for class in [generation.channel_class, generation.copy_class, generation.usermode_class] {
            assert!(expected.used_classes.contains(&class), "{class:#x}");
        }
    }
}

#[test]
fn the_nodes_come_in_the_order_and_number_windows_lists() {
    for (card, _, expected) in CARDS {
        let backend = started(card);
        let told: Vec<String> = (0..MOST_NODES)
            .filter_map(|node| answer(&backend, NODE_QUERY, NODE_ANSWER_SIZE, &node.to_le_bytes()))
            .map(|answer| node_label(&answer))
            .collect();
        assert_eq!(told, expected.nodes);
        assert_eq!(backend.facts().node_count as usize, expected.nodes.len());
        let record = record(&backend);
        assert_eq!(word(&record, LAYOUT.node_count) as usize, expected.nodes.len());
    }
}

#[test]
fn each_nodes_entry_in_the_record_is_as_the_table_says() {
    for (card, _, expected) in CARDS {
        let record = record(&started(card));
        assert_eq!(expected.entries.len(), expected.nodes.len());
        for (ordinal, entry) in expected.entries.iter().enumerate() {
            let at = LAYOUT.node_entries + ordinal * NODE_ENTRY_SIZE;
            let found =
                [word(&record, at), word(&record, at + 4), word(&record, at + 8), word(&record, at + 0x14)];
            assert_eq!(found, [entry.kind, entry.listed, entry.scheduled, entry.in_flight], "{ordinal}");
            let details = LAYOUT.node_records + ordinal * NODE_RECORD_SIZE;
            let (flags, engine) =
                entry.copy.map_or((0, 0), |(flags, engine)| (flags | expected.copy_caps, engine));
            assert_eq!(word(&record, details), flags, "{ordinal}");
            assert_eq!(word(&record, details + NODE_COPY_ENGINE_FIELD), engine, "{ordinal}");
        }
    }
}

#[test]
fn the_1660_has_two_scheduled_copy_nodes_and_the_4080_one() {
    let scheduled = |expected: &Expected| {
        expected
            .entries
            .iter()
            .filter(|entry| entry.copy.is_some() && entry.kind != 1 && entry.scheduled == 1)
            .count()
    };
    assert_eq!(scheduled(&GTX_1660_SUPER), 2);
    assert_eq!(scheduled(&RTX_4080), 1);
}

#[test]
fn a_card_that_will_not_say_its_copy_caps_is_refused() {
    let mut rm = FakeRm::new();
    rm.refuse_control = Some(COPY_ENGINE_CAPS.0);
    let Err(refusal) = start_on_card(rm, &FAKE_ADA, FAKE_ADA.device as u16) else {
        panic!("the record needs them");
    };
    assert!(refusal.0.starts_with("Reading the card failed"), "{}", refusal.0);
}

#[test]
fn only_graphics_and_copy_nodes_take_a_context() {
    for (card, _, expected) in CARDS {
        let mut backend = started(card);
        for (node, label) in (0u32..).zip(expected.nodes) {
            let wanted = ["3D", "copy"].contains(label);
            assert_eq!(backend.context_allowed(node, 0), wanted, "{label}");
            assert_eq!(backend.context_allowed(node, VULKAN_CLIENT), wanted, "{label} for Vulkan");
        }
    }
}

#[test]
fn no_video_runs_and_the_record_offers_none() {
    for (card, generation, _) in CARDS {
        let record = record(&started(card));
        assert!(!generation.nodes.video_runs);
        let caps: Vec<u32> = (0..4).map(|index| word(&record, LAYOUT.video_caps + index * 4)).collect();
        assert_eq!(caps, [0; 4]);
        assert_eq!(record[LAYOUT.decoder_flags], 0);
        assert_eq!(record[LAYOUT.encoder_flags], 0);
    }
}

#[test]
fn the_guests_handles_stand_for_this_generations_classes() {
    for (_, generation, expected) in CARDS {
        let classes = guest_channel_classes(&GUEST_617, generation);
        let found: Vec<u32> = classes.iter().map(|&(_, class)| class).collect();
        assert_eq!(found, [expected.used_classes[5], expected.used_classes[0], expected.used_classes[6]]);
    }
}

fn channel(generation: &Generation, engine: Engine) -> (Device<FakeRm>, Vec<u32>) {
    let mut device = Device::open(FakeRm::new(), &HOST_615).unwrap();
    let space = device.create_address_space(&[(0x400_0000, 0x1_0000_0000)]).unwrap();
    let place = ChannelPlace {
        ring_address: 0x900_0000,
        ring_entries: 1024,
        area_address: AREA_ADDRESS,
        area_size: AREA_SIZE,
    };
    let channel = Channel::new(&mut device, generation, &HOST_615, &space, place, engine).unwrap();
    let prelude = RingEntry::parse(channel.prelude_entry().to_le_bytes());
    let start = (prelude.address - AREA_ADDRESS) as usize;
    let words = channel.area().load_words(start, prelude.length as usize);
    (device, words)
}

#[test]
fn a_graphics_channel_makes_and_binds_the_generations_classes_in_order() {
    for (_, generation, _) in CARDS {
        let (device, words) = channel(generation, Engine::Graphics);
        let made: Vec<u32> = device.rm.allocated().map(|(class, _)| class).collect();
        assert!(made.contains(&generation.usermode_class));
        let wanted: Vec<u32> = generation.graphics_classes.iter().map(|class| class.0).collect();
        let at = made.iter().position(|&class| class == generation.channel_class).unwrap();
        assert_eq!(made[at + 1..at + 1 + wanted.len()], wanted);
        let bound: Vec<u32> = words.chunks(2).take(wanted.len()).map(|pair| pair[1]).collect();
        assert_eq!(bound, wanted);
    }
}

#[test]
fn a_copy_channel_makes_the_generations_copy_class() {
    for (_, generation, _) in CARDS {
        let (device, words) = channel(generation, Engine::Copy);
        assert!(device.rm.allocated().any(|(class, _)| class == generation.copy_class));
        assert_eq!(words[1], generation.copy_class);
    }
}

type Guest = Session<NvidiaBackend<FakeRm>, SparseMemory>;

fn send(session: &mut Guest, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    assert_eq!(session.handle_message(message, TICKET, &mut out), Answer::Now);
    out
}

fn guest_on(card: &FakeCard) -> Guest {
    let backend = started(card);
    let limits = MemoryLimits::new(backend.facts(), None, 16 << 30);
    let mut config = Config::new(limits);
    config.frame_wait = Duration::ZERO;
    let mut session = Session::new(backend, SparseMemory::new(WINDOW_END), config);
    let mut io_space = message(Command::SetIoSpace, HEADER_SIZE + 16);
    set_u64(&mut io_space, HEADER_SIZE, WINDOW);
    send(&mut session, &io_space);
    session
}

#[test]
fn a_guests_first_submission_runs_on_a_channel_of_this_generation() {
    for (card, generation, _) in CARDS {
        let mut session = guest_on(card);
        let mut request = message(Command::CreateContext, 0x34);
        set_u32(&mut request, 0x20, 0);
        let context = u32_at(&send(&mut session, &request), 0x18).unwrap();
        let mut request = message(Command::CreateHardwareQueue, 0x44 + 0x80);
        set_u32(&mut request, 0x38, context);
        set_u32(&mut request, 0x40, 0x80);
        let reply = send(&mut session, &request);
        let queue = u32_at(&reply, 0x1C).unwrap();
        assert_ne!(u64_at(&reply, 0x28), Ok(0));

        let mut private = vec![0; LIST_AT + 8];
        set_u32(&mut private, LIST_OFFSET_FIELD, LIST_AT as u32);
        set_u32(&mut private, ENTRIES_FIELD, 1);
        set_u64(&mut private, LIST_AT, ring_entry(0x1000, 4));
        let mut submission = message(Command::SubmitToHardwareQueue, 0x50);
        set_u32(&mut submission, 0x18, queue);
        set_u64(&mut submission, 0x20, 1);
        submission.extend_from_slice(&private);
        send(&mut session, &submission);

        let rm = &session.backend().device().rm;
        assert!(rm.allocated().any(|(class, _)| class == generation.channel_class));
        assert!(!rm.allocated().any(|(class, _)| class == 0xCA6F), "no Blackwell class on this card");
        assert_eq!(rm.doorbells().len(), 1);
    }
}

#[test]
fn a_card_with_no_profile_is_refused_by_name() {
    for architecture in [0x170, 0x180] {
        let card = FakeCard { architecture, ..FAKE_ADA };
        let Err(refusal) = start_on_card(FakeRm::new(), &card, card.device as u16) else {
            panic!("{architecture:#x} has no profile");
        };
        assert_eq!(refusal.0, format!("This build has no profile for GPU architecture {architecture:#x}."));
    }
}

#[test]
fn a_chip_with_an_sm_version_nobody_has_reported_is_refused() {
    let card = FakeCard { sm_version: 0x705, ..FAKE_TURING };
    let Err(refusal) = start_on_card(FakeRm::new(), &card, card.device as u16) else {
        panic!("0x705 has no shader set");
    };
    assert!(refusal.0.contains("SM 0x705"), "{}", refusal.0);
}

#[test]
fn these_generations_have_not_been_run_with_any_guest_branch() {
    assert!(ADA.run_on_guest_branches.is_empty());
    assert!(TURING.run_on_guest_branches.is_empty());
}

#[test]
fn the_summary_says_what_was_found_and_holds_nothing_that_identifies_the_card() {
    for (card, generation, expected) in CARDS {
        let backend = started(card);
        let summary = backend.card_summary();
        assert!(summary.contains(&format!("Generation: {}", generation.name)), "{summary}");
        assert!(summary.contains(&format!("SM version: {:#X}", expected.sm_version)), "{summary}");
        assert!(summary.contains(&format!("Shader set: {:#X}", expected.shader_set)), "{summary}");
        assert!(summary.contains(&format!("Classes: {}", expected.classes)), "{summary}");
        let record = record(&backend);
        let uuid: String =
            record[LAYOUT.uuid..LAYOUT.uuid + 4].iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(!summary.to_lowercase().contains(&uuid), "{summary}");
    }
}
