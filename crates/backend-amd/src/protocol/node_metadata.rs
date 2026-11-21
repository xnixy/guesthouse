//! The node metadata query, KMTQAITYPE_NODEMETADATA: what kind of engine a node is, its name, and
//! whether it uses the GPU's own page tables. Direct3D asks it for a node it makes a context on.
//!
//! The answer is the WDK's D3DKMT_NODEMETADATA: the node's ordinal as asked, then DXGK_NODEMETADATA's
//! engine type, a name of [`NAME_CHARS`] UTF-16 units, its flags, and GpuMmuSupported and
//! IoMmuSupported. The NVIDIA and Intel backends give the same shape for their own nodes.

use super::bytes::dword;
use crate::info::{EngineSet, HW_IP_COMPUTE, HW_IP_DMA, HW_IP_GFX, HW_IP_VCN_ENC};
use crate::nodes::{Node, nodes};
use guesthouse_backend::{Status, VendorQuery};

/// KMTQAITYPE_NODEMETADATA.
pub const QUERY_NODE_METADATA: u32 = 25;
const ANSWER_SIZE: usize = 78;
const NAME_AT: usize = 8;
const NAME_CHARS: usize = 32;

/// DXGK_ENGINE_TYPE.
const ENGINE_OTHER: u32 = 0;
const ENGINE_3D: u32 = 1;
const ENGINE_COPY: u32 = 6;

/// The low half of what is asked is the node's ordinal, and the high half the adapter's index.
const ORDINAL_MASK: u32 = 0xFFFF;

/// DXGK_NODEMETADATA_FLAGS, all clear. They are for a node whose contexts the card schedules
/// itself, and the adapter block promises no hardware scheduler.
const FLAGS: u32 = 0;
/// Every node uses the GPU's page tables, as the WDDM 2.0 capabilities say, and none the IOMMU.
const GPU_MMU_SUPPORTED: u8 = 1;
const IO_MMU_SUPPORTED: u8 = 0;

/// The engine type and the name of a node. Compute has no type of its own in DXGK_ENGINE_TYPE, so
/// it is "other". Windows shows an engine of that type by its name, so the name is the one its
/// GPU Engine performance counters show for an AMD card's compute node. The video ring decodes and
/// encodes, which are two types there, so it is "other" as well, under AMD's name for the engine.
fn engine(node: &Node) -> (u32, &'static str) {
    match node.ip_type {
        HW_IP_GFX => (ENGINE_3D, "3D"),
        HW_IP_DMA => (ENGINE_COPY, "Copy"),
        HW_IP_COMPUTE => (ENGINE_OTHER, "Compute 0"),
        HW_IP_VCN_ENC => (ENGINE_OTHER, "Video Codec 0"),
        _ => (ENGINE_OTHER, "Other"),
    }
}

/// Answers for one of the nodes the guest is offered. Any other query is left alone, and so is one
/// for a node past them, which then fails as it did before.
pub fn answer(query: &VendorQuery<'_>, engines: &EngineSet, out: &mut Vec<u8>) -> Option<Status> {
    if query.kind != QUERY_NODE_METADATA || query.size != ANSWER_SIZE {
        return None;
    }
    let asked = dword(query.data, 0)?;
    let offered = nodes(engines);
    let (engine_type, name) = engine(offered.get((asked & ORDINAL_MASK) as usize)?);
    let start = out.len();
    out.extend_from_slice(&asked.to_le_bytes());
    out.extend_from_slice(&engine_type.to_le_bytes());
    name.encode_utf16().take(NAME_CHARS).for_each(|unit| out.extend_from_slice(&unit.to_le_bytes()));
    out.resize(start + NAME_AT + NAME_CHARS * size_of::<u16>(), 0);
    out.extend_from_slice(&FLAGS.to_le_bytes());
    out.extend_from_slice(&[GPU_MMU_SUPPORTED, IO_MMU_SUPPORTED]);
    Some(Status::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_CARD: EngineSet = EngineSet { graphics: true, compute: true, dma: true, video: false };

    fn asked(node: u32) -> [u8; ANSWER_SIZE] {
        let mut data = [0; ANSWER_SIZE];
        data[..4].copy_from_slice(&node.to_le_bytes());
        data
    }

    fn query(data: &[u8]) -> VendorQuery<'_> {
        VendorQuery {
            kind: QUERY_NODE_METADATA,
            size: ANSWER_SIZE,
            data,
            process_is_32_bit: false,
            program: "dwm.exe",
        }
    }

    fn name_of(answer: &[u8]) -> String {
        let units: Vec<u16> = answer[NAME_AT..NAME_AT + NAME_CHARS * 2]
            .chunks(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn each_node_is_answered_with_its_engine_type_and_name() {
        let told: Vec<(u32, String)> = (0..3)
            .map(|node| {
                let data = asked(node);
                let mut out = Vec::new();
                assert_eq!(answer(&query(&data), &FULL_CARD, &mut out), Some(Status::SUCCESS));
                assert_eq!(out.len(), ANSWER_SIZE);
                assert_eq!(dword(&out, 0), Some(node));
                let after_name = NAME_AT + NAME_CHARS * 2;
                assert_eq!(&out[after_name..], &[0, 0, 0, 0, GPU_MMU_SUPPORTED, IO_MMU_SUPPORTED]);
                (dword(&out, 4).unwrap(), name_of(&out))
            })
            .collect();
        assert_eq!(
            told,
            [
                (ENGINE_3D, "3D".to_owned()),
                (ENGINE_OTHER, "Compute 0".to_owned()),
                (ENGINE_COPY, "Copy".to_owned())
            ]
        );
    }

    #[test]
    fn a_card_without_compute_numbers_its_copy_node_second() {
        let engines = EngineSet { graphics: true, compute: false, dma: true, video: false };
        let data = asked(1);
        let mut out = Vec::new();
        assert_eq!(answer(&query(&data), &engines, &mut out), Some(Status::SUCCESS));
        assert_eq!((dword(&out, 4), name_of(&out).as_str()), (Some(ENGINE_COPY), "Copy"));
    }

    #[test]
    fn the_video_ring_is_answered_only_when_it_is_offered() {
        let data = asked(3);
        let mut out = Vec::new();
        assert_eq!(answer(&query(&data), &FULL_CARD, &mut out), None);
        let offered = EngineSet { video: true, ..FULL_CARD };
        assert_eq!(answer(&query(&data), &offered, &mut out), Some(Status::SUCCESS));
        assert_eq!((dword(&out, 4), name_of(&out).as_str()), (Some(ENGINE_OTHER), "Video Codec 0"));
    }

    #[test]
    fn the_adapter_index_in_the_high_half_comes_back_as_asked() {
        let data = asked(1 << 16 | 2);
        let mut out = vec![0xA5; 4];
        assert_eq!(answer(&query(&data), &FULL_CARD, &mut out), Some(Status::SUCCESS));
        assert_eq!(dword(&out, 4), Some(1 << 16 | 2));
        assert_eq!(dword(&out, 8), Some(ENGINE_COPY));
        assert_eq!(out.len(), 4 + ANSWER_SIZE, "the answer follows what was there");
    }

    #[test]
    fn a_node_past_those_offered_a_wrong_size_or_no_ordinal_is_left_alone() {
        let past = asked(FULL_CARD.count());
        let mut out = Vec::new();
        assert_eq!(answer(&query(&past), &FULL_CARD, &mut out), None);
        let first = asked(0);
        assert_eq!(answer(&VendorQuery { size: 40, ..query(&first) }, &FULL_CARD, &mut out), None);
        assert_eq!(answer(&VendorQuery { kind: 24, ..query(&first) }, &FULL_CARD, &mut out), None);
        assert_eq!(answer(&query(&[0; 3]), &FULL_CARD, &mut out), None);
        assert!(out.is_empty(), "nothing is written for a query that is left");
    }
}
