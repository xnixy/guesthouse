//! The nodes the guest sees, one for each engine family the card offers.
//!
//! A node is the guest's word for an engine it can submit to. Each one here stands for one of
//! amdgpu's engine families, in a fixed order with absent families left out, so the same card gives
//! the same nodes every time. The video ring's node is last, and there only when the daemon was
//! asked to offer it: see [`crate::video`].

use crate::info::{EngineSet, HW_IP_COMPUTE, HW_IP_DMA, HW_IP_GFX, HW_IP_VCN_ENC};

/// One node: the host's engine family behind it and the guest driver's own number for its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// amdgpu's `HW_IP_*`.
    pub ip_type: u32,
    /// What AMD's Windows driver calls the engine, which picks the queue type it builds for.
    pub guest_engine: u8,
}

/// AMD's Windows driver's engine numbers. Its user-mode driver makes a universal queue for 0, a
/// compute queue for 5 and a copy queue for 4.
const GUEST_UNIVERSAL: u8 = 0;
const GUEST_COMPUTE: u8 = 5;
const GUEST_DMA: u8 = 4;
/// Its first video codec engine, which both decodes and encodes. The driver sends its video work
/// to the node with this number, in the form amdgpu's one video ring takes from VCN 4 on.
pub const GUEST_VIDEO_CODEC: u8 = 41;

const GRAPHICS: Node = Node { ip_type: HW_IP_GFX, guest_engine: GUEST_UNIVERSAL };
const COMPUTE: Node = Node { ip_type: HW_IP_COMPUTE, guest_engine: GUEST_COMPUTE };
const DMA: Node = Node { ip_type: HW_IP_DMA, guest_engine: GUEST_DMA };
const VIDEO: Node = Node { ip_type: HW_IP_VCN_ENC, guest_engine: GUEST_VIDEO_CODEC };

/// The card's nodes, in the order the guest numbers them.
pub fn nodes(engines: &EngineSet) -> Vec<Node> {
    [(engines.graphics, GRAPHICS), (engines.compute, COMPUTE), (engines.dma, DMA), (engines.video, VIDEO)]
        .into_iter()
        .filter_map(|(present, node)| present.then_some(node))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_card_gives_graphics_compute_and_copy_in_that_order() {
        let engines = EngineSet { graphics: true, compute: true, dma: true, video: false };
        let kinds: Vec<_> = nodes(&engines).iter().map(|node| (node.ip_type, node.guest_engine)).collect();
        assert_eq!(kinds, [(HW_IP_GFX, 0), (HW_IP_COMPUTE, 5), (HW_IP_DMA, 4)]);
    }

    #[test]
    fn the_video_ring_is_the_last_node_and_only_when_it_is_offered() {
        let engines = EngineSet { graphics: true, compute: true, dma: true, video: true };
        let kinds: Vec<_> = nodes(&engines).iter().map(|node| (node.ip_type, node.guest_engine)).collect();
        assert_eq!(kinds, [(HW_IP_GFX, 0), (HW_IP_COMPUTE, 5), (HW_IP_DMA, 4), (HW_IP_VCN_ENC, 41)]);
        assert_eq!(engines.count(), 4);
        // The nodes before it keep their numbers, so a guest sees the same card with one more engine.
        let without = EngineSet { video: false, ..engines };
        assert_eq!(nodes(&without)[..], nodes(&engines)[..3]);
        assert_eq!(without.count(), 3);
    }

    #[test]
    fn an_absent_family_leaves_no_gap() {
        let engines = EngineSet { graphics: true, compute: false, dma: true, video: false };
        let kinds: Vec<_> = nodes(&engines).iter().map(|node| node.ip_type).collect();
        assert_eq!(kinds, [HW_IP_GFX, HW_IP_DMA]);
    }
}
