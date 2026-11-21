//! The adapter block AMD's Windows driver reads at start, built from the host card.
//!
//! The guest asks for it with an adapter query of type 0, the WDK's DXGKQAITYPE_UMDRIVERPRIVATE.
//! The kernel's answer to that query is [`super::adapter_header`], then this block of [`BLOCK_SIZE`]
//! bytes when the buffer has room for both. AMD's open interop header calls the structure
//! ADAPTERINFOEX. All of it is the kernel driver's to fill in, and the guest only reads it, so each
//! field here is one of three things:
//!
//! - read off the host card through amdgpu's INFO queries, which the field's comment names;
//! - a fixed value this backend chooses, for what a guest in a virtual machine should be told;
//! - left zero, because nothing on the host says what it should be.
//!
//! Every field not named below is zero. The last dword folded into [`CHECKSUM`] covers the whole
//! block, so the zeros are part of the answer too.

use super::adapter_header;
use super::adapter_records::{ADAPTER_RECORD_SIZE, DIRECTORY_SIZE, adapter_record, directory};
use super::allocation::SURFACE_SIZE;
use super::bytes::{first_dwords, index_xor_sum, put_dword, put_qword, put_word};
use crate::info::{CardDetails, DeviceInfo};
use crate::nodes::nodes;
use guesthouse_backend::{GPU_RANGES, Status, VendorQuery};

/// The adapter query type that asks for the block, DXGKQAITYPE_UMDRIVERPRIVATE.
pub const QUERY_TYPE: u32 = 0;
/// The block's size. It follows the header in the query's answer.
pub const BLOCK_SIZE: usize = 0x3A00;

// The block's start. Fixed values.
/// The block's version, major and minor. The guest's user-mode driver keeps a copy and doesn't
/// check either, but these are what AMD's own kernel driver says.
const MAJOR_VERSION: usize = 0x000;
const MINOR_VERSION: usize = 0x004;
const BLOCK_VERSION: (u32, u32) = (1, 2);
// VersionFlags at 0x008 stays zero. Its one known bit, bit 0, says the second virtual-memory block
// (alignments at 0x3190 and 0x338C, and the IOMMU flags) is valid. The guest's user-mode driver
// loads none of those fields, so a clear bit and zero fields match everything it reads. The display
// minimum alignments at 0x3390 and 0x3398 are read only when bit 2 of the dword at 0x2D50 is set,
// and that dword is in the range left zero.
/// The surface descriptor size the kernel expects in allocation private data. The guest steps
/// through its descriptors by this size only when [`FEATURE_SURFACE_STRIDE_GIVEN`] is set.
const SURFACE_DESCRIPTOR_SIZE: usize = 0x00C;
const SYSTEM_FLAGS: usize = 0x208;
/// The guest runs in a virtual machine. The secure-VM, IOMMU, Thunderbolt and USB4 bits stay clear.
const SYSTEM_IN_VIRTUAL_MACHINE: u32 = 1 << 1;
/// How many GPUs the adapter links. One, with linking off.
const GPU_COUNT: usize = 0x608;

// Firmware versions, from FW_VERSION. Video firmware stays zero: the guest's driver offers video
// without it.
const FIRMWARE_SDMA0: usize = 0x88C;
const FIRMWARE_SDMA1: usize = 0x890;
const FIRMWARE_SMU: usize = 0x8E8;
const FIRMWARE_RLC: usize = 0x8EC;
const FIRMWARE_ME: usize = 0x8F0;
const FIRMWARE_MEC: usize = 0x8F4;
const FIRMWARE_PFP: usize = 0x8F8;
const FIRMWARE_CE: usize = 0x8FC;
const FIRMWARE_MEMORY_CONTROLLER: usize = 0x900;
const FIRMWARE_PSP_SOS: usize = 0x908;
const FIRMWARE_MES: usize = 0x938;
/// The smallest pitch alignment, in pixels. Nothing on the host says; this is a choice, see
/// [`MIN_PITCH_ALIGN_PIXELS`].
const MIN_PITCH_ALIGN: usize = 0x888;
/// 256 pixels is at least 256 bytes for any pixel size, which is the linear pitch rule amdgpu and
/// Mesa follow on every GFX9 and later part. A larger alignment than needed is always still valid.
const MIN_PITCH_ALIGN_PIXELS: u32 = 256;

// The first GPU's own block. From DEV_INFO and MEMORY unless said otherwise.
const CHIP_ID: usize = 0xA00;
/// amdgpu's AMDGPU_FAMILY_* number, which is the numbering the guest compares against.
const FAMILY: usize = 0xA04;
/// A stepping inside the family, not the PCI revision. The guest buckets it by ranges that match
/// amdgpu's external_rev bases per chip, so INFO's external_rev goes here. Medium confidence.
const EMULATED_REVISION: usize = 0xA08;
// GfxEngineID at 0xA10 stays zero. The guest copies it and compares it with nothing, and no
// numbering for it is known.
/// From sysfs, as INFO has no subsystem. One dword; the guest splits it nowhere, so the config-space
/// packing, vendor in the low half, is unconfirmed.
const SUBSYSTEM_ID: usize = 0xA0C;
const LOCAL_VISIBLE_HEAP: usize = 0xA14;
const LOCAL_INVISIBLE_HEAP: usize = 0xA1C;
/// Taken as hertz, as INFO gives kilohertz. Unconfirmed: neither the guest nor the kernel driver
/// scales it, so its unit isn't shown. The clocks in megahertz are separate words, and amdgpu's
/// header calls the host's figure kilohertz, but nothing shows the guest's dword is the same.
const COUNTER_FREQUENCY: usize = 0xA24;
/// Hertz in a kilohertz, the guess above.
const COUNTER_HERTZ_PER_KILOHERTZ: u32 = 1000;
const ENGINE_CLOCK_MHZ: usize = 0xA28;
const MEMORY_CLOCK_MHZ: usize = 0xA2A;
const REGISTER_PROTECTION: usize = 0xA2C;
/// Registers are protected. The guest's command buffers run under the host kernel, which never lets
/// a user's work write privileged registers, so the guest must not count on it. The two bits that
/// would open the RLC and the copy engine's registers stay clear. A fixed choice.
const REGISTERS_PROTECTED: u32 = 1 << 0;
const LOCAL_VISIBLE_SEGMENT: usize = 0xA30;
const LOCAL_INVISIBLE_SEGMENT: usize = 0xA38;
const REVISION_ID: usize = 0xA40;
/// The host's memory the card can reach, which the guest's driver budgets its system heaps by. Read
/// only when [`FEATURE_HOST_MEMORY_SIZE_GIVEN`] is set.
const HOST_MEMORY_SIZE: usize = 0xA44;
/// The graphics engine's generation. The guest copies it and compares it nowhere that was found.
const GFX_ENGINE_ID: usize = 0xA10;
/// What AMD's kernel driver gives every part from GFX9 on.
const GFX_ENGINE_GFX9_AND_LATER: u32 = 0xD;

// The graphics part of the first GPU's block, the GFX9-and-later member of its family union.
const GB_ADDR_CONFIG: usize = 0xC00;
/// The high half of the render backend disable word: one bit per render backend that is off.
const DISABLED_RB_MASK: usize = 0xC06;
const CE_RAM_SIZE: usize = 0xC08;
/// 16-bit masks of active units, two per shader engine, one for each array, with room for four
/// engines. The guest copies [`SHADER_ENGINES`] times four bytes of them and counts each set bit as
/// one unit, with no doubling after: a unit is a compute unit before GFX10 and a workgroup
/// processor from GFX10 on, so a GFX10 count is a workgroup count.
const ACTIVE_UNITS: usize = 0xC20;
/// The units kept on all the time, laid out the same way.
const ALWAYS_ON_UNITS: usize = 0xC30;
const GRAPHICS_FLAGS: usize = 0xC44;
/// The counts from [`MAX_VGPRS`] on are filled in.
const GRAPHICS_COUNTS_VALID: u32 = 1 << 0;
/// [`ACTIVE_UNITS`] and [`ALWAYS_ON_UNITS`] are filled in. Without this bit, or the one for the
/// per-engine state arrays (bit 16, whose arrays this block leaves empty), the guest reads no unit
/// masks at all. Whether a GFX10.3 card sets bit 16 on a real host isn't known; this bit's path is
/// the one that reads the masks held here.
const UNIT_MASKS_VALID: u32 = 1 << 2;
/// [`TILE_STEERING_OVERRIDE`] is filled in.
const TILE_STEERING_VALID: u32 = 1 << 4;
const MAX_VGPRS: usize = 0xC58;
const WAVEFRONT_SIZE: usize = 0xC5C;
const SHADER_ENGINES: usize = 0xC60;
const RB_PER_ENGINE: usize = 0xC64;
/// Arrays in each engine, not in all of them: the guest walks the masks by this and the engine
/// count as two bounds.
const ARRAYS_PER_ENGINE: usize = 0xC68;
const CUS_PER_ARRAY: usize = 0xC6C;
const TCC_BLOCKS: usize = 0xC74;
const MAX_GS_WAVES: usize = 0xC78;
const GS_VGT_TABLE_DEPTH: usize = 0xC80;
const GS_PRIM_BUFFER_DEPTH: usize = 0xC84;
const COMPUTE_RINGS: usize = 0xC88;
const TILE_STEERING_OVERRIDE: usize = 0xCE0;

/// One byte for each of the guest's nodes: AMD's number for the engine behind it, or
/// [`NO_ENGINE`]. The guest's user-mode driver picks each node's queue type from this.
const NODE_ENGINES: usize = 0x89C;
const NODE_SLOTS: usize = 64;
const NO_ENGINE: u8 = 0xFF;

/// For each node, where its command buffers may start and how their size rounds, in bytes, two
/// dwords a node. Left zero, the guest's driver takes the card to have no virtual memory, builds its
/// command buffers in physical memory and fails.
const NODE_IB_ALIGNMENT: usize = 0x318C;
const NODE_IB_ALIGNMENT_STRIDE: usize = 8;

/// The card's virtual memory, as the guest's driver lays out its own addresses by it. It needs
/// at least 28 GiB of span and reserves 4 GiB blocks from it.
const VM_INFO_VALID: usize = 0x2A00;
/// The first address and the last one, inclusive.
const VM_FIRST_ADDRESS: usize = 0x2A08;
const VM_LAST_ADDRESS: usize = 0x2A10;
/// The page-table geometry, which is the same on every GFX9 and later card: 8-byte directory and
/// table entries, 2 MiB under each directory entry and 64 KiB fragments.
const VM_PDE_SIZE: usize = 0x2A18;
const VM_PTE_SIZE: usize = 0x2A1C;
const VM_SPACE_PER_PDE: usize = 0x2A20;
const VM_FRAGMENT_SIZE: usize = 0x2A24;
const PAGE_TABLE_ENTRY_BYTES: u32 = 8;
const SPACE_PER_PDE: u32 = 2 << 20;
const FRAGMENT_BYTES: u32 = 64 << 10;

/// The software features the kernel offers the guest. Fixed.
const SOFTWARE_FEATURES: usize = 0x2E00;
/// Allocation private data may hold more than one surface descriptor.
const FEATURE_SEVERAL_SURFACES: u32 = 1 << 2;
/// Allocation private data may carry the second information block after its surface descriptors.
const FEATURE_ALLOCATION_SECOND_BLOCK: u32 = 1 << 3;
/// The guest steps through surface descriptors by [`SURFACE_DESCRIPTOR_SIZE`]. Clear, it uses a
/// smaller fixed size that doesn't match the descriptors the kernel checks.
const FEATURE_SURFACE_STRIDE_GIVEN: u32 = 1 << 20;
/// [`HOST_MEMORY_SIZE`] is filled in.
const FEATURE_HOST_MEMORY_SIZE_GIVEN: u32 = 1 << 22;
/// An allocation's private data must fit in 128 KiB, which is the most the kernel takes and the
/// most Windows carries from a guest to its host. Clear, the guest's Direct3D 12 driver describes
/// every mip of every layer of a texture in one allocation's data, and a big texture array is then
/// refused inside the guest before the host hears of it, which costs the program its device. Set,
/// the driver keeps each allocation's data small.
const FEATURE_PRIVATE_DATA_FITS_128K: u32 = 1 << 23;
/// Bit 1, render status, stays clear: it promises command-buffer handling of render status that
/// nothing here does, though the guest doesn't test it. Bit 13 promises a GPU virtual-address
/// offset in the adapter block, bit 17 that the guest may send fullscreen frame metadata in an
/// escape, and bit 24 a hardware scheduler's queue sizes: this backend honours none of those, so
/// all stay clear.
const FEATURES: u32 = FEATURE_SEVERAL_SURFACES
    | FEATURE_ALLOCATION_SECOND_BLOCK
    | FEATURE_SURFACE_STRIDE_GIVEN
    | FEATURE_HOST_MEMORY_SIZE_GIVEN
    | FEATURE_PRIVATE_DATA_FITS_128K;

/// The sum the guest checks the block against, kept in the block. See [`index_xor_sum`].
const CHECKSUM: usize = 0x2E20;

// DriverStorePath at 0x3128 stays zero. The guest takes an empty path and goes on, and a host path
// means nothing in the guest. The private and shared apertures from 0x2D54 stay zero as well: the
// guest's user-mode driver doesn't read them.

/// The newest submission checksum the kernel takes, which bounds the version a submission's
/// checksum record may give. One is AMD's own default.
const CHECKSUM_VERSION: usize = 0x35E0;
pub const SUBMISSION_CHECKSUM_VERSION: u32 = 1;

/// Two dwords the guest's driver wants set before it offers any video: with either left zero it
/// lists no hardware decoder and finds no encoder, whatever nodes it is given. What they mean isn't
/// known. The values are the ones a Radeon 890M's own driver has in its block, and the only ones
/// that were tried. They are filled in only when the video ring is offered: see [`crate::video`].
const VIDEO_FIRST: usize = 0x366C;
const VIDEO_FIRST_AS_SEEN: u32 = 4;
const VIDEO_SECOND: usize = 0x36EC;
const VIDEO_SECOND_AS_SEEN: u32 = 1;

/// The family union's arrays have room for four shader engines with two arrays each.
const ENGINES_IN_BLOCK: usize = 4;
const ARRAYS_IN_BLOCK: usize = 2;
/// From GFX10 a workgroup processor is two compute units, and the block counts processors.
const FIRST_GFX_WITH_WGPS: u32 = 10;

/// Answers the adapter query, if it is the one for this block.
pub fn answer(query: &VendorQuery<'_>, info: &DeviceInfo, out: &mut Vec<u8>) -> Option<Status> {
    if query.kind != QUERY_TYPE {
        return None;
    }
    let header = adapter_header::HEADER_SIZE;
    // The kernel's answer is the header, then the block if there is room for it. The user-mode
    // driver opens the adapter with the header alone. A buffer of just the block is answered with
    // the block, as an older path of the kernel answers it.
    let answer = match query.size {
        size if size == header => adapter_header::build(info, size),
        size if size == header + BLOCK_SIZE => [adapter_header::build(info, size), build(info)].concat(),
        BLOCK_SIZE => build(info),
        size if size == header + ADAPTER_RECORD_SIZE => {
            [adapter_header::build(info, size), adapter_record(&info.details)].concat()
        }
        size if size == header + DIRECTORY_SIZE => [adapter_header::build(info, size), directory()].concat(),
        _ => Vec::new(),
    };
    if answer.is_empty() {
        log::info!(
            "adapter block refused: the guest asked for {} bytes, with {} bytes of input ({})",
            query.size,
            query.data.len(),
            first_dwords(query.data, 4)
        );
        return Some(Status::INVALID_PARAMETER);
    }
    log::debug!("answered the adapter block, {} bytes", query.size);
    out.extend_from_slice(&answer);
    Some(Status::SUCCESS)
}

/// The whole block for this card.
pub fn build(info: &DeviceInfo) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    put_dword(&mut block, MAJOR_VERSION, BLOCK_VERSION.0);
    put_dword(&mut block, MINOR_VERSION, BLOCK_VERSION.1);
    put_dword(&mut block, SURFACE_DESCRIPTOR_SIZE, SURFACE_SIZE as u32);
    put_dword(&mut block, SYSTEM_FLAGS, SYSTEM_IN_VIRTUAL_MACHINE);
    put_dword(&mut block, GPU_COUNT, 1);
    put_dword(&mut block, MIN_PITCH_ALIGN, MIN_PITCH_ALIGN_PIXELS);
    fill_firmware(&mut block, info);
    fill_first_gpu(&mut block, info);
    fill_graphics(&mut block, &info.details);
    fill_nodes(&mut block, info);
    fill_virtual_memory(&mut block);
    put_dword(&mut block, SOFTWARE_FEATURES, FEATURES);
    put_dword(&mut block, CHECKSUM_VERSION, SUBMISSION_CHECKSUM_VERSION);
    if info.engines.video {
        put_dword(&mut block, VIDEO_FIRST, VIDEO_FIRST_AS_SEEN);
        put_dword(&mut block, VIDEO_SECOND, VIDEO_SECOND_AS_SEEN);
    }
    let sum = index_xor_sum(&block, BLOCK_SIZE / 4, CHECKSUM);
    put_dword(&mut block, CHECKSUM, sum);
    block
}

fn fill_nodes(block: &mut [u8], info: &DeviceInfo) {
    block[NODE_ENGINES..NODE_ENGINES + NODE_SLOTS].fill(NO_ENGINE);
    for (index, node) in nodes(&info.engines).into_iter().enumerate() {
        block[NODE_ENGINES + index] = node.guest_engine;
        let alignment = info.ib_alignment.of(node.ip_type);
        let at = NODE_IB_ALIGNMENT + index * NODE_IB_ALIGNMENT_STRIDE;
        put_dword(block, at, alignment.start);
        put_dword(block, at + 4, alignment.size);
    }
}

/// The range the guest's driver places its own addresses in, which is the upper of the ranges every
/// space covers. The lower one is below 4 GiB and too small for it.
fn fill_virtual_memory(block: &mut [u8]) {
    let range = GPU_RANGES[GPU_RANGES.len() - 1];
    put_dword(block, VM_INFO_VALID, 1);
    put_qword(block, VM_FIRST_ADDRESS, range.start);
    put_qword(block, VM_LAST_ADDRESS, range.end - 1);
    put_dword(block, VM_PDE_SIZE, PAGE_TABLE_ENTRY_BYTES);
    put_dword(block, VM_PTE_SIZE, PAGE_TABLE_ENTRY_BYTES);
    put_dword(block, VM_SPACE_PER_PDE, SPACE_PER_PDE);
    put_dword(block, VM_FRAGMENT_SIZE, FRAGMENT_BYTES);
}

fn fill_firmware(block: &mut [u8], info: &DeviceInfo) {
    let firmware = &info.details.firmware;
    let versions = [
        (FIRMWARE_SDMA0, firmware.sdma0),
        (FIRMWARE_SDMA1, firmware.sdma1),
        (FIRMWARE_SMU, firmware.smc),
        (FIRMWARE_RLC, firmware.rlc),
        (FIRMWARE_ME, firmware.me),
        (FIRMWARE_MEC, firmware.mec),
        (FIRMWARE_PFP, firmware.pfp),
        (FIRMWARE_CE, firmware.ce),
        (FIRMWARE_MEMORY_CONTROLLER, firmware.memory_controller),
        (FIRMWARE_PSP_SOS, firmware.sos),
        (FIRMWARE_MES, firmware.mes),
    ];
    for (at, version) in versions {
        put_dword(block, at, version);
    }
}

fn megahertz(kilohertz: u64) -> u16 {
    (kilohertz / 1000).try_into().unwrap_or(u16::MAX)
}

fn fill_first_gpu(block: &mut [u8], info: &DeviceInfo) {
    let details = &info.details;
    let invisible = info.vram_total.saturating_sub(info.visible_vram_total);
    put_dword(block, CHIP_ID, u32::from(info.device_id));
    put_dword(block, FAMILY, info.family);
    put_dword(block, EMULATED_REVISION, details.external_revision);
    put_dword(block, SUBSYSTEM_ID, details.subsystem);
    put_qword(block, LOCAL_VISIBLE_HEAP, info.visible_vram_total);
    put_qword(block, LOCAL_INVISIBLE_HEAP, invisible);
    put_dword(block, COUNTER_FREQUENCY, info.counter_freq_khz.saturating_mul(COUNTER_HERTZ_PER_KILOHERTZ));
    put_word(block, ENGINE_CLOCK_MHZ, megahertz(details.max_engine_clock_khz));
    put_word(block, MEMORY_CLOCK_MHZ, megahertz(details.max_memory_clock_khz));
    put_dword(block, REGISTER_PROTECTION, REGISTERS_PROTECTED);
    put_qword(block, LOCAL_VISIBLE_SEGMENT, info.visible_vram_total);
    put_qword(block, LOCAL_INVISIBLE_SEGMENT, invisible);
    put_dword(block, REVISION_ID, u32::from(info.pci_revision));
    put_qword(block, HOST_MEMORY_SIZE, info.gtt_total);
    put_dword(block, GFX_ENGINE_ID, GFX_ENGINE_GFX9_AND_LATER);
}

/// One unit per two compute units, counted only when both are active, as amdgpu turns them off in
/// pairs anyway.
fn workgroup_mask(compute_units: u32) -> u32 {
    (0..16).filter(|pair| compute_units >> (pair * 2) & 0b11 == 0b11).fold(0, |mask, pair| mask | 1 << pair)
}

/// The eight 16-bit unit masks the block holds, from INFO's engine-by-array bitmap.
pub fn unit_masks(bitmap: &[[u32; 4]; 4], gfx_major: u32) -> [u16; ENGINES_IN_BLOCK * ARRAYS_IN_BLOCK] {
    let mut masks = [0; ENGINES_IN_BLOCK * ARRAYS_IN_BLOCK];
    for engine in 0..ENGINES_IN_BLOCK {
        for array in 0..ARRAYS_IN_BLOCK {
            let units = bitmap[engine][array];
            let units = if gfx_major >= FIRST_GFX_WITH_WGPS { workgroup_mask(units) } else { units };
            masks[engine * ARRAYS_IN_BLOCK + array] = units as u16;
        }
    }
    masks
}

fn put_masks(block: &mut [u8], at: usize, masks: &[u16]) {
    for (index, &mask) in masks.iter().enumerate() {
        put_word(block, at + index * 2, mask);
    }
}

fn fill_graphics(block: &mut [u8], details: &CardDetails) {
    let gfx_major = details.gfx_version.0;
    let rb_pipes = details.rb_pipes.min(64);
    let present = if rb_pipes == 64 { u64::MAX } else { (1u64 << rb_pipes) - 1 };
    let disabled = !details.enabled_rb_mask & present;
    put_dword(block, GB_ADDR_CONFIG, details.gb_addr_config.unwrap_or(0));
    put_word(block, DISABLED_RB_MASK, disabled as u16);
    put_dword(block, CE_RAM_SIZE, details.ce_ram_size);
    put_masks(block, ACTIVE_UNITS, &unit_masks(&details.cu_bitmap, gfx_major));
    put_masks(block, ALWAYS_ON_UNITS, &unit_masks(&details.always_on_cu_bitmap, gfx_major));

    let mut flags = GRAPHICS_COUNTS_VALID | UNIT_MASKS_VALID;
    if gfx_major >= FIRST_GFX_WITH_WGPS && details.tile_steering_override != 0 {
        flags |= TILE_STEERING_VALID;
        put_dword(block, TILE_STEERING_OVERRIDE, details.tile_steering_override);
    }
    put_dword(block, GRAPHICS_FLAGS, flags);
    let rb_per_engine = details.rb_pipes.checked_div(details.shader_engines).unwrap_or(0);
    let counts = [
        (MAX_VGPRS, details.visible_vgprs),
        (WAVEFRONT_SIZE, details.wavefront_size),
        (SHADER_ENGINES, details.shader_engines),
        (RB_PER_ENGINE, rb_per_engine),
        (ARRAYS_PER_ENGINE, details.arrays_per_engine),
        (CUS_PER_ARRAY, details.cus_per_array),
        (TCC_BLOCKS, details.tcc_blocks),
        (MAX_GS_WAVES, details.max_gs_waves_per_vgt),
        (GS_VGT_TABLE_DEPTH, details.gs_vgt_table_depth),
        (GS_PRIM_BUFFER_DEPTH, details.gs_prim_buffer_depth),
        (COMPUTE_RINGS, details.compute_rings),
    ];
    for (at, value) in counts {
        put_dword(block, at, value);
    }
}

/// The fields a person checks against what amdgpu says, read back out of a built block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFields {
    pub chip_id: u32,
    pub family: u32,
    pub in_virtual_machine: bool,
    pub visible_heap: u64,
    pub invisible_heap: u64,
    pub gb_addr_config: u32,
    pub shader_engines: u32,
    pub arrays_per_engine: u32,
    pub cus_per_array: u32,
    /// Set bits across every active-unit mask, in the block's own unit.
    pub active_units: u32,
    pub engine_clock_mhz: u16,
    pub checksum_holds: bool,
}

impl KeyFields {
    pub fn read(block: &[u8]) -> Option<Self> {
        use super::bytes::{dword, qword};
        let word = |at: usize| block.get(at..at + 2).map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
        let active_units = (0..ENGINES_IN_BLOCK * ARRAYS_IN_BLOCK)
            .map(|index| word(ACTIVE_UNITS + index * 2).map(u16::count_ones))
            .sum::<Option<u32>>()?;
        Some(Self {
            chip_id: dword(block, CHIP_ID)?,
            family: dword(block, FAMILY)?,
            in_virtual_machine: dword(block, SYSTEM_FLAGS)? & SYSTEM_IN_VIRTUAL_MACHINE != 0,
            visible_heap: qword(block, LOCAL_VISIBLE_HEAP)?,
            invisible_heap: qword(block, LOCAL_INVISIBLE_HEAP)?,
            gb_addr_config: dword(block, GB_ADDR_CONFIG)?,
            shader_engines: dword(block, SHADER_ENGINES)?,
            arrays_per_engine: dword(block, ARRAYS_PER_ENGINE)?,
            cus_per_array: dword(block, CUS_PER_ARRAY)?,
            active_units,
            engine_clock_mhz: word(ENGINE_CLOCK_MHZ)?,
            checksum_holds: block.len() == BLOCK_SIZE
                && index_xor_sum(block, BLOCK_SIZE / 4, CHECKSUM) == dword(block, CHECKSUM)?,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::info::{EngineSet, Firmware, IbAlignment, IbAlignments};

    const VERSION_FLAGS: usize = 0x008;
    const DISPLAY_ALIGNMENT_FLAGS: usize = 0x2D50;

    /// A small RDNA2 part like the host's: one engine, one array of two units.
    pub(crate) fn small_card() -> DeviceInfo {
        let mut cu_bitmap = [[0; 4]; 4];
        cu_bitmap[0][0] = 0b11;
        DeviceInfo {
            device_id: 0x1234,
            pci_revision: 0xC1,
            family: 149,
            active_compute_units: 2,
            is_apu: true,
            vram_total: 2 << 30,
            visible_vram_total: 2 << 30,
            gtt_total: 8 << 30,
            counter_freq_khz: 100_000,
            engines: EngineSet { graphics: true, compute: true, dma: true, video: false },
            ib_alignment: IbAlignments {
                graphics: IbAlignment { start: 32, size: 32 },
                compute: IbAlignment { start: 32, size: 32 },
                dma: IbAlignment { start: 256, size: 4 },
                video: IbAlignment { start: 256, size: 4 },
            },
            details: CardDetails {
                external_revision: 0x9,
                shader_engines: 1,
                arrays_per_engine: 1,
                max_engine_clock_khz: 2_200_000,
                cu_bitmap,
                always_on_cu_bitmap: cu_bitmap,
                enabled_rb_mask: 0b01,
                rb_pipes: 2,
                wavefront_size: 64,
                cus_per_array: 2,
                gfx_version: (10, 3),
                compute_rings: 4,
                gb_addr_config: Some(0x42),
                firmware: Firmware { me: 7, mec: 9, ..Firmware::default() },
                tile_steering_override: 0x5,
                ..CardDetails::default()
            },
            ..DeviceInfo::default()
        }
    }

    fn qword_at(block: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(block[at..at + 8].try_into().unwrap())
    }

    fn dword_at(block: &[u8], at: usize) -> u32 {
        super::super::bytes::dword(block, at).unwrap()
    }

    #[test]
    fn the_block_is_the_size_asked_for_and_its_sum_holds() {
        let block = build(&small_card());
        assert_eq!(block.len(), BLOCK_SIZE);
        assert!(KeyFields::read(&block).unwrap().checksum_holds);
    }

    #[test]
    fn a_changed_byte_breaks_the_sum() {
        let mut block = build(&small_card());
        block[0x10] ^= 1;
        assert!(!KeyFields::read(&block).unwrap().checksum_holds);
    }

    #[test]
    fn the_guest_is_told_it_is_in_a_virtual_machine_and_may_send_the_second_block() {
        let block = build(&small_card());
        assert_eq!(dword_at(&block, SYSTEM_FLAGS), SYSTEM_IN_VIRTUAL_MACHINE);
        let features = dword_at(&block, SOFTWARE_FEATURES);
        assert_ne!(features & FEATURE_ALLOCATION_SECOND_BLOCK, 0);
        assert_ne!(features & FEATURE_SEVERAL_SURFACES, 0);
        assert_eq!(dword_at(&block, SURFACE_DESCRIPTOR_SIZE), SURFACE_SIZE as u32);
    }

    #[test]
    fn the_block_gives_its_version() {
        let block = build(&small_card());
        assert_eq!((dword_at(&block, MAJOR_VERSION), dword_at(&block, MINOR_VERSION)), (1, 2));
        assert_eq!(dword_at(&block, VERSION_FLAGS), 0);
    }

    #[test]
    fn only_the_features_this_backend_can_honour_are_set() {
        let features = dword_at(&build(&small_card()), SOFTWARE_FEATURES);
        assert_eq!(features, (1 << 2) | (1 << 3) | (1 << 20) | (1 << 22) | (1 << 23));
        for bit in [1, 13, 17, 24] {
            assert_eq!(features & 1 << bit, 0, "bit {bit}");
        }
    }

    #[test]
    fn the_guest_is_told_to_keep_private_data_within_what_the_kernel_takes() {
        let features = dword_at(&build(&small_card()), SOFTWARE_FEATURES);
        assert_ne!(features & FEATURE_PRIVATE_DATA_FITS_128K, 0);
        assert_eq!(crate::protocol::allocation::MAX_SIZE, 128 << 10);
    }

    #[test]
    fn each_node_names_its_engine_and_its_command_buffer_alignment() {
        let block = build(&small_card());
        assert_eq!(&block[NODE_ENGINES..NODE_ENGINES + 4], &[0, 5, 4, NO_ENGINE]);
        assert!(block[NODE_ENGINES + 3..NODE_ENGINES + NODE_SLOTS].iter().all(|&byte| byte == NO_ENGINE));
        let words: Vec<u32> = (0..8).map(|index| dword_at(&block, NODE_IB_ALIGNMENT + index * 4)).collect();
        assert_eq!(words, [32, 32, 32, 32, 256, 4, 0, 0]);
    }

    /// [`small_card`] with its video ring offered to the guest.
    fn card_offering_video() -> DeviceInfo {
        let mut card = small_card();
        card.video_ring = true;
        card.engines.video = true;
        card
    }

    #[test]
    fn a_card_that_offers_video_names_the_codec_node_and_sets_the_two_video_dwords() {
        let block = build(&card_offering_video());
        assert_eq!(&block[NODE_ENGINES..NODE_ENGINES + 5], &[0, 5, 4, 41, NO_ENGINE]);
        let at = NODE_IB_ALIGNMENT + 3 * NODE_IB_ALIGNMENT_STRIDE;
        assert_eq!((dword_at(&block, at), dword_at(&block, at + 4)), (256, 4));
        assert_eq!((dword_at(&block, VIDEO_FIRST), dword_at(&block, VIDEO_SECOND)), (4, 1));
        assert!(KeyFields::read(&block).unwrap().checksum_holds);
    }

    /// The sum the block of [`small_card`] held before there was any video to offer. The sum is
    /// over every dword of the block, so it stands for the whole of it.
    const SUM_BEFORE_VIDEO: u32 = 0x4655_C071;

    #[test]
    fn a_card_that_offers_no_video_is_told_as_it_was_before() {
        let block = build(&small_card());
        assert_eq!(dword_at(&block, CHECKSUM), SUM_BEFORE_VIDEO);
        assert!(KeyFields::read(&block).unwrap().checksum_holds);
        assert_eq!((dword_at(&block, VIDEO_FIRST), dword_at(&block, VIDEO_SECOND)), (0, 0));
        // The ring being there changes nothing by itself: the guest hears of it only when it is offered.
        let with_a_ring = DeviceInfo { video_ring: true, ..small_card() };
        assert_eq!(build(&with_a_ring), block);
        // Offered, the block differs in the node's two places, the two dwords and the sum, and nowhere else.
        let offered = build(&card_offering_video());
        let changed: Vec<usize> =
            (0..BLOCK_SIZE).step_by(4).filter(|&at| block[at..at + 4] != offered[at..at + 4]).collect();
        let node = NODE_IB_ALIGNMENT + 3 * NODE_IB_ALIGNMENT_STRIDE;
        assert_eq!(changed, [NODE_ENGINES, CHECKSUM, node, node + 4, VIDEO_FIRST, VIDEO_SECOND]);
    }

    #[test]
    fn the_virtual_memory_is_the_upper_range_with_gfx9_page_tables() {
        let block = build(&small_card());
        assert_eq!(dword_at(&block, VM_INFO_VALID), 1);
        assert_eq!(qword_at(&block, VM_FIRST_ADDRESS), 0x1_4000_0000);
        assert_eq!(qword_at(&block, VM_LAST_ADDRESS), 0xFF_FFFF_FFFF);
        let geometry: Vec<u32> = [VM_PDE_SIZE, VM_PTE_SIZE, VM_SPACE_PER_PDE, VM_FRAGMENT_SIZE]
            .into_iter()
            .map(|at| dword_at(&block, at))
            .collect();
        assert_eq!(geometry, [8, 8, 0x20_0000, 0x1_0000]);
    }

    #[test]
    fn the_host_memory_size_and_the_engine_generation_are_given() {
        let block = build(&small_card());
        assert_eq!(qword_at(&block, HOST_MEMORY_SIZE), 8 << 30);
        assert_eq!(dword_at(&block, GFX_ENGINE_ID), 0xD);
    }

    #[test]
    fn the_version_flags_and_display_alignment_stay_unpromised() {
        let block = build(&small_card());
        assert_eq!(dword_at(&block, VERSION_FLAGS), 0);
        assert_eq!(dword_at(&block, DISPLAY_ALIGNMENT_FLAGS), 0);
    }

    #[test]
    fn the_guest_steps_through_descriptors_by_the_size_the_kernel_checks() {
        let block = build(&small_card());
        assert_ne!(dword_at(&block, SOFTWARE_FEATURES) & FEATURE_SURFACE_STRIDE_GIVEN, 0);
        assert_eq!(dword_at(&block, SURFACE_DESCRIPTOR_SIZE), 0x120);
    }

    #[test]
    fn submissions_may_use_either_checksum_version() {
        let block = build(&small_card());
        assert_eq!(dword_at(&block, CHECKSUM_VERSION), 1);
        assert_eq!(dword_at(&block, CHECKSUM_VERSION), SUBMISSION_CHECKSUM_VERSION);
    }

    #[test]
    fn the_driver_store_path_and_the_apertures_are_left_empty() {
        let block = build(&small_card());
        let driver_store_path = 0x3128..0x3128 + 0x64;
        assert!(block[driver_store_path].iter().all(|&byte| byte == 0));
        let apertures = 0x2D54..0x2DA0;
        assert!(block[apertures].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn the_card_facts_land_where_the_guest_reads_them() {
        let block = build(&small_card());
        let key = KeyFields::read(&block).unwrap();
        assert_eq!(key.chip_id, 0x1234);
        assert_eq!(key.family, 149);
        assert_eq!(key.visible_heap, 2 << 30);
        assert_eq!(key.invisible_heap, 0);
        assert_eq!(key.gb_addr_config, 0x42);
        assert_eq!(key.engine_clock_mhz, 2200);
        assert_eq!(dword_at(&block, COUNTER_FREQUENCY), 100_000_000);
        assert_eq!(dword_at(&block, FIRMWARE_ME), 7);
        assert_eq!(dword_at(&block, FIRMWARE_MEC), 9);
        assert_eq!(dword_at(&block, RB_PER_ENGINE), 2);
        assert_eq!(
            dword_at(&block, GRAPHICS_FLAGS),
            GRAPHICS_COUNTS_VALID | UNIT_MASKS_VALID | TILE_STEERING_VALID
        );
        assert_eq!(dword_at(&block, EMULATED_REVISION), 0x9);
    }

    #[test]
    fn a_render_backend_that_is_off_shows_in_the_disable_mask() {
        let block = build(&small_card());
        assert_eq!(u16::from_le_bytes([block[DISABLED_RB_MASK], block[DISABLED_RB_MASK + 1]]), 0b10);
    }

    #[test]
    fn the_unit_masks_sit_two_to_an_engine_and_arrays_are_counted_per_engine() {
        let mut info = small_card();
        let mut bitmap = [[0; 4]; 4];
        bitmap[0][0] = 0b0011;
        bitmap[0][1] = 0b1111;
        bitmap[1][0] = 0b1100;
        bitmap[1][1] = 0b0000;
        info.details.cu_bitmap = bitmap;
        info.details.shader_engines = 2;
        info.details.arrays_per_engine = 2;
        let block = build(&info);
        // The guest copies four bytes an engine: engine 0's two arrays, then engine 1's.
        let words: Vec<u16> = (0..4)
            .map(|index| {
                u16::from_le_bytes([block[ACTIVE_UNITS + index * 2], block[ACTIVE_UNITS + index * 2 + 1]])
            })
            .collect();
        assert_eq!(words, [0b01, 0b11, 0b10, 0b00]);
        assert_eq!(dword_at(&block, SHADER_ENGINES), 2);
        assert_eq!(dword_at(&block, ARRAYS_PER_ENGINE), 2);
        assert_ne!(dword_at(&block, GRAPHICS_FLAGS) & UNIT_MASKS_VALID, 0);
    }

    #[test]
    fn units_are_counted_as_workgroups_from_gfx10() {
        let mut bitmap = [[0; 4]; 4];
        bitmap[0][0] = 0b1111;
        bitmap[1][1] = 0b0111;
        let rdna = unit_masks(&bitmap, 10);
        assert_eq!(rdna[0], 0b11);
        // A pair with one unit off is not a whole workgroup.
        assert_eq!(rdna[3], 0b01);
        let gcn = unit_masks(&bitmap, 9);
        assert_eq!(gcn[0], 0b1111);
        assert_eq!(gcn[3], 0b0111);
    }

    #[test]
    fn only_the_block_query_is_answered() {
        let info = small_card();
        let mut out = Vec::new();
        let other =
            VendorQuery { kind: 7, size: BLOCK_SIZE, data: &[], process_is_32_bit: false, program: "" };
        assert_eq!(answer(&other, &info, &mut out), None);
        let short =
            VendorQuery { kind: QUERY_TYPE, size: 0x100, data: &[], process_is_32_bit: false, program: "" };
        assert_eq!(answer(&short, &info, &mut out), Some(Status::INVALID_PARAMETER));
        assert!(out.is_empty());
        let too_big = VendorQuery {
            kind: QUERY_TYPE,
            size: BLOCK_SIZE + 4,
            data: &[],
            process_is_32_bit: false,
            program: "",
        };
        assert_eq!(answer(&too_big, &info, &mut out), Some(Status::INVALID_PARAMETER));
        assert!(out.is_empty());
        let right = VendorQuery {
            kind: QUERY_TYPE,
            size: BLOCK_SIZE,
            data: &[],
            process_is_32_bit: false,
            program: "",
        };
        assert_eq!(answer(&right, &info, &mut out), Some(Status::SUCCESS));
        assert_eq!(out.len(), BLOCK_SIZE);
        out.clear();
        let header_size = adapter_header::HEADER_SIZE;
        let header = VendorQuery {
            kind: QUERY_TYPE,
            size: header_size,
            data: &[],
            process_is_32_bit: false,
            program: "",
        };
        assert_eq!(answer(&header, &info, &mut out), Some(Status::SUCCESS));
        assert_eq!(out, adapter_header::build(&info, header_size));
        out.clear();
        let both_size = header_size + BLOCK_SIZE;
        let both = VendorQuery {
            kind: QUERY_TYPE,
            size: both_size,
            data: &[],
            process_is_32_bit: false,
            program: "",
        };
        assert_eq!(answer(&both, &info, &mut out), Some(Status::SUCCESS));
        assert_eq!(out[..header_size], adapter_header::build(&info, both_size)[..]);
        assert_eq!(out[header_size..], build(&info)[..]);
    }

    #[test]
    fn the_record_and_the_directory_follow_the_header_by_the_size_asked() {
        let info = small_card();
        let header_size = adapter_header::HEADER_SIZE;
        for (tail, body) in
            [(ADAPTER_RECORD_SIZE, adapter_record(&info.details)), (DIRECTORY_SIZE, directory())]
        {
            let mut out = Vec::new();
            let size = header_size + tail;
            let query =
                VendorQuery { kind: QUERY_TYPE, size, data: &[], process_is_32_bit: false, program: "" };
            assert_eq!(answer(&query, &info, &mut out), Some(Status::SUCCESS));
            assert_eq!(out[header_size..], body[..]);
        }
    }

    #[test]
    fn a_card_without_a_readable_address_config_still_gets_a_block() {
        let mut info = small_card();
        info.details.gb_addr_config = None;
        info.details.shader_engines = 0;
        let key = KeyFields::read(&build(&info)).unwrap();
        assert_eq!(key.gb_addr_config, 0);
        assert!(key.checksum_holds);
    }
}
