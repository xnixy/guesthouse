//! A standalone proof of the AMD host-execution path, run directly against the iGPU.
//!
//! It opens the card, allocates and maps buffers, writes a tiny SDMA command buffer that stores a
//! known word to a mapped buffer, submits it, waits for completion on an eventfd, and reads the word
//! back. It then measures submit-to-completion latency over many iterations and times a PRIME export
//! of one buffer. Nothing here involves a guest or any closed code.
//!
//! Run it with no arguments to use the one AMD card, or pass a PCI address to pick one of several.

use guesthouse_backend::{GpuRange, MapRequest, MemoryKind};
use guesthouse_backend_amd::{
    AmdBackend, Context, CpuAccess, DOMAIN_GTT, Device, HW_IP_DMA, MapFlags, MemoryType, Syncobj,
};
use std::process::ExitCode;
use std::time::Instant;

const MAGIC: u32 = 0xCAFE_D00D;
const PAGE: u64 = 4096;
const ITERATIONS: usize = 200;
const WAIT_MS: i32 = 2000;

fn main() -> ExitCode {
    let address = std::env::args().nth(1);
    match run(address.as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("amd-probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(address: Option<&str>) -> Result<(), String> {
    let device = Device::open(address).map_err(|error| format!("opening the AMD card: {error}"))?;
    let info = device.info().clone();
    println!(
        "card            device {:#06x} rev {} family {}",
        info.device_id, info.pci_revision, info.family
    );
    println!(
        "kind            {}",
        if info.is_apu { "APU (VRAM carved from system RAM)" } else { "discrete" }
    );
    println!("compute units   {}", info.active_compute_units);
    println!(
        "engines         gfx={} compute={} dma={}",
        info.engines.graphics, info.engines.compute, info.engines.dma
    );
    println!("vram total      {} MiB", info.vram_total >> 20);
    println!("vram visible    {} MiB", info.visible_vram_total >> 20);
    println!("gtt total       {} MiB", info.gtt_total >> 20);
    println!("va range        {:#x}..{:#x} align {:#x}", info.va_start, info.va_end, info.va_alignment);
    println!("counter freq    {} kHz", info.counter_freq_khz);

    if !info.engines.dma {
        return Err("this card reports no SDMA ring; the skeleton's write packet is SDMA-only".into());
    }

    // Two buffers in GART-mapped system memory: one for the command buffer, one for the result. Both
    // are CPU-reachable so the harness can fill the IB and read the result back.
    let align = info.va_alignment.max(PAGE);
    let ib_handle = memory_create(&device, PAGE, align, "IB")?;
    let result_handle = memory_create(&device, PAGE, align, "result")?;

    let result_va = info.va_start + 0x100_000;
    let ib_va = info.va_start + 0x200_000;
    let rwx =
        MapFlags { readable: true, writable: true, executable: true, memory_type: MemoryType::Uncached };
    guesthouse_backend_amd::map_buffer(&device, ib_handle, ib_va, 0, PAGE, rwx)
        .map_err(with("mapping the IB"))?;
    guesthouse_backend_amd::map_buffer(&device, result_handle, result_va, 0, PAGE, rwx)
        .map_err(with("mapping the result"))?;

    let mut ib_cpu = device.map_cpu(ib_handle, PAGE as usize).map_err(with("CPU-mapping the IB"))?;
    let mut result_cpu =
        device.map_cpu(result_handle, PAGE as usize).map_err(with("CPU-mapping the result"))?;

    let ib = guesthouse_backend_amd::sdma_write_ib(result_va, &[MAGIC]);
    ib_cpu.write_dwords(&ib);
    let ib_bytes = (ib.len() * 4) as u32;

    let context = Context::create(&device).map_err(with("creating a context"))?;

    // One proving run, checked for correctness.
    result_cpu.write_u32(0, 0);
    let syncobj = Syncobj::create(&device).map_err(with("creating a sync object"))?;
    guesthouse_backend_amd::submit(&device, &context, HW_IP_DMA, ib_va, ib_bytes, &syncobj)
        .map_err(with("submitting"))?;
    let event = syncobj.arm_eventfd(&device).map_err(with("arming the eventfd"))?;
    if !guesthouse_backend_amd::wait_eventfd(&event, WAIT_MS).map_err(with("waiting"))? {
        return Err("submission did not complete before the timeout".into());
    }
    let got = result_cpu.read_u32(0);
    syncobj.destroy(&device);
    println!(
        "\nsubmit proof    wrote {MAGIC:#010x}, read back {got:#010x} -> {}",
        if got == MAGIC { "PASS" } else { "FAIL" }
    );
    if got != MAGIC {
        return Err("the GPU did not write the expected word".into());
    }

    // Latency over many warm iterations.
    let mut samples = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        result_cpu.write_u32(0, 0);
        let syncobj = Syncobj::create(&device).map_err(with("creating a sync object"))?;
        let start = Instant::now();
        guesthouse_backend_amd::submit(&device, &context, HW_IP_DMA, ib_va, ib_bytes, &syncobj)
            .map_err(with("submitting"))?;
        let event = syncobj.arm_eventfd(&device).map_err(with("arming the eventfd"))?;
        let done = guesthouse_backend_amd::wait_eventfd(&event, WAIT_MS).map_err(with("waiting"))?;
        let elapsed = start.elapsed().as_secs_f64() * 1e3;
        syncobj.destroy(&device);
        if !done || result_cpu.read_u32(0) != MAGIC {
            return Err("an iteration did not complete correctly".into());
        }
        samples.push(elapsed);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    println!(
        "cs->completion  over {ITERATIONS} runs: min {:.3} ms, median {:.3} ms, max {:.3} ms",
        samples[0],
        median,
        samples[samples.len() - 1]
    );

    // PRIME export of the result buffer, timed.
    let start = Instant::now();
    let exported =
        guesthouse_backend_amd::export_buffer(&device, result_handle).map_err(with("PRIME export"))?;
    let export_ms = start.elapsed().as_secs_f64() * 1e3;
    println!("prime export    fd {} in {:.4} ms", std::os::fd::AsRawFd::as_raw_fd(&exported), export_ms);
    drop(exported);

    // Take the addresses back and let the buffers go.
    let _ = guesthouse_backend_amd::unmap_buffer(&device, ib_handle, ib_va, PAGE);
    let _ = guesthouse_backend_amd::unmap_buffer(&device, result_handle, result_va, PAGE);
    drop(ib_cpu);
    drop(result_cpu);
    guesthouse_backend_amd::close_buffer(&device, ib_handle);
    guesthouse_backend_amd::close_buffer(&device, result_handle);
    context.destroy(&device);

    // Show the Backend trait's own entry points working on the same card: open, facts, allocate in
    // VRAM, map, export, clock.
    println!("\n-- through the Backend trait --");
    let mut backend = AmdBackend::open_card(address).map_err(|error| error.to_string())?;
    trait_demo(&mut backend)?;

    Ok(())
}

fn trait_demo(backend: &mut AmdBackend) -> Result<(), String> {
    use guesthouse_backend::Backend;
    let facts = backend.facts().clone();
    println!(
        "facts           {} video_memory {} MiB nodes {}",
        facts.name,
        facts.video_memory >> 20,
        facts.node_count
    );
    let va = backend.device_info().va_start + 0x400_000;
    let range = GpuRange { start: va, end: va + 0x1_000_000 };
    let space = backend.create_address_space(&[range]).map_err(|error| error.to_string())?;
    let (memory, size) =
        backend.allocate_video_memory(0x10_000, MemoryKind::PLAIN).map_err(|error| error.to_string())?;
    let request = MapRequest {
        space,
        memory,
        offset: 0,
        length: size,
        address: va,
        small_pages: false,
        gpu_cached: false,
    };
    backend.map(&request).map_err(|error| error.to_string())?;
    let exported = backend.export_memory(memory, size).map_err(|error| error.to_string())?;
    let clock = backend.gpu_clock().map_err(|error| error.to_string())?;
    println!(
        "allocate+map    VRAM {} KiB at {va:#x}, exported fd {}",
        size >> 10,
        std::os::fd::AsRawFd::as_raw_fd(&exported)
    );
    println!("gpu clock       {clock} ns");
    drop(exported);
    backend.release_memory(memory);
    backend.destroy_address_space(space);
    Ok(())
}

fn memory_create(
    device: &Device,
    size: u64,
    align: u64,
    what: &str,
) -> Result<guesthouse_backend_amd::GemHandle, String> {
    guesthouse_backend_amd::create_buffer(device, size, align, DOMAIN_GTT, CpuAccess::Required)
        .map_err(|error| format!("creating the {what} buffer: {error}"))
}

fn with(what: &'static str) -> impl Fn(std::io::Error) -> String {
    move |error| format!("{what}: {error}")
}
