//! The end-to-end proof as a test, run only where an AMD render node is present.
//!
//! It opens the card, allocates and maps a buffer, writes a tiny SDMA command buffer that stores a
//! known word to it, submits, waits on an eventfd, and checks the word came back. Without an AMD
//! card it returns early, so the test is green on any machine and real only on one with the card.

use guesthouse_backend::Backend;
use guesthouse_backend::{GpuRange, MapRequest, MemoryKind};
use guesthouse_backend_amd::{
    AmdBackend, Context, CpuAccess, DOMAIN_GTT, Device, HW_IP_DMA, MapFlags, MemoryType, Syncobj,
};

const MAGIC: u32 = 0x5EED_1234;
const PAGE: u64 = 4096;

fn card() -> Option<Device> {
    Device::open(None).ok()
}

#[test]
fn a_submission_writes_and_completes_on_an_eventfd() {
    let Some(device) = card() else {
        eprintln!("no AMD render node; skipping the hardware proof");
        return;
    };
    let info = device.info().clone();
    assert!(info.engines.dma, "the skeleton's write packet needs an SDMA ring");

    let align = info.va_alignment.max(PAGE);
    let ib =
        guesthouse_backend_amd::create_buffer(&device, PAGE, align, DOMAIN_GTT, CpuAccess::Required).unwrap();
    let result =
        guesthouse_backend_amd::create_buffer(&device, PAGE, align, DOMAIN_GTT, CpuAccess::Required).unwrap();

    let result_va = info.va_start + 0x100_000;
    let ib_va = info.va_start + 0x200_000;
    let rwx =
        MapFlags { readable: true, writable: true, executable: true, memory_type: MemoryType::Uncached };
    guesthouse_backend_amd::map_buffer(&device, ib, ib_va, 0, PAGE, rwx).unwrap();
    guesthouse_backend_amd::map_buffer(&device, result, result_va, 0, PAGE, rwx).unwrap();

    let mut ib_cpu = device.map_cpu(ib, PAGE as usize).unwrap();
    let mut result_cpu = device.map_cpu(result, PAGE as usize).unwrap();
    let packet = guesthouse_backend_amd::sdma_write_ib(result_va, &[MAGIC]);
    ib_cpu.write_dwords(&packet);
    result_cpu.write_u32(0, 0);

    let context = Context::create(&device).unwrap();
    let syncobj = Syncobj::create(&device).unwrap();
    guesthouse_backend_amd::submit(&device, &context, HW_IP_DMA, ib_va, (packet.len() * 4) as u32, &syncobj)
        .unwrap();
    let event = syncobj.arm_eventfd(&device).unwrap();
    assert!(guesthouse_backend_amd::wait_eventfd(&event, 2000).unwrap(), "did not complete");
    assert_eq!(result_cpu.read_u32(0), MAGIC, "the GPU did not write the expected word");

    syncobj.destroy(&device);
    guesthouse_backend_amd::unmap_buffer(&device, ib, ib_va, PAGE).unwrap();
    guesthouse_backend_amd::unmap_buffer(&device, result, result_va, PAGE).unwrap();
    guesthouse_backend_amd::close_buffer(&device, ib);
    guesthouse_backend_amd::close_buffer(&device, result);
    context.destroy(&device);
}

#[test]
fn the_backend_trait_opens_allocates_maps_and_exports() {
    if card().is_none() {
        eprintln!("no AMD render node; skipping the trait proof");
        return;
    }
    let mut backend = AmdBackend::open_card(None).unwrap();
    assert_eq!(backend.facts().card.vendor, guesthouse_backend_amd::AMD_VENDOR);
    assert!(backend.facts().node_count >= 1);

    let va = backend.device_info().va_start + 0x400_000;
    let space = backend.create_address_space(&[GpuRange { start: va, end: va + 0x1_000_000 }]).unwrap();
    let (memory, size) = backend.allocate_video_memory(0x10_000, MemoryKind::PLAIN).unwrap();
    let request = MapRequest {
        space,
        memory,
        offset: 0,
        length: size,
        address: va,
        small_pages: false,
        gpu_cached: false,
        readable: true,
        writable: true,
        executable: false,
        memory_type: guesthouse_backend::MapMemoryType::Default,
    };
    backend.map(&request).unwrap();
    let _fd = backend.export_memory(memory, size).unwrap();
    assert!(backend.gpu_clock().unwrap() > 0);
    backend.release_memory(memory);
    backend.destroy_address_space(space);
}

#[test]
fn the_adapter_block_matches_what_amdgpu_reports() {
    use guesthouse_backend_amd::protocol::adapter_info::{self, KeyFields};
    let Some(device) = card() else {
        eprintln!("no AMD render node; skipping the adapter block check");
        return;
    };
    let info = device.info();
    let key = KeyFields::read(&adapter_info::build(info)).unwrap();
    assert!(key.checksum_holds);
    assert!(key.in_virtual_machine);
    assert_eq!(key.chip_id, u32::from(info.device_id));
    let folder = device.device_folder().unwrap();
    let sysfs = |name: &str| std::fs::read_to_string(folder.join(name)).ok()?.trim().parse::<u64>().ok();
    if let Some(vram) = sysfs("mem_info_vram_total") {
        assert_eq!(key.visible_heap + key.invisible_heap, vram);
    }
    if let Some(major) = sysfs("ip_discovery/die/0/11/0/major") {
        assert_eq!(u64::from(info.details.gfx_version.0), major);
    }
}

#[test]
fn a_tiled_surface_buffer_keeps_its_swizzle_mode() {
    use guesthouse_backend_amd::protocol::tiling;
    use guesthouse_backend_amd::{buffer_tiling, close_buffer, create_buffer, set_buffer_tiling};
    /// addrlib's 64 KiB render layout with XOR, the usual one for a frame on RDNA.
    const SW_64KB_R_X: u32 = 27;
    let Some(device) = card() else {
        eprintln!("no AMD render node; skipping the tiling check");
        return;
    };
    let gfx = device.info().details.gfx_version;
    let buffer = tiling::host_buffer(SW_64KB_R_X, gfx);
    let handle = create_buffer(&device, 0x10_0000, 0x1_0000, buffer.domain, buffer.cpu_access).unwrap();
    set_buffer_tiling(&device, handle, buffer.tiling_info).unwrap();
    assert_eq!(buffer_tiling(&device, handle).unwrap(), buffer.tiling_info);
    close_buffer(&device, handle);
    let config = device.info().details.gb_addr_config.unwrap_or(0);
    assert!(tiling::modifier(SW_64KB_R_X, gfx, config, Some(4)).is_some());
}
