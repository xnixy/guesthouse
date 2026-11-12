//! Holds what a Blackwell guest is told and what its channels ask of the RM to fixed digests. Adding
//! another generation must leave every one of them alone. All of it runs on the pretend RM.

use guesthouse_backend::{Backend, GuestBooks, Status, VendorQuery, VideoMemoryUse};
use guesthouse_backend_nvidia::NvidiaBackend;
use guesthouse_backend_nvidia::answers::nodes::{copy_record_flags, node_entry, node_list, node_metadata};
use guesthouse_backend_nvidia::channel::{AREA_SIZE, Channel, ChannelPlace, Engine, VideoEngine};
use guesthouse_backend_nvidia::fake_guest::fake_backend;
use guesthouse_backend_nvidia::profile::{BLACKWELL, GUEST_617, HOST_615, guest_channel_classes};
use guesthouse_backend_nvidia::pushbuffer::RingEntry;
use guesthouse_backend_nvidia::rm::device::Device;
use guesthouse_backend_nvidia::rm::fake::FakeRm;

const FNV_START: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_STEP: u64 = 0x0100_0000_01B3;

const RECORD_QUERY: u32 = 0;
const NODE_QUERY: u32 = 25;
const NODE_ANSWER_SIZE: usize = 78;
const MOST_NODES: u32 = 40;
/// The short form of the record, for the guest's video runtime.
const CAPS_SIZE: usize = 0x40D;
const FIXED_QUERIES: [(u32, usize); 11] =
    [(11, 4), (15, 4), (17, 12), (22, 4), (24, 4), (55, 4), (60, 80), (61, 56), (62, 64), (75, 4), (76, 4)];

const ESCAPE_FUNCTION: usize = 0x10;
const ESCAPE_LIST_NODES: u32 = 0x0100_000F;
const ESCAPE_LIST_NODES_SIZE: usize = 0x31C;

const AREA_ADDRESS: u64 = 0x800_0000;
const RM_DECODER: u32 = 0x13;

/// The engines of the card this has run on, as far as nodes go, which the pretend card doesn't have.
const TESTED_ENGINES: [u32; 12] = [0x01, 0x09, 0x0D, 0x0E, 0x13, 0x16, 0x1C, 0x1D, 0x3F, 0x26, 0x33, 0x2E];
const TESTED_GRAPHICS_COPY: u32 = 0x09;

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_START, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(FNV_STEP))
}

fn answer(backend: &NvidiaBackend<FakeRm>, kind: u32, size: usize, data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let query = VendorQuery { kind, size, data, process_is_32_bit: false };
    (backend.answers().answer(&query, &mut out) == Some(Status::SUCCESS)).then_some(out)
}

#[test]
fn the_record_has_not_changed() {
    let backend = fake_backend(FakeRm::new());
    let record = answer(&backend, RECORD_QUERY, GUEST_617.record.size, &[]).unwrap();
    assert_eq!(digest(&record), 0x213c_7536_731e_862c);
    let short = answer(&backend, RECORD_QUERY, CAPS_SIZE, &[0; CAPS_SIZE]).unwrap();
    assert_eq!(digest(&short), 0x51b3_a94e_07b9_5b47);
}

#[test]
fn the_record_carries_the_video_caps() {
    let backend = fake_backend(FakeRm::new());
    let record = answer(&backend, RECORD_QUERY, GUEST_617.record.size, &[]).unwrap();
    let at = GUEST_617.record.video_caps;
    let caps: Vec<u32> =
        record[at..at + 16].chunks(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect();
    assert_eq!(caps, [1 << 24, 1 << 26, 1 << 20, 0x40_0030]);
    assert_eq!(caps[..3], BLACKWELL.video_record_flags.caps[..3]);
    assert_eq!(record[GUEST_617.record.encoder_flags], 0x10);
}

#[test]
fn the_record_carries_texture_limits_for_direct3d_9() {
    let backend = fake_backend(FakeRm::new());
    let record = answer(&backend, RECORD_QUERY, GUEST_617.record.size, &[]).unwrap();
    let at = GUEST_617.record.texture_limits;
    let limits: Vec<u32> =
        record[at..at + 20].chunks(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect();
    assert_eq!(limits, [0x8000, 0x8000, 0x2_0000, 0x2_0000, 0x800]);
}

#[test]
fn the_record_names_a_registry_key_for_the_drivers_settings() {
    let backend = fake_backend(FakeRm::new());
    let record = answer(&backend, RECORD_QUERY, GUEST_617.record.size, &[]).unwrap();
    let key =
        r"\REGISTRY\MACHINE\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let at = GUEST_617.record.registry_key;
    let units: Vec<u16> = record[at..at + key.len() * 2 + 2]
        .chunks(2)
        .map(|unit| u16::from_le_bytes(unit.try_into().unwrap()))
        .collect();
    assert_eq!(&units[..key.len()], key.encode_utf16().collect::<Vec<_>>().as_slice());
    assert_eq!(units[key.len()], 0, "the key ends in a zero");
}

#[test]
fn the_adapters_other_answers_have_not_changed() {
    let backend = fake_backend(FakeRm::new());
    let mut all = Vec::new();
    for (kind, size) in FIXED_QUERIES {
        all.extend(answer(&backend, kind, size, &[]).unwrap());
    }
    for node in 0..MOST_NODES {
        all.extend(answer(&backend, NODE_QUERY, NODE_ANSWER_SIZE, &node.to_le_bytes()).unwrap_or_default());
    }
    assert_eq!(digest(&all), 0xdd4b_c2cf_45f8_4698);
}

#[test]
fn the_list_of_nodes_for_the_video_runtime_has_not_changed() {
    let mut backend = fake_backend(FakeRm::new());
    let mut data = vec![0; ESCAPE_LIST_NODES_SIZE];
    data[ESCAPE_FUNCTION..ESCAPE_FUNCTION + 4].copy_from_slice(&ESCAPE_LIST_NODES.to_le_bytes());
    let memory = || VideoMemoryUse { total: 0, used: 0 };
    backend.escape(1, &mut data, &GuestBooks { memory: &memory, processes: &Vec::new });
    assert_eq!(digest(&data), 0x1ae3_70ad_4aee_1d38);
}

#[test]
fn the_nodes_of_the_tested_card_have_not_changed() {
    let mut all = Vec::new();
    let nodes = node_list(&TESTED_ENGINES, Some(TESTED_GRAPHICS_COPY), &BLACKWELL.nodes);
    assert_eq!(nodes.len(), 17);
    for (ordinal, node) in (0u32..).zip(&nodes) {
        all.extend(node_metadata(ordinal, node));
        all.extend(node_entry(node));
        all.extend(copy_record_flags(node, BLACKWELL.copy_node_flags.unwrap()).to_le_bytes());
    }
    assert_eq!(digest(&all), 0xf027_c5d6_0242_e90e);
}

#[test]
fn the_classes_each_guest_handle_stands_for_have_not_changed() {
    let classes = guest_channel_classes(&GUEST_617, &BLACKWELL);
    assert_eq!(classes, [(0xFF1F_E01A, 0xCD40), (0xFF1F_E026, 0xCE97), (0xFF1F_E02C, 0x902D)]);
}

/// Everything the RM is asked to make a channel, and the commands the channel starts with.
fn channel_digest(engine: Engine) -> u64 {
    let mut device = Device::open(FakeRm::new(), &HOST_615).unwrap();
    let space = device.create_address_space(&[(0x400_0000, 0x1_0000_0000)]).unwrap();
    let place = ChannelPlace {
        ring_address: 0x900_0000,
        ring_entries: 1024,
        area_address: AREA_ADDRESS,
        area_size: AREA_SIZE,
    };
    let channel = Channel::new(&mut device, &BLACKWELL, &HOST_615, &space, place, engine).unwrap();
    let prelude = RingEntry::parse(channel.prelude_entry().to_le_bytes());
    let start = (prelude.address - AREA_ADDRESS) as usize;
    let words = channel.area().load_words(start, prelude.length as usize);
    digest(format!("{:?} {words:x?}", device.rm.calls).as_bytes())
}

#[test]
fn channels_ask_the_rm_for_the_same_things_and_start_the_same_way() {
    assert_eq!(channel_digest(Engine::Graphics), 0x8910_33d6_d1b3_aefd);
    assert_eq!(channel_digest(Engine::Copy), 0x4b8c_d70c_895f_8e19);
    let class = BLACKWELL.video_decode_class;
    assert_eq!(
        channel_digest(Engine::Video(VideoEngine { rm_engine: RM_DECODER, class, instance: 0 })),
        0xcabc_3c62_5d3e_ccd2
    );
}
