//! What a Windows guest on an Intel card needs from setup: the user-mode half of Intel's Windows
//! driver, taken out of Intel's installer, and its version. The daemon reads them from `intel.json`
//! in the setup folder. The card itself is the machine's own choice, so the file names none.
//!
//! The Intel daemon tells Windows where each driver is by its file name in one folder, as AMD's
//! does, so every file it names has to be in the folder the guest gets.

use crate::context::Context;
use crate::failure::Result;
use crate::gpu_maker::GpuMaker;
use crate::render_cards::{RenderCard, render_card, render_cards};
use crate::settings::write_json;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const INTEL_SETUP_FILE: &str = "intel.json";
const DRIVER_FOLDER_FIELD: &str = "driver_folder";
const DRIVER_VERSION_FIELD: &str = "driver_version";
pub const NO_INTEL_SETUP: &str = "A Windows machine on an Intel card needs Intel's Windows driver unpacked first, \
                                  and it hasn't been.";

/// The files the Intel daemon names to Windows, apart from the kernel driver: the Direct3D drivers,
/// the OpenCL and Level Zero drivers, and the Vulkan manifests. The kernel driver is named too, as
/// the adapter is said to have one, but no file of it goes to the guest. A test holds the daemon to
/// this list.
pub const NAMED_FILES: [&str; 8] = [
    "igd9trinity64.dll",
    "igd10iumd64.dll",
    "igd12umd64.dll",
    "igdrcl64.dll",
    "igdrcl32.dll",
    "igvk64.json",
    "igvk32.json",
    "ze_intel_gpu64.dll",
];
/// The Intel cards the Intel daemon is written for, by PCI device id: Lunar Lake's Arc 140V. Intel's
/// other cards are in this PC's list as well, and offering them would put an iGPU that can't work in
/// the card choice of nearly every PC with an Intel processor.
pub const SUPPORTED_DEVICES: [u32; 1] = [0x64A0];

/// The Intel cards in this PC that a Windows machine can be set up for.
pub fn supported_intel_cards(context: &Context) -> Vec<RenderCard> {
    let cards = render_cards(context, GpuMaker::Intel);
    cards.into_iter().filter(|card| SUPPORTED_DEVICES.contains(&card.device)).collect()
}

/// Vulkan's manifests name the library the loader opens, beside them.
const VULKAN_MANIFESTS: [&str; 2] = ["igvk64.json", "igvk32.json"];
const SAME_FOLDER: &str = ".\\";

fn intel_setup_file(context: &Context) -> PathBuf {
    context.paths.setup_folder().join(INTEL_SETUP_FILE)
}

/// Where Intel's Windows driver was unpacked, if it was.
pub fn intel_driver_folder(context: &Context) -> Option<PathBuf> {
    let text = fs::read_to_string(intel_setup_file(context)).ok()?;
    let setup: Value = serde_json::from_str(&text).ok()?;
    let folder = PathBuf::from(setup.get(DRIVER_FOLDER_FIELD)?.as_str()?);
    folder.is_dir().then_some(folder)
}

/// Writes down where Intel's driver was unpacked, and its version, for the daemon.
pub fn write_intel_setup(context: &Context, folder: &Path, version: &str) -> Result<()> {
    let setup = json!({ DRIVER_FOLDER_FIELD: folder, DRIVER_VERSION_FIELD: version });
    write_json(&intel_setup_file(context), &setup)
}

/// The libraries a Vulkan manifest in the folder names for its driver.
fn manifest_libraries(folder: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for manifest in VULKAN_MANIFESTS {
        let Ok(text) = fs::read_to_string(folder.join(manifest)) else { continue };
        let Ok(manifest) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(path) = manifest["ICD"]["library_path"].as_str() {
            found.push(path.strip_prefix(SAME_FOLDER).unwrap_or(path).to_owned());
        }
    }
    found
}

/// The files Windows will be told to load that aren't in the folder.
pub fn missing_files(folder: &Path) -> Vec<String> {
    let mut wanted: Vec<String> = NAMED_FILES.iter().map(|name| (*name).to_owned()).collect();
    wanted.extend(manifest_libraries(folder));
    wanted.retain(|name| !folder.join(name).is_file());
    wanted.sort();
    wanted
}

/// Says what the folder lacks, if it lacks anything.
pub fn folder_problem(folder: &Path) -> Option<String> {
    let missing = missing_files(folder);
    (!missing.is_empty()).then(|| {
        format!(
            "Intel's driver lacks {}, which Windows is told to load, so it can't use the card.",
            missing.join(", ")
        )
    })
}

/// Why a Windows machine can't start on the Intel card at `pci`, if it can't.
pub fn intel_start_problems(context: &Context, pci: &str) -> Vec<String> {
    let mut found = Vec::new();
    if intel_driver_folder(context).is_none() {
        found.push(NO_INTEL_SETUP.to_owned());
    }
    if render_card(context, GpuMaker::Intel, pci).is_none() {
        found.push(format!(
            "The Intel card this machine runs on, at {pci}, isn't in this PC now. Choose another Intel card \
             in its settings."
        ));
    }
    found
}

#[cfg(test)]
pub mod stand_in {
    use crate::testing::Scratch;

    /// A Vulkan manifest whose driver is `library`, beside it.
    pub fn manifest(library: &str) -> String {
        serde_json::json!({"ICD": {"library_path": format!(".\\{library}")}}).to_string()
    }

    /// Writes an unpacked driver folder `folder` in `scratch` with every file the daemon names, and
    /// the libraries the manifests name. Each file holds its own path.
    pub fn lay_out_driver(scratch: &Scratch, folder: &str) {
        for name in super::NAMED_FILES {
            scratch.write(&format!("{folder}/{name}"), format!("{folder}/{name}"));
        }
        scratch.write(&format!("{folder}/igvk64.json"), manifest("igvk64.dll"));
        scratch.write(&format!("{folder}/igvk32.json"), manifest("igvk32.dll"));
        scratch.write(&format!("{folder}/igvk64.dll"), "vulkan");
        scratch.write(&format!("{folder}/igvk32.dll"), "vulkan");
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::lay_out_driver;
    use super::*;
    use crate::linux::amd::stand_in::fake_render_node;
    use crate::testing::{Scratch, scratch_context};
    use regex::Regex;
    use std::collections::BTreeSet;

    const CARD: &str = "0000:00:02.0";
    /// The kernel driver's name, which the daemon gives and the guest gets no file for.
    const KERNEL_DRIVER: &str = "igdkmdn64.sys";

    #[test]
    fn only_the_card_the_daemon_is_written_for_counts() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        fake_render_node(&scratch, 128, CARD, "0x8086", 0x46a6);
        fake_render_node(&scratch, 129, "0000:00:03.0", "0x8086", 0x64a0);
        let found: Vec<String> = supported_intel_cards(&context).into_iter().map(|card| card.pci).collect();
        assert_eq!(found, ["0000:00:03.0"]);
    }

    #[test]
    fn with_no_driver_and_no_card_both_are_said() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let found = intel_start_problems(&context, CARD);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0], NO_INTEL_SETUP);
        assert!(found[1].contains("at 0000:00:02.0, isn't in this PC now"));
    }

    #[test]
    fn with_the_driver_unpacked_and_the_card_here_it_can_start() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        lay_out_driver(&scratch, "intel-driver");
        let folder = scratch.path("intel-driver");
        write_intel_setup(&context, &folder, "32.0.101.9175").unwrap();
        fake_render_node(&scratch, 128, CARD, "0x8086", 0x64a0);
        fake_render_node(&scratch, 129, "0000:7c:00.0", "0x1002", 0x13c0);
        assert_eq!(intel_driver_folder(&context), Some(folder));
        assert_eq!(intel_start_problems(&context, CARD), Vec::<String>::new());
        let on_amd = intel_start_problems(&context, "0000:7c:00.0");
        assert_eq!(on_amd.len(), 1, "an AMD card is not an Intel one: {on_amd:?}");
    }

    #[test]
    fn setup_is_written_for_the_daemon_by_the_names_it_reads() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let folder = scratch.path("driver");
        fs::create_dir_all(&folder).unwrap();
        write_intel_setup(&context, &folder, "32.0.101.9175").unwrap();
        let text = fs::read_to_string(intel_setup_file(&context)).unwrap();
        let written: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(written["driver_folder"], folder.display().to_string());
        assert_eq!(written["driver_version"], "32.0.101.9175");
    }

    #[test]
    fn a_file_the_daemon_names_or_a_manifest_names_has_to_be_there() {
        let scratch = Scratch::new();
        lay_out_driver(&scratch, "driver");
        let folder = scratch.path("driver");
        assert_eq!(folder_problem(&folder), None);
        fs::remove_file(folder.join("igvk32.dll")).unwrap();
        fs::remove_file(folder.join("igd12umd64.dll")).unwrap();
        let problem = folder_problem(&folder).unwrap();
        assert!(problem.contains("igd12umd64.dll, igvk32.dll"), "{problem}");
    }

    /// The file names in the daemon's answers, from its source: a name the daemon gives that isn't
    /// here would never be checked for.
    #[test]
    fn the_named_files_are_the_ones_the_daemon_names() {
        let file_name = Regex::new(r#""([A-Za-z0-9_-]+\.(?:dll|json|sys))""#).unwrap();
        let path = format!("{}/../backend-intel/src/protocol/guest_driver.rs", env!("CARGO_MANIFEST_DIR"));
        let text = fs::read_to_string(path).unwrap();
        let named: BTreeSet<String> =
            file_name.captures_iter(&text).map(|found| found[1].to_owned()).collect();
        let ours: BTreeSet<String> =
            NAMED_FILES.iter().copied().chain([KERNEL_DRIVER]).map(str::to_owned).collect();
        assert_eq!(named, ours);
    }
}
