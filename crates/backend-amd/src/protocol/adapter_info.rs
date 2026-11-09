//! The adapter block AMD's Windows driver reads at start, built from the host card.
//!
//! The guest asks for it with an adapter query of type 0, the WDK's DXGKQAITYPE_UMDRIVERPRIVATE,
//! into a buffer of exactly [`BLOCK_SIZE`] bytes. AMD's open interop header calls the structure
//! ADAPTERINFOEX. All of it is the kernel driver's to fill in, and the guest only reads it, so each
//! field here is one of three things:
//!
//! - read off the host card through amdgpu's INFO queries, which the field's comment names;
//! - a fixed value this backend chooses, for what a guest in a virtual machine should be told;
//! - left zero, because nothing on the host says what it should be.
//!
//! Every field not named below is zero. The last dword folded into [`CHECKSUM`] covers the whole
//! block, so the zeros are part of the answer too.

use super::allocation::SURFACE_SIZE;
use super::bytes::{index_xor_sum, put_dword, put_qword, put_word};
use crate::info::{CardDetails, DeviceInfo};
use guesthouse_backend::{Status, VendorQuery};

/// The adapter query type that asks for the block, DXGKQAITYPE_UMDRIVERPRIVATE.
pub const QUERY_TYPE: u32 = 0;
/// The guest's buffer must be exactly this size, or the kernel refuses the query.
pub const BLOCK_SIZE: usize = 0x3A00;

// The block's start. Fixed values.
/// The surface descriptor size the kernel expects in allocation private data.
const SURFACE_DESCRIPTOR_SIZE: usize = 0x00C;
const SYSTEM_FLAGS: usize = 0x208;
/// The guest runs in a virtual machine. The secure-VM, IOMMU, Thunderbolt and USB4 bits stay clear.
const SYSTEM_IN_VIRTUAL_MACHINE: u32 = 1 << 1;
/// How many GPUs the adapter links. One, with linking off.
const GPU_COUNT: usize = 0x608;

// Firmware versions, from FW_VERSION. Video firmware stays zero, as no video engine is offered.
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
const FAMILY: usize = 0xA04;
/// INFO's external_rev, which is the closest the host has.
const EMULATED_REVISION: usize = 0xA08;
/// From sysfs, as INFO has no subsystem.
const SUBSYSTEM_ID: usize = 0xA0C;
const LOCAL_VISIBLE_HEAP: usize = 0xA14;
const LOCAL_INVISIBLE_HEAP: usize = 0xA1C;
/// In hertz. INFO gives kilohertz.
const COUNTER_FREQUENCY: usize = 0xA24;
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

// The graphics part of the first GPU's block, the GFX9-and-later member of its family union.
const GB_ADDR_CONFIG: usize = 0xC00;
/// The high half of the render backend disable word: one bit per render backend that is off.
const DISABLED_RB_MASK: usize = 0xC06;
const CE_RAM_SIZE: usize = 0xC08;
/// Eight 16-bit masks of active units, two arrays per shader engine for four engines. A unit is a
/// compute unit before GFX10 and a workgroup processor from GFX10 on.
const ACTIVE_UNITS: usize = 0xC20;
/// The units kept on all the time, laid out the same way.
const ALWAYS_ON_UNITS: usize = 0xC30;
const GRAPHICS_FLAGS: usize = 0xC44;
/// The counts from [`MAX_VGPRS`] on are filled in.
const GRAPHICS_COUNTS_VALID: u32 = 1 << 0;
/// [`TILE_STEERING_OVERRIDE`] is filled in.
const TILE_STEERING_VALID: u32 = 1 << 4;
const MAX_VGPRS: usize = 0xC58;
const WAVEFRONT_SIZE: usize = 0xC5C;
const SHADER_ENGINES: usize = 0xC60;
const RB_PER_ENGINE: usize = 0xC64;
const ARRAYS_PER_ENGINE: usize = 0xC68;
const CUS_PER_ARRAY: usize = 0xC6C;
const TCC_BLOCKS: usize = 0xC74;
const MAX_GS_WAVES: usize = 0xC78;
const GS_VGT_TABLE_DEPTH: usize = 0xC80;
const GS_PRIM_BUFFER_DEPTH: usize = 0xC84;
const COMPUTE_RINGS: usize = 0xC88;
const TILE_STEERING_OVERRIDE: usize = 0xCE0;

/// The software features the kernel offers the guest. Fixed.
const SOFTWARE_FEATURES: usize = 0x2E00;
/// Allocation private data may carry the second information block after its surface descriptors.
const FEATURE_ALLOCATION_SECOND_BLOCK: u32 = 1 << 3;

/// The sum the guest checks the block against, kept in the block. See [`index_xor_sum`].
const CHECKSUM: usize = 0x2E20;

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
    if query.size != BLOCK_SIZE {
        return Some(Status::INVALID_PARAMETER);
    }
    out.extend_from_slice(&build(info));
    Some(Status::SUCCESS)
}

/// The whole block for this card.
pub fn build(info: &DeviceInfo) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    put_dword(&mut block, SURFACE_DESCRIPTOR_SIZE, SURFACE_SIZE as u32);
    put_dword(&mut block, SYSTEM_FLAGS, SYSTEM_IN_VIRTUAL_MACHINE);
    put_dword(&mut block, GPU_COUNT, 1);
    put_dword(&mut block, MIN_PITCH_ALIGN, MIN_PITCH_ALIGN_PIXELS);
    fill_firmware(&mut block, info);
    fill_first_gpu(&mut block, info);
    fill_graphics(&mut block, &info.details);
    put_dword(&mut block, SOFTWARE_FEATURES, FEATURE_ALLOCATION_SECOND_BLOCK);
    let sum = index_xor_sum(&block, BLOCK_SIZE / 4, CHECKSUM);
    put_dword(&mut block, CHECKSUM, sum);
    block
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
    put_dword(block, COUNTER_FREQUENCY, info.counter_freq_khz.saturating_mul(1000));
    put_word(block, ENGINE_CLOCK_MHZ, megahertz(details.max_engine_clock_khz));
    put_word(block, MEMORY_CLOCK_MHZ, megahertz(details.max_memory_clock_khz));
    put_dword(block, REGISTER_PROTECTION, REGISTERS_PROTECTED);
    put_qword(block, LOCAL_VISIBLE_SEGMENT, info.visible_vram_total);
    put_qword(block, LOCAL_INVISIBLE_SEGMENT, invisible);
    put_dword(block, REVISION_ID, u32::from(info.pci_revision));
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

    let mut flags = GRAPHICS_COUNTS_VALID;
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
mod tests {
    use super::*;
    use crate::info::{EngineSet, Firmware};

    /// A small RDNA2 part like the host's: one engine, one array of two units.
    fn small_card() -> DeviceInfo {
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
            engines: EngineSet { graphics: true, compute: true, dma: true },
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
        assert_eq!(dword_at(&block, SOFTWARE_FEATURES), FEATURE_ALLOCATION_SECOND_BLOCK);
        assert_eq!(dword_at(&block, SURFACE_DESCRIPTOR_SIZE), SURFACE_SIZE as u32);
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
        assert_eq!(dword_at(&block, GRAPHICS_FLAGS), GRAPHICS_COUNTS_VALID | TILE_STEERING_VALID);
    }

    #[test]
    fn a_render_backend_that_is_off_shows_in_the_disable_mask() {
        let block = build(&small_card());
        assert_eq!(u16::from_le_bytes([block[DISABLED_RB_MASK], block[DISABLED_RB_MASK + 1]]), 0b10);
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
        let other = VendorQuery { kind: 7, size: BLOCK_SIZE, data: &[], process_is_32_bit: false };
        assert_eq!(answer(&other, &info, &mut out), None);
        let short = VendorQuery { kind: QUERY_TYPE, size: 0x100, data: &[], process_is_32_bit: false };
        assert_eq!(answer(&short, &info, &mut out), Some(Status::INVALID_PARAMETER));
        assert!(out.is_empty());
        let right = VendorQuery { kind: QUERY_TYPE, size: BLOCK_SIZE, data: &[], process_is_32_bit: false };
        assert_eq!(answer(&right, &info, &mut out), Some(Status::SUCCESS));
        assert_eq!(out.len(), BLOCK_SIZE);
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
