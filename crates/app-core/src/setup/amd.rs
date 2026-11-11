//! What a Windows guest on an AMD card needs from setup: AMD's Windows driver, unpacked, and its
//! version. The daemon reads them from `amd.json` in the setup folder. The card itself is the
//! machine's own choice, so the file names none.

use super::amd_guest_folder::{add_components, folder_problem};
use crate::context::Context;
use crate::failure::Result;
use crate::linux::amd::amd_card;
use crate::settings::write_json;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const AMD_SETUP_FILE: &str = "amd.json";
const DRIVER_FOLDER_FIELD: &str = "driver_folder";
const DRIVER_VERSION_FIELD: &str = "driver_version";
pub const NO_AMD_SETUP: &str = "A Windows machine on an AMD card needs AMD's Windows driver unpacked first, and it \
                                hasn't been.";

fn amd_setup_file(context: &Context) -> PathBuf {
    context.paths.setup_folder().join(AMD_SETUP_FILE)
}

/// Where AMD's Windows driver was unpacked, if it was.
pub fn amd_driver_folder(context: &Context) -> Option<PathBuf> {
    let text = fs::read_to_string(amd_setup_file(context)).ok()?;
    let setup: Value = serde_json::from_str(&text).ok()?;
    let folder = PathBuf::from(setup.get(DRIVER_FOLDER_FIELD)?.as_str()?);
    folder.is_dir().then_some(folder)
}

/// The version of AMD's Windows driver that was unpacked, if one was.
pub fn amd_driver_version(context: &Context) -> Option<String> {
    let text = fs::read_to_string(amd_setup_file(context)).ok()?;
    let setup: Value = serde_json::from_str(&text).ok()?;
    Some(setup.get(DRIVER_VERSION_FIELD)?.as_str()?.to_owned())
}

/// Brings a driver unpacked by an older version of the app up to what the guest gets now, and says
/// what it still lacks, if anything. Unpacking does the same, so after the first time this only looks.
pub fn complete_amd_driver(context: &Context) -> Option<String> {
    let folder = amd_driver_folder(context)?;
    if let Err(failure) = add_components(&folder) {
        return Some(format!("AMD's Vulkan and OpenGL could not be added to its driver: {}", failure.0));
    }
    let problem = folder_problem(&folder)?;
    let setup = amd_setup_file(context);
    Some(format!(
        "{problem} To unpack it again, delete {}, and a new machine on this card asks for AMD's installer.",
        setup.display()
    ))
}

/// Writes down where AMD's driver was unpacked, and its version, for the daemon.
pub fn write_amd_setup(context: &Context, folder: &Path, version: &str) -> Result<()> {
    let setup = json!({ DRIVER_FOLDER_FIELD: folder, DRIVER_VERSION_FIELD: version });
    write_json(&amd_setup_file(context), &setup)
}

/// Why a Windows machine can't start on the AMD card at `pci`, if it can't.
pub fn amd_start_problems(context: &Context, pci: &str) -> Vec<String> {
    let mut found = Vec::new();
    if amd_driver_folder(context).is_none() {
        found.push(NO_AMD_SETUP.to_owned());
    }
    if amd_card(context, pci).is_none() {
        found.push(format!(
            "The AMD card this machine runs on, at {pci}, isn't in this PC now. Choose another AMD card in \
             its settings."
        ));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::amd::stand_in::fake_render_node;
    use crate::setup::amd_guest_folder::stand_in::lay_out_driver;
    use crate::testing::{Scratch, scratch_context};

    const CARD: &str = "0000:7c:00.0";

    #[test]
    fn with_no_driver_and_no_card_both_are_said() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let found = amd_start_problems(&context, CARD);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0], NO_AMD_SETUP);
        assert!(found[1].contains("at 0000:7c:00.0, isn't in this PC now"));
    }

    /// What 0.4.0 left for the newer cards: the user-mode driver's folder alone written down, with
    /// Vulkan and OpenGL beside it.
    #[test]
    fn a_driver_unpacked_before_is_completed_once_and_then_only_looked_at() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        lay_out_driver(&scratch, "amd-driver/WT6A_INF", "B026716", false);
        let folder = scratch.path("amd-driver/WT6A_INF/B026716");
        write_amd_setup(&context, &folder, "32.0.32015.12012").unwrap();
        assert!(!folder.join("atio6axx.dll").exists());
        assert_eq!(complete_amd_driver(&context), None);
        assert!(folder.join("atio6axx.dll").is_file() && folder.join("amd-vulkan64.json").is_file());
        assert_eq!(complete_amd_driver(&context), None);
        fs::remove_dir_all(scratch.path("amd-driver/WT6A_INF/amdogl")).unwrap();
        fs::remove_file(folder.join("atioglxx.dll")).unwrap();
        assert!(complete_amd_driver(&context).unwrap().contains("atioglxx.dll"));
    }

    #[test]
    fn with_the_driver_unpacked_and_the_card_here_it_can_start() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let driver = scratch.path("amd-driver");
        fs::create_dir_all(&driver).unwrap();
        fs::create_dir_all(context.paths.setup_folder()).unwrap();
        let setup = serde_json::json!({ DRIVER_FOLDER_FIELD: driver });
        fs::write(amd_setup_file(&context), setup.to_string()).unwrap();
        fake_render_node(&scratch, 128, CARD, "0x1002", 0x13c0);
        assert_eq!(amd_driver_folder(&context), Some(driver));
        assert_eq!(amd_start_problems(&context, CARD), Vec::<String>::new());
    }
}
