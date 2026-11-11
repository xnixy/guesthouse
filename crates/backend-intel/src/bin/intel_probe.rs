//! `intel-probe [PCI_ADDRESS]`: what the host's xe driver says about an Intel card, in a form to
//! paste into an issue.
//!
//! It reads sysfs and asks xe its read-only device queries, and nothing else: no address space,
//! buffer or queue is made, so nothing runs on the card and the desktop is not touched. With no
//! address it takes the first Intel card on xe.

use guesthouse_backend_intel::card::CardFacts;
use guesthouse_backend_intel::generation;
use guesthouse_backend_intel::ioctl::{self, INTEL_VENDOR, XE_DRIVER};
use guesthouse_backend_intel::xe_query::{FirmwareVersion, Gt, MemoryRegion};
use std::path::Path;
use std::process::ExitCode;

const MIB: u64 = 1 << 20;

fn main() -> ExitCode {
    let address = std::env::args().nth(1);
    let Some(node) = ioctl::intel_render_node(address.as_deref()) else {
        eprintln!("No Intel card on the {XE_DRIVER} driver was found.");
        for (name, driver) in ioctl::intel_nodes_in(Path::new("/sys/class/drm")) {
            let driver = driver.as_deref().unwrap_or("no driver");
            eprintln!("  {} is an Intel card on {driver}", name.to_string_lossy());
        }
        return ExitCode::FAILURE;
    };
    let card = match CardFacts::read(&node) {
        Ok(card) => card,
        Err(error) => {
            eprintln!("Opening {} failed: {error}", node.device_file.display());
            return ExitCode::FAILURE;
        }
    };
    print_card(&node.device_file, &card);
    ExitCode::SUCCESS
}

fn or_unknown(text: &Option<String>) -> &str {
    text.as_deref().unwrap_or("unknown")
}

fn print_card(device_file: &Path, card: &CardFacts) {
    println!("intel-probe {}", env!("CARGO_PKG_VERSION"));
    println!("node            {} ({})", device_file.display(), or_unknown(&card.address));
    println!(
        "kernel          {}, driver {}, xe source {}",
        or_unknown(&card.kernel),
        card.driver,
        or_unknown(&card.xe_source)
    );
    let pci = card.pci;
    let vendor_note = if pci.vendor == INTEL_VENDOR { "" } else { " (not Intel)" };
    println!(
        "pci             {:04x}:{:04x} rev {:#04x}, subsystem {:04x}:{:04x}, class {:#08x}{vendor_note}",
        pci.vendor, pci.device, pci.revision, pci.subsystem_vendor, pci.subsystem_device, pci.class
    );
    let config = card.config;
    println!(
        "config          device {:#06x} rev {}, flags {:#x}, alignment {:#x}, va bits {}, top queue priority {}",
        config.device_id,
        config.revision,
        config.flags,
        config.min_alignment,
        config.va_bits,
        config.max_queue_priority
    );
    for region in &card.regions {
        let class = if region.class == MemoryRegion::SYSTEM { "system" } else { "vram" };
        println!(
            "memory          {class} {}: {} MiB, {} MiB used, page {:#x}, cpu visible {} MiB",
            region.instance,
            region.total / MIB,
            region.used / MIB,
            region.min_page_size,
            region.cpu_visible / MIB
        );
    }
    for gt in &card.gts {
        let kind = if gt.kind == Gt::MAIN {
            "main"
        } else if gt.kind == Gt::MEDIA {
            "media"
        } else {
            "other"
        };
        println!(
            "gt              {} {kind} on tile {}: ip {}, timestamp {} Hz, memory near {:#x} far {:#x}",
            gt.id, gt.tile, gt.ip, gt.reference_clock, gt.near_memory, gt.far_memory
        );
    }
    let engines: Vec<String> = card
        .engines
        .iter()
        .map(|engine| format!("{}{} (gt {})", engine.class_name(), engine.instance, engine.gt))
        .collect();
    println!("engines         {}", engines.join(", "));
    for mask in &card.topology {
        let bytes: String = mask.mask.iter().map(|byte| format!("{byte:02x}")).collect();
        println!("topology        gt {} {}: {bytes} ({} set)", mask.gt, mask.name(), mask.bits_set());
    }
    println!(
        "counted         {} dual subslices x {} EUs = {} EUs, slices {}",
        card.dual_subslices(),
        card.eus_per_dual_subslice(),
        card.eu_count(),
        card.slices().map_or("unknown".into(), |slices| slices.to_string())
    );
    for entry in &card.hwconfig.entries {
        let values: Vec<String> = entry.values.iter().map(u32::to_string).collect();
        let name = entry.name().map(|name| format!(" ({name})")).unwrap_or_default();
        println!("hwconfig        {}{name}: {}", entry.key, values.join(" "));
    }
    println!("firmware        GuC {}, HuC {}", firmware(card.guc), firmware(card.huc));
    let f = card.frequencies;
    let mhz = |value: Option<u32>| value.map_or("-".to_string(), |value| value.to_string());
    println!(
        "frequency MHz   RP0 {}, RPn {}, RPe {}, set {}..{}",
        mhz(f.highest),
        mhz(f.lowest),
        mhz(f.efficient),
        mhz(f.min_set),
        mhz(f.max_set)
    );
    for (what, error) in &card.unanswered {
        println!("not answered    {what}: {error}");
    }
    match card.main_gt().and_then(|gt| generation::for_render_ip(gt.ip)) {
        Some(generation) => println!("profile         {}", generation.name),
        None => println!("profile         none: this render IP has no profile yet"),
    }
}

fn firmware(version: Option<FirmwareVersion>) -> String {
    version.map_or("unknown".into(), |version| version.to_string())
}
