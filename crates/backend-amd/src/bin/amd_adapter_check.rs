//! Builds the guest's adapter block from the real AMD card and sets its key fields beside what
//! amdgpu reports on its own in sysfs, so a field read from the wrong INFO offset shows up.
//!
//! The sysfs side is independent of the INFO queries the block is built from: memory sizes from
//! amdgpu's mem_info files, the graphics block's version from IP discovery, and compute units from
//! the compute driver's topology. Run it with no arguments to use the one AMD card, or pass a PCI
//! address to pick one of several. It never opens anything but that card's render node.

use guesthouse_backend_amd::Device;
use guesthouse_backend_amd::protocol::adapter_info::{self, KeyFields};
use std::fs;
use std::path::Path;
use std::process::ExitCode;

const VRAM_TOTAL_FILE: &str = "mem_info_vram_total";
const VISIBLE_VRAM_FILE: &str = "mem_info_vis_vram_total";
const GTT_TOTAL_FILE: &str = "mem_info_gtt_total";
/// IP discovery numbers the graphics block 11, GC_HWID in amdgpu.
const GRAPHICS_IP_FOLDER: &str = "ip_discovery/die/0/11/0";
const TOPOLOGY_FOLDER: &str = "/sys/class/kfd/kfd/topology/nodes";
const MIB: u64 = 1 << 20;

fn main() -> ExitCode {
    let address = std::env::args().nth(1);
    match run(address.as_deref()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("amd-adapter-check: some fields disagree with amdgpu");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("amd-adapter-check failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn read_number(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// A topology node's properties, as `name value` lines.
fn property(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

/// The compute units the compute driver counts for the card at this PCI location.
fn topology_compute_units(location: u64) -> Option<u64> {
    for entry in fs::read_dir(TOPOLOGY_FOLDER).ok()?.flatten() {
        let Ok(text) = fs::read_to_string(entry.path().join("properties")) else { continue };
        if property(&text, "location_id") == Some(location) && property(&text, "simd_count")? != 0 {
            return Some(property(&text, "simd_count")? / property(&text, "simd_per_cu")?.max(1));
        }
    }
    None
}

/// Bus, device and function packed the way the topology's location_id packs them, from a PCI
/// address such as `0000:7c:00.0`.
fn pci_location(folder: &Path) -> Option<u64> {
    let name = fs::canonicalize(folder).ok()?.file_name()?.to_string_lossy().into_owned();
    let mut parts = name.split(':');
    let (_domain, bus, slot) = (parts.next()?, parts.next()?, parts.next()?);
    let (device, function) = slot.split_once('.')?;
    let hex = |text: &str| u64::from_str_radix(text, 16).ok();
    Some(hex(bus)? << 8 | hex(device)? << 3 | hex(function)?)
}

/// Every device file this process holds, to show the check opened the AMD node and nothing else.
fn open_device_files() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/proc/self/fd") else { return Vec::new() };
    let mut files: Vec<String> = entries
        .flatten()
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .map(|target| target.to_string_lossy().into_owned())
        .filter(|target| target.starts_with("/dev/"))
        .collect();
    files.sort();
    files.dedup();
    files
}

struct Row {
    name: &'static str,
    ours: String,
    amdgpu: String,
    agrees: Option<bool>,
}

fn row(name: &'static str, ours: impl ToString, amdgpu: Option<impl ToString + PartialEq<String>>) -> Row {
    let ours = ours.to_string();
    let agrees = amdgpu.as_ref().map(|value| *value == ours);
    let amdgpu = amdgpu.map_or_else(|| "not reported".to_owned(), |value| value.to_string());
    Row { name, ours, amdgpu, agrees }
}

fn run(address: Option<&str>) -> Result<bool, String> {
    let device = Device::open(address).map_err(|error| format!("opening the AMD card: {error}"))?;
    let info = device.info();
    let folder = device.device_folder().ok_or("the card has no sysfs folder")?;
    let block = adapter_info::build(info);
    let key = KeyFields::read(&block).ok_or("the block is too short to read back")?;

    let vram = read_number(&folder.join(VRAM_TOTAL_FILE)).map(|bytes| (bytes / MIB).to_string());
    let visible = read_number(&folder.join(VISIBLE_VRAM_FILE)).map(|bytes| (bytes / MIB).to_string());
    let gtt = read_number(&folder.join(GTT_TOTAL_FILE)).map(|bytes| (bytes / MIB).to_string());
    let ip = folder.join(GRAPHICS_IP_FOLDER);
    let gfx_level = match (read_number(&ip.join("major")), read_number(&ip.join("minor"))) {
        (Some(major), Some(minor)) => Some(format!("{major}.{minor}")),
        _ => None,
    };
    let cus = pci_location(folder).and_then(topology_compute_units).map(|count| count.to_string());
    let (gfx_major, gfx_minor) = info.details.gfx_version;
    // From GFX10 the block counts workgroup processors, two compute units each.
    let units_per_mask_bit = if gfx_major >= 10 { 2 } else { 1 };
    let rows = [
        row("chip id", format!("{:#06x}", key.chip_id), Some(format!("{:#06x}", info.device_id))),
        row("compute units", key.active_units * units_per_mask_bit, cus),
        row("vram MiB", (key.visible_heap + key.invisible_heap) / MIB, vram),
        row("visible vram MiB", key.visible_heap / MIB, visible),
        row("gtt MiB (INFO; not in the block)", info.gtt_total / MIB, gtt),
        row("gfx level", format!("{gfx_major}.{gfx_minor}"), gfx_level),
    ];

    println!("{:<34} {:>14} {:>14}", "field", "adapter block", "amdgpu");
    for row in &rows {
        let mark = match row.agrees {
            Some(true) => "",
            Some(false) => "  <- differs",
            None => "",
        };
        println!("{:<34} {:>14} {:>14}{mark}", row.name, row.ours, row.amdgpu);
    }
    println!();
    println!("family                {}", key.family);
    println!("in a virtual machine  {}", key.in_virtual_machine);
    println!("GB_ADDR_CONFIG        {:#x}", key.gb_addr_config);
    println!(
        "engines/arrays/CUs    {} / {} / {}",
        key.shader_engines, key.arrays_per_engine, key.cus_per_array
    );
    println!("engine clock          {} MHz", key.engine_clock_mhz);
    println!("block checksum holds  {}", key.checksum_holds);
    println!(
        "firmware ME/PFP/MEC   {:#x} / {:#x} / {:#x}",
        info.details.firmware.me, info.details.firmware.pfp, info.details.firmware.mec
    );

    println!("device files open     {}", open_device_files().join(", "));

    let all_agree = rows.iter().all(|row| row.agrees != Some(false));
    Ok(all_agree && key.checksum_holds && key.in_virtual_machine)
}
