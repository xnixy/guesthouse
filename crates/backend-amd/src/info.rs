//! What the card and its driver report, read through `DRM_AMDGPU_INFO`.
//!
//! An INFO call hands the kernel a pointer to a buffer and the number of bytes it may fill, plus a
//! query id. The buffers are the structures in the uAPI header; the fields this backend needs are
//! read out by offset. Nothing here touches the guest, and none of it comes from a table of cards.

use crate::ioctl::{self, call, get32, get64, put32, put64};
use std::io;
use std::os::fd::AsRawFd;

// Query ids, from the header.
const DEV_INFO: u32 = 0x16;
const MEMORY: u32 = 0x19;
const HW_IP_INFO: u32 = 0x02;
const TIMESTAMP: u32 = 0x05;

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

// Offsets into drm_amdgpu_memory_info, the 96-byte reply. Each heap is a 32-byte block whose first
// field is its total size.
const MEMORY_INFO_SIZE: usize = 96;
const MEM_VRAM_TOTAL: usize = 0;
const MEM_VISIBLE_VRAM_TOTAL: usize = 32;
const MEM_GTT_TOTAL: usize = 64;

// Offsets into drm_amdgpu_info_hw_ip, the 40-byte reply.
const HW_IP_SIZE: usize = 40;
const HW_IP_VERSION_MAJOR: usize = 0;
const HW_IP_AVAILABLE_RINGS: usize = 24;

/// Everything this backend reads off the card at open time.
#[derive(Debug, Clone, PartialEq, Eq)]
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

fn query_hw_ip(file: &impl AsRawFd, ip_type: u32) -> io::Result<bool> {
    let mut request = [0u8; INFO_REQUEST_SIZE];
    let mut reply = [0u8; HW_IP_SIZE];
    put64(&mut request, INFO_RETURN_POINTER, reply.as_mut_ptr() as u64);
    put32(&mut request, INFO_RETURN_SIZE, reply.len() as u32);
    put32(&mut request, INFO_QUERY, HW_IP_INFO);
    put32(&mut request, INFO_HW_IP_TYPE, ip_type);
    put32(&mut request, INFO_HW_IP_INSTANCE, 0);
    ioctl::drm_ioctl(file, call::info(), &mut request)?;
    let _version = get32(&reply, HW_IP_VERSION_MAJOR);
    Ok(get32(&reply, HW_IP_AVAILABLE_RINGS) != 0)
}

/// Reads everything the backend needs about the card. One INFO call per structure.
pub fn read_device_info(file: &impl AsRawFd) -> io::Result<DeviceInfo> {
    let mut dev = vec![0u8; DEV_INFO_SIZE];
    query(file, DEV_INFO, &mut dev)?;
    let mut mem = vec![0u8; MEMORY_INFO_SIZE];
    query(file, MEMORY, &mut mem)?;

    let engines = EngineSet {
        graphics: query_hw_ip(file, HW_IP_GFX).unwrap_or(false),
        compute: query_hw_ip(file, HW_IP_COMPUTE).unwrap_or(false),
        dma: query_hw_ip(file, HW_IP_DMA).unwrap_or(false),
    };

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
