//! What the card and its driver report, read through `DRM_AMDGPU_INFO`.
//!
//! An INFO call hands the kernel a pointer to a buffer and the number of bytes it may fill, plus a
//! query id. The buffers are the structures in the uAPI header; the fields this backend needs are
//! read out by offset. Nothing here touches the guest, and none of it comes from a table of cards.

use crate::ioctl::{self, call, get32, get64, put32, put64};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

// Query ids, from the header.
const DEV_INFO: u32 = 0x16;
const MEMORY: u32 = 0x19;
const HW_IP_INFO: u32 = 0x02;
const TIMESTAMP: u32 = 0x05;
const FW_VERSION: u32 = 0x0e;
const READ_MMR_REG: u32 = 0x15;

// Firmware kinds for FW_VERSION, from AMDGPU_INFO_FW_*.
const FW_GMC: u32 = 0x03;
const FW_GFX_ME: u32 = 0x04;
const FW_GFX_PFP: u32 = 0x05;
const FW_GFX_CE: u32 = 0x06;
const FW_GFX_RLC: u32 = 0x07;
const FW_GFX_MEC: u32 = 0x08;
const FW_SMC: u32 = 0x0a;
const FW_SDMA: u32 = 0x0b;
const FW_SOS: u32 = 0x0c;
const FW_MES: u32 = 0x1a;

/// GB_ADDR_CONFIG, at the dword offset the kernel accepts for it on GFX9 and later, the same one
/// libdrm reads. It says how the card interleaves memory across pipes, which a tiled frame's
/// modifier has to carry.
const GB_ADDR_CONFIG_REGISTER: u32 = 0x263e;
/// Asks for the register as the whole card sees it, not one shader engine's copy.
const ALL_INSTANCES: u32 = 0xffff_ffff;

// Hardware IP types this backend cares about, from AMDGPU_HW_IP_*.
pub const HW_IP_GFX: u32 = 0;
pub const HW_IP_COMPUTE: u32 = 1;
pub const HW_IP_DMA: u32 = 2;

/// An APU carves its video memory out of system RAM, so its sizes and CPU reach differ from a card
/// with its own VRAM. AMDGPU_IDS_FLAGS_FUSION says which this is.
const IDS_FLAGS_FUSION: u64 = 0x01;

// Offsets into drm_amdgpu_info, the 32-byte request.
const INFO_RETURN_POINTER: usize = 0;
const INFO_RETURN_SIZE: usize = 8;
const INFO_QUERY: usize = 12;
const INFO_HW_IP_TYPE: usize = 16;
const INFO_HW_IP_INSTANCE: usize = 20;
const INFO_REQUEST_SIZE: usize = 32;
// The same request's union, for a firmware query and a register read.
const INFO_FW_TYPE: usize = 16;
const INFO_FW_INDEX: usize = 24;
const INFO_REGISTER_OFFSET: usize = 16;
const INFO_REGISTER_COUNT: usize = 20;
const INFO_REGISTER_INSTANCE: usize = 24;

// Offsets into drm_amdgpu_info_device, the 448-byte reply.
const DEV_INFO_SIZE: usize = 448;
const DEV_DEVICE_ID: usize = 0;
const DEV_PCI_REV: usize = 12;
const DEV_FAMILY: usize = 16;
const DEV_GPU_COUNTER_FREQ: usize = 28;
const DEV_CU_ACTIVE: usize = 48;
const DEV_IDS_FLAGS: usize = 136;
const DEV_VA_START: usize = 144;
const DEV_VA_MAX: usize = 152;
const DEV_VA_ALIGN: usize = 160;
const DEV_VRAM_TYPE: usize = 176;
const DEV_VRAM_BIT_WIDTH: usize = 180;
const DEV_EXTERNAL_REV: usize = 8;
const DEV_SHADER_ENGINES: usize = 20;
const DEV_ARRAYS_PER_ENGINE: usize = 24;
const DEV_MAX_ENGINE_CLOCK: usize = 32;
const DEV_MAX_MEMORY_CLOCK: usize = 40;
const DEV_CU_BITMAP: usize = 56;
const DEV_ENABLED_RB_MASK: usize = 120;
const DEV_RB_PIPES: usize = 124;
const DEV_CE_RAM_SIZE: usize = 172;
const DEV_WAVEFRONT_SIZE: usize = 240;
const DEV_VISIBLE_VGPRS: usize = 244;
const DEV_CU_PER_ARRAY: usize = 248;
const DEV_TCC_BLOCKS: usize = 252;
const DEV_GS_VGT_TABLE_DEPTH: usize = 256;
const DEV_GS_PRIM_BUFFER_DEPTH: usize = 260;
const DEV_MAX_GS_WAVES: usize = 264;
const DEV_CU_ALWAYS_ON_BITMAP: usize = 272;
const DEV_TILE_STEERING_OVERRIDE: usize = 352;
const DEV_ENABLED_RB_MASK_HIGH: usize = 416;
/// cu_bitmap and cu_ao_bitmap are four shader engines by four arrays of 32-bit masks.
const BITMAP_SIDE: usize = 4;

// Offsets into drm_amdgpu_memory_info, the 96-byte reply. Each heap is a 32-byte block whose first
// field is its total size.
const MEMORY_INFO_SIZE: usize = 96;
const MEM_VRAM_TOTAL: usize = 0;
const MEM_VISIBLE_VRAM_TOTAL: usize = 32;
const MEM_GTT_TOTAL: usize = 64;

// Offsets into drm_amdgpu_info_hw_ip, the 40-byte reply.
const HW_IP_SIZE: usize = 40;
const HW_IP_VERSION_MAJOR: usize = 0;
const HW_IP_VERSION_MINOR: usize = 4;
const HW_IP_AVAILABLE_RINGS: usize = 24;
/// The block's version from IP discovery: major in bits 23:16, minor in 15:8. The major and minor
/// above are the kernel's own driver for the block, which reads 10.0 on a GFX10.3 part, so this one
/// is preferred when the kernel fills it in.
const HW_IP_DISCOVERY_VERSION: usize = 28;

// drm_amdgpu_info_firmware, the 8-byte reply.
const FIRMWARE_REPLY_SIZE: usize = 8;
const FIRMWARE_VERSION: usize = 0;

const SUBSYSTEM_VENDOR_FILE: &str = "subsystem_vendor";
const SUBSYSTEM_DEVICE_FILE: &str = "subsystem_device";

/// Everything this backend reads off the card at open time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_id: u16,
    pub pci_revision: u8,
    pub family: u32,
    pub active_compute_units: u32,
    pub is_apu: bool,
    pub vram_type: u32,
    pub vram_bit_width: u32,
    /// The card's total and CPU-reachable video memory, and its GART-mapped system memory.
    pub vram_total: u64,
    pub visible_vram_total: u64,
    pub gtt_total: u64,
    /// The usable GPU virtual address range, which reserves the bottom, a hole and the top.
    pub va_start: u64,
    pub va_end: u64,
    pub va_alignment: u64,
    /// The GPU timer's tick rate in kHz, for turning a timestamp into nanoseconds.
    pub counter_freq_khz: u32,
    /// Which of the three engine families have a ring the guest could be given.
    pub engines: EngineSet,
    /// The rest of what the card reports, which the guest's adapter block is built from.
    pub details: CardDetails,
}

/// What the card reports beyond what the backend itself needs, for the adapter block AMD's Windows
/// driver reads at start. Each field is the INFO field of the same meaning, unconverted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardDetails {
    pub external_revision: u32,
    pub shader_engines: u32,
    pub arrays_per_engine: u32,
    pub max_engine_clock_khz: u64,
    pub max_memory_clock_khz: u64,
    /// Active compute units, by shader engine and then array, one bit per unit.
    pub cu_bitmap: [[u32; BITMAP_SIDE]; BITMAP_SIDE],
    /// The units kept powered all the time, laid out the same way.
    pub always_on_cu_bitmap: [[u32; BITMAP_SIDE]; BITMAP_SIDE],
    pub enabled_rb_mask: u64,
    /// Render backends the card has room for, enabled or not.
    pub rb_pipes: u32,
    pub ce_ram_size: u32,
    pub wavefront_size: u32,
    pub visible_vgprs: u32,
    pub cus_per_array: u32,
    pub tcc_blocks: u32,
    pub gs_vgt_table_depth: u32,
    pub gs_prim_buffer_depth: u32,
    pub max_gs_waves_per_vgt: u32,
    pub tile_steering_override: u32,
    /// The graphics block's version, major and minor.
    pub gfx_version: (u32, u32),
    pub compute_rings: u32,
    /// GB_ADDR_CONFIG, if the kernel let it be read.
    pub gb_addr_config: Option<u32>,
    pub firmware: Firmware,
    /// The PCI subsystem, vendor in the low half and device in the high half, as config space
    /// holds them. Read from sysfs, as INFO has no field for it.
    pub subsystem: u32,
}

/// Firmware versions, from FW_VERSION. Zero for firmware the card doesn't have.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Firmware {
    pub me: u32,
    pub pfp: u32,
    pub ce: u32,
    pub rlc: u32,
    pub mec: u32,
    pub sdma0: u32,
    pub sdma1: u32,
    pub smc: u32,
    pub sos: u32,
    pub memory_controller: u32,
    pub mes: u32,
}

/// Which engine rings the card offers. A submission must name a family that has a ring, or the
/// kernel refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EngineSet {
    pub graphics: bool,
    pub compute: bool,
    pub dma: bool,
}

impl EngineSet {
    pub fn count(&self) -> u32 {
        u32::from(self.graphics) + u32::from(self.compute) + u32::from(self.dma)
    }
}

fn query(file: &impl AsRawFd, query: u32, reply: &mut [u8]) -> io::Result<()> {
    let mut request = [0u8; INFO_REQUEST_SIZE];
    put64(&mut request, INFO_RETURN_POINTER, reply.as_mut_ptr() as u64);
    put32(&mut request, INFO_RETURN_SIZE, reply.len() as u32);
    put32(&mut request, INFO_QUERY, query);
    ioctl::drm_ioctl(file, call::info(), &mut request)
}

/// One engine family's reply: its version and a mask of the rings it offers.
#[derive(Debug, Clone, Copy, Default)]
struct HwIp {
    version: (u32, u32),
    rings: u32,
}

fn query_hw_ip(file: &impl AsRawFd, ip_type: u32) -> io::Result<HwIp> {
    let mut request = [0u8; INFO_REQUEST_SIZE];
    let mut reply = [0u8; HW_IP_SIZE];
    put64(&mut request, INFO_RETURN_POINTER, reply.as_mut_ptr() as u64);
    put32(&mut request, INFO_RETURN_SIZE, reply.len() as u32);
    put32(&mut request, INFO_QUERY, HW_IP_INFO);
    put32(&mut request, INFO_HW_IP_TYPE, ip_type);
    put32(&mut request, INFO_HW_IP_INSTANCE, 0);
    ioctl::drm_ioctl(file, call::info(), &mut request)?;
    let discovered = get32(&reply, HW_IP_DISCOVERY_VERSION);
    let version = if discovered != 0 {
        (discovered >> 16 & 0xff, discovered >> 8 & 0xff)
    } else {
        (get32(&reply, HW_IP_VERSION_MAJOR), get32(&reply, HW_IP_VERSION_MINOR))
    };
    Ok(HwIp { version, rings: get32(&reply, HW_IP_AVAILABLE_RINGS) })
}

/// A firmware's version, or zero if the card has none of that kind.
fn query_firmware(file: &impl AsRawFd, kind: u32, index: u32) -> u32 {
    let mut request = [0u8; INFO_REQUEST_SIZE];
    let mut reply = [0u8; FIRMWARE_REPLY_SIZE];
    put64(&mut request, INFO_RETURN_POINTER, reply.as_mut_ptr() as u64);
    put32(&mut request, INFO_RETURN_SIZE, reply.len() as u32);
    put32(&mut request, INFO_QUERY, FW_VERSION);
    put32(&mut request, INFO_FW_TYPE, kind);
    put32(&mut request, INFO_FW_INDEX, index);
    match ioctl::drm_ioctl(file, call::info(), &mut request) {
        Ok(()) => get32(&reply, FIRMWARE_VERSION),
        Err(_) => 0,
    }
}

/// One register from the kernel's list of those anyone may read.
fn read_register(file: &impl AsRawFd, register: u32) -> Option<u32> {
    let mut request = [0u8; INFO_REQUEST_SIZE];
    let mut reply = [0u8; 4];
    put64(&mut request, INFO_RETURN_POINTER, reply.as_mut_ptr() as u64);
    put32(&mut request, INFO_RETURN_SIZE, reply.len() as u32);
    put32(&mut request, INFO_QUERY, READ_MMR_REG);
    put32(&mut request, INFO_REGISTER_OFFSET, register);
    put32(&mut request, INFO_REGISTER_COUNT, 1);
    put32(&mut request, INFO_REGISTER_INSTANCE, ALL_INSTANCES);
    ioctl::drm_ioctl(file, call::info(), &mut request).ok()?;
    Some(u32::from_le_bytes(reply))
}

fn read_firmware(file: &impl AsRawFd) -> Firmware {
    Firmware {
        me: query_firmware(file, FW_GFX_ME, 0),
        pfp: query_firmware(file, FW_GFX_PFP, 0),
        ce: query_firmware(file, FW_GFX_CE, 0),
        rlc: query_firmware(file, FW_GFX_RLC, 0),
        mec: query_firmware(file, FW_GFX_MEC, 0),
        sdma0: query_firmware(file, FW_SDMA, 0),
        sdma1: query_firmware(file, FW_SDMA, 1),
        smc: query_firmware(file, FW_SMC, 0),
        sos: query_firmware(file, FW_SOS, 0),
        memory_controller: query_firmware(file, FW_GMC, 0),
        mes: query_firmware(file, FW_MES, 0),
    }
}

fn bitmap(dev: &[u8], at: usize) -> [[u32; BITMAP_SIDE]; BITMAP_SIDE] {
    let mut out = [[0; BITMAP_SIDE]; BITMAP_SIDE];
    for (engine, row) in out.iter_mut().enumerate() {
        for (array, mask) in row.iter_mut().enumerate() {
            *mask = get32(dev, at + (engine * BITMAP_SIDE + array) * 4);
        }
    }
    out
}

/// The PCI subsystem from sysfs, packed as config space packs it. Zero if it can't be read.
fn read_subsystem(device_folder: Option<&Path>) -> u32 {
    let Some(folder) = device_folder else { return 0 };
    let read = |name: &str| {
        let text = fs::read_to_string(folder.join(name)).ok()?;
        u16::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
    };
    match (read(SUBSYSTEM_VENDOR_FILE), read(SUBSYSTEM_DEVICE_FILE)) {
        (Some(vendor), Some(device)) => u32::from(device) << 16 | u32::from(vendor),
        _ => 0,
    }
}

fn read_details(
    file: &impl AsRawFd,
    dev: &[u8],
    gfx: HwIp,
    compute: HwIp,
    device_folder: Option<&Path>,
) -> CardDetails {
    CardDetails {
        external_revision: get32(dev, DEV_EXTERNAL_REV),
        shader_engines: get32(dev, DEV_SHADER_ENGINES),
        arrays_per_engine: get32(dev, DEV_ARRAYS_PER_ENGINE),
        max_engine_clock_khz: get64(dev, DEV_MAX_ENGINE_CLOCK),
        max_memory_clock_khz: get64(dev, DEV_MAX_MEMORY_CLOCK),
        cu_bitmap: bitmap(dev, DEV_CU_BITMAP),
        always_on_cu_bitmap: bitmap(dev, DEV_CU_ALWAYS_ON_BITMAP),
        enabled_rb_mask: u64::from(get32(dev, DEV_ENABLED_RB_MASK_HIGH)) << 32
            | u64::from(get32(dev, DEV_ENABLED_RB_MASK)),
        rb_pipes: get32(dev, DEV_RB_PIPES),
        ce_ram_size: get32(dev, DEV_CE_RAM_SIZE),
        wavefront_size: get32(dev, DEV_WAVEFRONT_SIZE),
        visible_vgprs: get32(dev, DEV_VISIBLE_VGPRS),
        cus_per_array: get32(dev, DEV_CU_PER_ARRAY),
        tcc_blocks: get32(dev, DEV_TCC_BLOCKS),
        gs_vgt_table_depth: get32(dev, DEV_GS_VGT_TABLE_DEPTH),
        gs_prim_buffer_depth: get32(dev, DEV_GS_PRIM_BUFFER_DEPTH),
        max_gs_waves_per_vgt: get32(dev, DEV_MAX_GS_WAVES),
        tile_steering_override: get32(dev, DEV_TILE_STEERING_OVERRIDE),
        gfx_version: gfx.version,
        compute_rings: compute.rings.count_ones(),
        gb_addr_config: read_register(file, GB_ADDR_CONFIG_REGISTER),
        firmware: read_firmware(file),
        subsystem: read_subsystem(device_folder),
    }
}

/// Reads everything the backend needs about the card. One INFO call per structure. The PCI device's
/// sysfs folder, if given, supplies the little INFO doesn't.
pub fn read_device_info(file: &impl AsRawFd, device_folder: Option<&Path>) -> io::Result<DeviceInfo> {
    let mut dev = vec![0u8; DEV_INFO_SIZE];
    query(file, DEV_INFO, &mut dev)?;
    let mut mem = vec![0u8; MEMORY_INFO_SIZE];
    query(file, MEMORY, &mut mem)?;

    let gfx = query_hw_ip(file, HW_IP_GFX).unwrap_or_default();
    let compute = query_hw_ip(file, HW_IP_COMPUTE).unwrap_or_default();
    let dma = query_hw_ip(file, HW_IP_DMA).unwrap_or_default();
    let engines = EngineSet { graphics: gfx.rings != 0, compute: compute.rings != 0, dma: dma.rings != 0 };

    Ok(DeviceInfo {
        device_id: get32(&dev, DEV_DEVICE_ID) as u16,
        pci_revision: get32(&dev, DEV_PCI_REV) as u8,
        family: get32(&dev, DEV_FAMILY),
        active_compute_units: get32(&dev, DEV_CU_ACTIVE),
        is_apu: get64(&dev, DEV_IDS_FLAGS) & IDS_FLAGS_FUSION != 0,
        vram_type: get32(&dev, DEV_VRAM_TYPE),
        vram_bit_width: get32(&dev, DEV_VRAM_BIT_WIDTH),
        vram_total: get64(&mem, MEM_VRAM_TOTAL),
        visible_vram_total: get64(&mem, MEM_VISIBLE_VRAM_TOTAL),
        gtt_total: get64(&mem, MEM_GTT_TOTAL),
        va_start: get64(&dev, DEV_VA_START),
        va_end: get64(&dev, DEV_VA_MAX),
        va_alignment: u64::from(get32(&dev, DEV_VA_ALIGN)),
        counter_freq_khz: get32(&dev, DEV_GPU_COUNTER_FREQ),
        engines,
        details: read_details(file, &dev, gfx, compute, device_folder),
    })
}

/// The GPU's clock in nanoseconds, from the timestamp counter and the tick rate read at open time.
pub fn read_gpu_clock(file: &impl AsRawFd, counter_freq_khz: u32) -> io::Result<u64> {
    let mut reply = [0u8; 8];
    query(file, TIMESTAMP, &mut reply)?;
    let ticks = u64::from_le_bytes(reply);
    let hz = u64::from(counter_freq_khz).max(1) * 1000;
    Ok((ticks as u128 * 1_000_000_000 / u128::from(hz)) as u64)
}
