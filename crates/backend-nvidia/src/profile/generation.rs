//! What changes with a GPU's architecture. Class numbers are in the open kernel module's headers.

use super::guest::{ChannelObject, GuestBranch};
use crate::depth::DepthKinds;

/// One class bound to a channel, and the size of its parameters.
pub type ChannelClass = (u32, usize);

/// An engine's index in the RM's list of its kind, and the node type the guest's driver knows it by.
pub type IndexedNode = (usize, u32);

/// A node listed with no type in the record. The field stays at zero.
pub const NODE_TYPE_NOT_SET: u32 = 0;

/// The node type of each engine that gets a node, by engine. An engine that isn't here gets no node,
/// so the guest doesn't see it and nothing reaches it.
#[derive(Debug, PartialEq, Eq)]
pub struct EngineNodes {
    /// The display's node, straight after graphics, if the guest gets one. It takes no work here.
    pub overlay: Option<u32>,
    pub decode: &'static [IndexedNode],
    pub encode: &'static [IndexedNode],
    pub optical_flow: &'static [IndexedNode],
    pub jpeg: &'static [IndexedNode],
    /// Whether contexts and channels can be made on the video nodes. If not, they're only listed,
    /// and none of their commands reach the card.
    pub video_runs: bool,
}

/// The record's flags for video. All zero, the guest's video runtimes offer no decoders.
#[derive(Debug, PartialEq, Eq)]
pub struct VideoRecordFlags {
    /// What the card's video engines can do, as four words. The guest's runtimes choose the
    /// decoders and encoders they offer from these.
    pub caps: [u32; 4],
    pub decoder: u8,
    pub encoder: u8,
}

/// What a generation changes.
#[derive(Debug, PartialEq, Eq)]
pub struct Generation {
    pub name: &'static str,
    /// The architecture numbers the RM reports for this generation.
    pub architectures: &'static [u32],
    pub channel_class: u32,
    pub copy_class: u32,
    pub video_decode_class: u32,
    pub video_encode_class: u32,
    pub usermode_class: u32,
    /// What graphics channels bind, in subchannel order, which the guest's commands assume.
    pub graphics_classes: &'static [ChannelClass],
    /// The class of each object a guest's channel starts with.
    pub channel_objects: &'static [(ChannelObject, u32)],
    /// The shader instruction set the guest's user-mode drivers use, by the SM version the RM reports.
    /// An SM version that isn't here has never been run, so the backend refuses it and doesn't guess.
    pub shader_sets: &'static [(u32, u32)],
    pub nodes: EngineNodes,
    /// What every copy node's record says, or `None` to ask the card about each copy engine. Each
    /// bit can go alone, but a guest with none of them failed its Vulkan test.
    pub copy_node_flags: Option<u32>,
    pub video_record_flags: VideoRecordFlags,
    /// Which surfaces get a depth kind for their memory.
    pub depth_kinds: DepthKinds,
    /// Guest branches this generation has been run with. On any other the backend still starts, and
    /// says so in the log.
    pub run_on_guest_branches: &'static [&'static str],
}

const BLACKWELL_DMA_COPY_B: u32 = 0xCAB5;
const BLACKWELL_B: u32 = 0xCE97;
const BLACKWELL_COMPUTE_B: u32 = 0xCEC0;
const BLACKWELL_INLINE_TO_MEMORY_A: u32 = 0xCD40;
const ADA_A: u32 = 0xC997;
const ADA_COMPUTE_A: u32 = 0xC9C0;
const AMPERE_DMA_COPY_B: u32 = 0xC7B5;
const TURING_A: u32 = 0xC597;
const TURING_COMPUTE_A: u32 = 0xC5C0;
const TURING_DMA_COPY_A: u32 = 0xC5B5;
const KEPLER_INLINE_TO_MEMORY_B: u32 = 0xA140;
const FERMI_TWOD_A: u32 = 0x902D;
const DEFERRED_API: u32 = super::super::rm::classes::DEFERRED_API;

/// With this bit of the first word clear, NVENC opens on a Direct3D 11 device but can't set up an
/// encoder.
const BLACKWELL_VIDEO_ENCODER_SETUP: u32 = 1 << 24;
/// With this bit of the second word clear, Direct3D 11 doesn't list the encoder's profile, and NVENC
/// can't open on a Direct3D 11 device, which is what Sunshine uses.
const BLACKWELL_VIDEO_ENCODER: u32 = 1 << 26;
/// With this bit of the third word clear, Direct3D 11 offers no video decoders at all.
const BLACKWELL_VIDEO_DECODERS: u32 = 1 << 20;
/// Without the fourth word a decoder can be made, but its first frame fails as out of memory.
const BLACKWELL_VIDEO_FRAMES: u32 = 0x40_0030;
const BLACKWELL_VIDEO_CAPS: [u32; 4] = [
    BLACKWELL_VIDEO_ENCODER_SETUP,
    BLACKWELL_VIDEO_ENCODER,
    BLACKWELL_VIDEO_DECODERS,
    BLACKWELL_VIDEO_FRAMES,
];
/// Either bit is enough, and with it at zero, making a decoder fails as out of memory.
const BLACKWELL_DECODER_FLAGS: u8 = 0x48;
/// The byte after it, for the encoder. At zero, NVENC opens on a Direct3D 11 device and then fails
/// to set up an encoder. Bit 5 would do as well as this one.
const BLACKWELL_ENCODER_FLAGS: u8 = 0x10;

/// Blackwell.
pub const BLACKWELL: Generation = Generation {
    name: "Blackwell",
    architectures: &[0x1A0, 0x1B0],
    channel_class: 0xCA6F,
    copy_class: BLACKWELL_DMA_COPY_B,
    video_decode_class: 0xCFB0,
    video_encode_class: 0xCFB7,
    usermode_class: 0xC761,
    graphics_classes: &[
        (BLACKWELL_B, 16),
        (BLACKWELL_COMPUTE_B, 16),
        (BLACKWELL_INLINE_TO_MEMORY_A, 16),
        (FERMI_TWOD_A, 16),
        (BLACKWELL_DMA_COPY_B, 8),
        (DEFERRED_API, 1),
    ],
    channel_objects: &[
        (ChannelObject::InlineToMemory, BLACKWELL_INLINE_TO_MEMORY_A),
        (ChannelObject::Graphics, BLACKWELL_B),
        (ChannelObject::TwoD, FERMI_TWOD_A),
    ],
    shader_sets: &[(0xA04, 0x9004)],
    nodes: EngineNodes {
        overlay: None,
        decode: &[(0, 0x1A), (3, 0x22)],
        encode: &[(1, 0x14), (2, 0x15), (3, 0x36)],
        optical_flow: &[(0, 0x21)],
        jpeg: &[(3, 0x27)],
        video_runs: true,
    },
    copy_node_flags: Some(0x1A0),
    video_record_flags: VideoRecordFlags {
        caps: BLACKWELL_VIDEO_CAPS,
        decoder: BLACKWELL_DECODER_FLAGS,
        encoder: BLACKWELL_ENCODER_FLAGS,
    },
    depth_kinds: DepthKinds::AsStated,
    run_on_guest_branches: &["617"],
};

const OVERLAY_NODE: u32 = 0x02;

/// The nodes these generations list go in the order of the card's own list, overlay included. Video
/// nodes take no context, so none of their commands reach the card, and the record's flags tell the
/// guest's runtimes not to offer them.
const ADA_NODES: EngineNodes = EngineNodes {
    overlay: Some(OVERLAY_NODE),
    decode: &[(0, NODE_TYPE_NOT_SET)],
    encode: &[(0, NODE_TYPE_NOT_SET), (1, NODE_TYPE_NOT_SET)],
    optical_flow: &[(0, 0x21)],
    jpeg: &[],
    video_runs: false,
};
const TURING_NODES: EngineNodes = EngineNodes {
    overlay: Some(OVERLAY_NODE),
    decode: &[(0, NODE_TYPE_NOT_SET)],
    encode: &[(0, 0x13)],
    optical_flow: &[],
    jpeg: &[],
    video_runs: false,
};
const NO_VIDEO_FLAGS: VideoRecordFlags = VideoRecordFlags { caps: [0; 4], decoder: 0, encoder: 0 };

/// Ada, which keeps Ampere's channel, copy and doorbell classes. Experimental.
pub const ADA: Generation = Generation {
    name: "Ada",
    architectures: &[0x190],
    channel_class: 0xC56F,
    copy_class: AMPERE_DMA_COPY_B,
    video_decode_class: 0xC9B0,
    video_encode_class: 0xC9B7,
    usermode_class: 0xC561,
    graphics_classes: &[
        (ADA_A, 16),
        (ADA_COMPUTE_A, 16),
        (KEPLER_INLINE_TO_MEMORY_B, 16),
        (FERMI_TWOD_A, 16),
        (AMPERE_DMA_COPY_B, 8),
        (DEFERRED_API, 1),
    ],
    channel_objects: &[
        (ChannelObject::InlineToMemory, KEPLER_INLINE_TO_MEMORY_B),
        (ChannelObject::Graphics, ADA_A),
        (ChannelObject::TwoD, FERMI_TWOD_A),
    ],
    shader_sets: &[(0x809, 0x7005)],
    nodes: ADA_NODES,
    copy_node_flags: None,
    video_record_flags: NO_VIDEO_FLAGS,
    depth_kinds: DepthKinds::Always,
    run_on_guest_branches: &[],
};

/// Turing, the 20 and 16 series. Experimental. Each SM version needs its own row in `shader_sets`, so
/// a chip that reports another one is refused. The TU117's encoder has another class, 0xB4B7, which
/// makes no difference while encoders take no context.
pub const TURING: Generation = Generation {
    name: "Turing",
    architectures: &[0x160],
    channel_class: 0xC46F,
    copy_class: TURING_DMA_COPY_A,
    video_decode_class: 0xC4B0,
    video_encode_class: 0xC4B7,
    usermode_class: 0xC461,
    graphics_classes: &[
        (TURING_A, 16),
        (TURING_COMPUTE_A, 16),
        (KEPLER_INLINE_TO_MEMORY_B, 16),
        (FERMI_TWOD_A, 16),
        (TURING_DMA_COPY_A, 8),
        (DEFERRED_API, 1),
    ],
    channel_objects: &[
        (ChannelObject::InlineToMemory, KEPLER_INLINE_TO_MEMORY_B),
        (ChannelObject::Graphics, TURING_A),
        (ChannelObject::TwoD, FERMI_TWOD_A),
    ],
    shader_sets: &[(0x703, 0x6000)],
    nodes: TURING_NODES,
    copy_node_flags: None,
    video_record_flags: NO_VIDEO_FLAGS,
    depth_kinds: DepthKinds::Always,
    run_on_guest_branches: &[],
};

const GENERATIONS: [&Generation; 3] = [&BLACKWELL, &ADA, &TURING];

/// The profile for an architecture number, if this build has one.
pub fn generation_for(architecture: u32) -> Option<&'static Generation> {
    GENERATIONS.iter().copied().find(|generation| generation.architectures.contains(&architecture))
}

/// The class behind each handle the guest's driver uses, by (handle, class). The guest's branch gives
/// the handles, and the card's generation the classes.
pub fn guest_channel_classes(guest: &GuestBranch, generation: &Generation) -> Vec<(u32, u32)> {
    let class_of = |object| generation.channel_objects.iter().find(|known| known.0 == object);
    let found = guest.channel_handles.iter();
    found.filter_map(|&(handle, object)| class_of(object).map(|&(_, class)| (handle, class))).collect()
}
