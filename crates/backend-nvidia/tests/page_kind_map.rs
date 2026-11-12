//! What a map of depth memory asks the RM for on each generation.

use guesthouse_backend::{Backend, GpuRange, MapMemoryType, MapRequest, MemoryKind, SpaceId};
use guesthouse_backend_nvidia::NvidiaBackend;
use guesthouse_backend_nvidia::fake_guest::start_on_card;
use guesthouse_backend_nvidia::rm::fake::{Call, FakeRm};
use guesthouse_backend_nvidia::rm::fake_card::{
    FAKE_ADA, FAKE_AMPERE, FAKE_BLACKWELL, FAKE_TURING, FakeCard,
};

const ADDRESS: u64 = 0x5000_0000;
const LENGTH: u64 = 0x1_0000;
/// The class byte of the driver's protection value, with something else around it.
const Z24S8_PROTECTION: u64 = 0x0400 | 0x23;
const Z24S8_KIND: u32 = 0x05;

fn started(rm: FakeRm, card: &FakeCard) -> NvidiaBackend<FakeRm> {
    start_on_card(rm, card, card.device as u16).map_err(|refusal| refusal.0).expect("the pretend card passes")
}

/// Maps video memory and says what each map the RM was asked for carried.
fn kinds_mapped(backend: &mut NvidiaBackend<FakeRm>, driver_protection: u64) -> Vec<Option<u32>> {
    let range = GpuRange { start: ADDRESS, end: ADDRESS + LENGTH * 4 };
    let space: SpaceId = backend.create_address_space(&[range]).expect("a space");
    let (memory, _) = backend.allocate_video_memory(LENGTH, MemoryKind::PLAIN).expect("memory");
    let request = MapRequest {
        space,
        memory,
        offset: 0,
        length: LENGTH,
        address: ADDRESS,
        small_pages: true,
        gpu_cached: false,
        readable: true,
        writable: true,
        executable: false,
        memory_type: MapMemoryType::Default,
        driver_protection,
    };
    backend.map(&request).expect("the map works");
    let maps = backend.device().rm.calls.iter();
    maps.filter_map(|call| match call {
        Call::MapDma(map) if map.address == ADDRESS => Some(map.page_kind),
        _ => None,
    })
    .collect()
}

#[test]
fn turing_ampere_and_ada_map_a_depth_class_with_its_kind() {
    for card in [&FAKE_TURING, &FAKE_AMPERE, &FAKE_ADA] {
        let mut backend = started(FakeRm::new(), card);
        assert_eq!(kinds_mapped(&mut backend, Z24S8_PROTECTION), [Some(Z24S8_KIND)], "{}", card.device);
    }
}

#[test]
fn a_map_with_no_depth_class_sets_no_kind() {
    for card in [&FAKE_TURING, &FAKE_AMPERE, &FAKE_ADA, &FAKE_BLACKWELL] {
        let mut backend = started(FakeRm::new(), card);
        assert_eq!(kinds_mapped(&mut backend, 0x23), [None], "{}", card.device);
    }
}

#[test]
fn blackwell_ignores_the_depth_class() {
    let mut backend = started(FakeRm::new(), &FAKE_BLACKWELL);
    assert_eq!(kinds_mapped(&mut backend, Z24S8_PROTECTION), [None]);
}

#[test]
fn a_host_that_refuses_the_kind_gets_the_map_without_it() {
    let mut rm = FakeRm::new();
    rm.refuse_page_kind = true;
    let mut backend = started(rm, &FAKE_ADA);
    assert_eq!(kinds_mapped(&mut backend, Z24S8_PROTECTION), [None]);
}
