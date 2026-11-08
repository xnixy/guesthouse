//! What changes with the guest driver's branch: the layouts of what it reads and writes.

/// Where things are in the guest driver's private data for an allocation.
#[derive(Debug, PartialEq, Eq)]
pub struct AllocationLayout {
    /// The first bytes of data in this format.
    pub magic: &'static [u8; 4],
    /// Where the data says how long it is, as the guest's driver wrote it.
    pub own_length: usize,
    pub size: usize,
    /// A field that held a size when the others were empty. Not confirmed.
    pub fallback_size: usize,
    pub whole_size: usize,
    pub asked_size: usize,
    pub size_limit: usize,
    pub total_size: usize,
    pub segments: usize,
    pub attributes: usize,
    pub width: usize,
    pub height: usize,
    pub depth: usize,
    pub mip_levels: usize,
    pub array_layers: usize,
    pub format: usize,
    pub colour: usize,
    pub refresh: usize,
    pub samples: usize,
    pub pitch: usize,
    pub tiling: usize,
    pub columns: usize,
    pub rows: usize,
    pub answer_width: usize,
    pub answer_height: usize,
    pub answer_pitch: usize,
    pub answer_attributes: usize,
    pub answer_attributes_2: usize,
    pub depth_bits: usize,
}

/// Where things are in the record the guest's user-mode driver reads about the adapter.
#[derive(Debug, PartialEq, Eq)]
pub struct RecordLayout {
    pub size: usize,
    pub size_field: usize,
    pub block_magic_field: usize,
    pub pci: usize,
    pub cpu_short: usize,
    pub ram_megabytes: [usize; 3],
    /// Four words of video caps.
    pub video_caps: usize,
    /// A byte of flags for video decoders whose meaning isn't known, bar what the profile says.
    pub decoder_flags: usize,
    /// The same for video encoders.
    pub encoder_flags: usize,
    pub current_clocks: usize,
    pub rated_clocks: usize,
    pub chipset: usize,
    pub architecture: usize,
    pub framebuffer_caps: usize,
    pub fifo_caps: usize,
    pub graphics_caps: usize,
    pub graphics_table: usize,
    pub framebuffer_table: usize,
    pub bus_table: usize,
    pub cpu: usize,
    pub uuid: usize,
    pub gpc_mask: usize,
    pub tpc_masks: usize,
    pub zcull: usize,
    pub ecid: usize,
    /// The RM's number for this card. The word before it stays at zero.
    pub gpu_id: usize,
    pub dedicated: usize,
    pub memory: usize,
    pub classes: usize,
    pub memory_low: usize,
    pub adapter_id: usize,
    pub name: usize,
    pub short_name: usize,
    pub host_bridge: usize,
    pub video_decode_caps: usize,
    pub jpeg_caps: usize,
    pub dma_caps: usize,
    pub node_records: usize,
    pub node_entries: usize,
    pub node_count: usize,
    pub system_ram: usize,
    /// Words that are one in every record seen.
    pub always_set: [usize; 8],
    pub driver_model_version: usize,
    pub shader_set: usize,
    pub windows_version: usize,
    pub windows_suite: usize,
    /// A word the guest's GPU discovery depends on. With it at zero, Vulkan and OpenGL both lost the
    /// GPU, and setting any other zero word near it didn't bring them back. What it means isn't known.
    pub gpu_discovery: usize,
}

/// What the guest's driver makes first on a channel, whatever the class is on this card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelObject {
    InlineToMemory,
    Graphics,
    TwoD,
}

/// A guest driver branch.
#[derive(Debug, PartialEq, Eq)]
pub struct GuestBranch {
    pub name: &'static str,
    /// The handle the guest's driver uses for each object on a channel it asks about.
    pub channel_handles: &'static [(u32, ChannelObject)],
    pub allocation: AllocationLayout,
    pub record: RecordLayout,
}

/// Branch 617.
pub const GUEST_617: GuestBranch = GuestBranch {
    name: "617",
    channel_handles: &[
        (0xFF1F_E01A, ChannelObject::InlineToMemory),
        (0xFF1F_E026, ChannelObject::Graphics),
        (0xFF1F_E02C, ChannelObject::TwoD),
    ],
    allocation: AllocationLayout {
        magic: b"ADVN",
        own_length: 0x08,
        size: 0x1CA,
        fallback_size: 0x70,
        whole_size: 0xCE,
        asked_size: 0x116,
        size_limit: 0xD6,
        total_size: 0x112,
        segments: 0x72,
        attributes: 0x122,
        width: 0x30,
        height: 0x34,
        depth: 0x38,
        mip_levels: 0x3C,
        array_layers: 0x40,
        format: 0x20,
        colour: 0x28,
        refresh: 0x50,
        samples: 0x58,
        pitch: 0x10E,
        tiling: 0x12A,
        columns: 0x132,
        rows: 0x136,
        answer_width: 0xC2,
        answer_height: 0xC6,
        answer_pitch: 0xCA,
        answer_attributes: 0xDE,
        answer_attributes_2: 0x10A,
        depth_bits: 0x11E,
    },
    record: RecordLayout {
        size: 0xC5B8,
        size_field: 0x08,
        block_magic_field: 0x0C,
        pci: 0x30,
        cpu_short: 0x40,
        ram_megabytes: [0x48, 0x4C, 0x50],
        video_caps: 0x58,
        decoder_flags: 0x1D5,
        encoder_flags: 0x1D6,
        current_clocks: 0xA8,
        rated_clocks: 0xB4,
        chipset: 0xE4,
        architecture: 0xEC,
        framebuffer_caps: 0x127,
        fifo_caps: 0x12A,
        graphics_caps: 0x12C,
        graphics_table: 0x1EB,
        framebuffer_table: 0x11AB,
        bus_table: 0x13D3,
        cpu: 0x1573,
        uuid: 0x15CB,
        gpc_mask: 0x18BB,
        tpc_masks: 0x18DB,
        zcull: 0x678B,
        ecid: 0x6ADF,
        gpu_id: 0x3E24,
        dedicated: 0x708F,
        memory: 0xB128,
        classes: 0xB158,
        memory_low: 0xB484,
        adapter_id: 0xC46C,
        name: 0xC4B8,
        short_name: 0xC538,
        host_bridge: 0xC578,
        video_decode_caps: 0x144,
        jpeg_caps: 0x18A,
        dma_caps: 0x1CDB,
        node_records: 0x1D02,
        node_entries: 0x6AE7,
        node_count: 0x6FBF,
        system_ram: 0x702B,
        always_set: [0xC450, 0xC45C, 0xC464, 0xC468, 0xC480, 0xC494, 0xC498, 0xC4B4],
        driver_model_version: 0x1763,
        shader_set: 0x1893,
        windows_version: 0x1767,
        windows_suite: 0x187F,
        gpu_discovery: 0xC488,
    },
};

const BRANCHES: [&GuestBranch; 1] = [&GUEST_617];

/// The profile for a guest branch this build knows.
pub fn guest_branch(name: &str) -> Option<&'static GuestBranch> {
    BRANCHES.iter().copied().find(|branch| branch.name == name)
}
