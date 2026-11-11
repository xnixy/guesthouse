//! The record's slice arrays against compute-runtime's topology reader, for cards of different
//! shapes: what the reader counts is what the card has, and what the record's scalars say.

mod common;

use common::*;
use guesthouse_backend_intel::IntelBackend;
use guesthouse_backend_intel::card::CardFacts;
use guesthouse_backend_intel::xe_query::TopologyMask;

fn record_of(card: CardFacts) -> Vec<u8> {
    IntelBackend::from_card(card).unwrap().record().to_vec()
}

fn dword(record: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(record[at..at + 4].try_into().unwrap())
}

/// A card with these masks and this much hwconfig (slices, dual subslices, threads per EU).
fn card_with(dss: &[u8], eus: &[u8], hwconfig: [u32; 3]) -> CardFacts {
    let mask = |kind, mask: &[u8]| TopologyMask { gt: 0, kind, mask: mask.to_vec() };
    CardFacts {
        topology: vec![
            mask(TopologyMask::DSS_GEOMETRY, dss),
            mask(TopologyMask::DSS_COMPUTE, dss),
            mask(TopologyMask::SIMD16_EU_PER_DSS, eus),
        ],
        hwconfig: hwconfig_of(&hwconfig),
        ..card()
    }
}

#[test]
fn a_lunar_lake_shape_comes_out_of_the_reader_as_the_cards() {
    let record = record_of(card());
    let mapping = translate_topology(&record).expect("the reader accepts it");
    assert_eq!(mapping.slice_indices, [0]);
    assert_eq!(mapping.subslice_indices, (0..8).collect::<Vec<i32>>());
    assert_eq!((mapping.slice_count, mapping.subslice_count, mapping.eu_count), (1, 8, 64));
    // The scalars say the same: EUs, threads, slices, subslices, dual subslices, and the most.
    let scalars: Vec<u32> = [0xB34, 0xB38, 0xB3C, 0xB40, 0xB44, 0xB94, 0xB98, 0xB9C, 0xBA0]
        .iter()
        .map(|&at| dword(&record, at))
        .collect();
    assert_eq!(scalars, [64, 512, 1, 8, 8, 0, 1, 8, 8]);
    // The slice's own counts, and the EUs of a core.
    assert_eq!((dword(&record, 0xCEC), dword(&record, 0xCF0)), (8, 8));
    assert_eq!((dword(&record, 0xBB0), dword(&record, 0xBB4)), (8, 0xFF));
}

#[test]
fn a_mask_with_holes_across_two_slices_is_read_as_the_cards() {
    // Cores 0, 2, 3, 5, 6 and 15 of 16, eight to a slice, with seven EUs in each.
    let record = record_of(card_with(&[0b0110_1101, 0b1000_0000], &[0b0111_1111], [2, 16, 8]));
    let mapping = translate_topology(&record).expect("the reader accepts it");
    assert_eq!(mapping.slice_indices, [0, 1]);
    assert!(mapping.subslice_indices.is_empty(), "only a single slice has its indices kept");
    assert_eq!((mapping.slice_count, mapping.subslice_count, mapping.eu_count), (2, 6, 42));
    assert_eq!([dword(&record, 0xB3C), dword(&record, 0xB40), dword(&record, 0xB34)], [2, 6, 42]);
    assert_eq!((dword(&record, 0xCEC), dword(&record, 0xCEC + 0x14C)), (5, 1));
    assert_eq!(dword(&record, 0xB38), 42 * 8);
}

#[test]
fn a_disabled_first_slice_and_holes_inside_the_second_keep_their_indices() {
    // Cores 9, 10 and 13: slice 1, places 1, 2 and 5. Place 5 is the second subslice of dual subslice 2.
    let record = record_of(card_with(&[0, 0b0010_0110], &[0b0000_1111], [2, 16, 8]));
    let mapping = translate_topology(&record).expect("the reader accepts it");
    assert_eq!(mapping.slice_indices, [1]);
    assert_eq!(mapping.subslice_indices, [1, 2, 5]);
    assert_eq!((mapping.slice_count, mapping.subslice_count, mapping.eu_count), (1, 3, 12));
    assert_eq!(record[0xBA8], 0, "slice 0 is not enabled");
    assert_eq!(dword(&record, 0xB3C), 1);
}

#[test]
fn a_card_whose_hwconfig_does_not_divide_its_cores_leaves_the_arrays_empty() {
    let mut card = card();
    card.hwconfig = hwconfig_of(&[0, 0, 8]);
    let record = record_of(card);
    assert!(translate_topology(&record).is_none());
    assert!(!guesthouse_backend_intel::protocol::shape::filled(&record));
    assert_eq!(dword(&record, 0xB34), 64, "the totals are still the card's");
}
