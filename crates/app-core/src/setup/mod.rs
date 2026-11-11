//! First-time setup: looks at this PC, and takes what is needed from NVIDIA's driver installer, and
//! for a Windows machine on an AMD or Intel card, from theirs.
//!
//! Nothing here touches the card. It's read through sysfs and `nvidia-smi`, so a VM using it isn't
//! disturbed. Everything setup makes goes in the store's setup folder, and `setup.json` there says
//! where.

pub mod amd;
pub mod amd_driver;
pub mod amd_guest_folder;
pub mod cards;
pub mod checks;
pub mod driver;
pub mod guest_driver;
pub mod guest_files;
pub mod inf;
pub mod intel;
pub mod intel_driver;
pub mod intel_inf;
pub mod toolkit;

use crate::context::Context;
use crate::settings::{driver_folder, read_setup};

/// Whether there's nothing to ask for: the driver's files are here and the card is known. The daemon
/// works out the rest when it starts.
pub fn setup_done(context: &Context) -> bool {
    read_setup(context).contains_key("card") && driver_folder(context).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::write_setup;
    use crate::testing::{Scratch, scratch_context};
    use serde_json::json;
    use std::fs;

    #[test]
    fn setup_needs_the_drivers_files_and_the_card() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        assert!(!setup_done(&context));
        let folder = context.paths.store.join("setup/driver");
        fs::create_dir_all(&folder).unwrap();
        let driver = json!({"driver_folder": folder});
        write_setup(&context, driver.as_object().cloned().unwrap()).unwrap();
        assert!(!setup_done(&context), "the driver alone is not enough");
        let card = json!({"card": {"device": 1}});
        write_setup(&context, card.as_object().cloned().unwrap()).unwrap();
        assert!(setup_done(&context));
    }
}
