//! The Windows driver a machine's guest is given at install: NVIDIA's from setup, or AMD's or Intel's
//! for a machine on their cards. The guest keeps it in the folder of the host driver store whose name
//! the daemon gives Windows, so the names here must match the backends'.

use super::amd::{NO_AMD_SETUP, amd_driver_folder};
use super::intel::{NO_INTEL_SETUP, intel_driver_folder};
use crate::context::Context;
use crate::gpu_maker::GpuMaker;
use crate::library::NO_DRIVER_FILES;
use crate::machine::Machine;
use crate::settings::driver_folder;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestDriver {
    /// Where the driver's files are on this PC.
    pub folder: PathBuf,
    /// The folder they're in on the install disc, which the guest's setup script looks for.
    pub disc_folder: &'static str,
    /// The folder in the guest's host driver store they go to.
    pub store_folder: &'static str,
}

const NVIDIA_DISC_FOLDER: &str = "nvidia";
const NVIDIA_STORE_FOLDER: &str = "nv_dispi.inf_amd64_gpupv";
const AMD_DISC_FOLDER: &str = "amd";
const AMD_STORE_FOLDER: &str = "amdxc.inf_amd64_gpupv";
const INTEL_DISC_FOLDER: &str = "intel";
pub(crate) const INTEL_STORE_FOLDER: &str = "iigd_dch.inf_amd64_gpupv";
const NOT_WINDOWS: &str = "Only a Windows machine is given a GPU driver.";

/// The driver for this machine's guest, or why there is none yet.
pub fn guest_driver(context: &Context, machine: &Machine) -> Result<GuestDriver, &'static str> {
    match machine.windows_maker() {
        Some(GpuMaker::Nvidia) => {
            let folder = driver_folder(context);
            if !folder.is_dir() {
                return Err(NO_DRIVER_FILES);
            }
            Ok(GuestDriver { folder, disc_folder: NVIDIA_DISC_FOLDER, store_folder: NVIDIA_STORE_FOLDER })
        }
        Some(GpuMaker::Amd) => {
            let folder = amd_driver_folder(context).ok_or(NO_AMD_SETUP)?;
            Ok(GuestDriver { folder, disc_folder: AMD_DISC_FOLDER, store_folder: AMD_STORE_FOLDER })
        }
        Some(GpuMaker::Intel) => {
            let folder = intel_driver_folder(context).ok_or(NO_INTEL_SETUP)?;
            Ok(GuestDriver { folder, disc_folder: INTEL_DISC_FOLDER, store_folder: INTEL_STORE_FOLDER })
        }
        None => Err(NOT_WINDOWS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::intel::write_intel_setup;
    use crate::testing::{Scratch, scratch_context};
    use std::fs;

    #[test]
    fn a_machine_on_nvidia_gets_setups_driver_and_one_on_amd_gets_amds() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let windows = Machine::new("w", "W", "/d", "/v");
        let on_amd = Machine { amd_gpu: "0000:7c:00.0".into(), ..windows.clone() };
        assert_eq!(guest_driver(&context, &windows), Err(NO_DRIVER_FILES));
        assert_eq!(guest_driver(&context, &on_amd), Err(NO_AMD_SETUP));
        fs::create_dir_all(driver_folder(&context)).unwrap();
        let nvidia = guest_driver(&context, &windows).unwrap();
        assert_eq!((nvidia.disc_folder, nvidia.store_folder), (NVIDIA_DISC_FOLDER, NVIDIA_STORE_FOLDER));
        assert_eq!(guest_driver(&context, &on_amd), Err(NO_AMD_SETUP), "NVIDIA's driver is not AMD's");
    }

    #[test]
    fn a_machine_on_intel_is_given_intels_driver_and_never_another_makers() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        fs::create_dir_all(driver_folder(&context)).unwrap();
        let mut intel = Machine::new("w", "W", "/d", "/v");
        intel.use_windows_card(GpuMaker::Intel, "0000:00:02.0");
        assert_eq!(guest_driver(&context, &intel), Err(NO_INTEL_SETUP), "NVIDIA's driver is not Intel's");
        let unpacked = scratch.path("intel-driver");
        fs::create_dir_all(&unpacked).unwrap();
        write_intel_setup(&context, &unpacked, "32.0.101.9175").unwrap();
        let driver = guest_driver(&context, &intel).unwrap();
        assert_eq!((driver.disc_folder, driver.store_folder), (INTEL_DISC_FOLDER, INTEL_STORE_FOLDER));
        assert_eq!(driver.folder, unpacked);
    }

    /// The store folders are the names the daemons tell Windows, which the backends hold.
    #[test]
    fn the_store_folders_are_the_ones_the_daemons_name() {
        let backends = [
            (NVIDIA_STORE_FOLDER, "backend-nvidia/src/answers/registry.rs"),
            (AMD_STORE_FOLDER, "backend-amd/src/protocol/guest_driver.rs"),
            (INTEL_STORE_FOLDER, "backend-intel/src/protocol/guest_driver.rs"),
        ];
        for (store, source) in backends {
            let path = format!("{}/../{source}", env!("CARGO_MANIFEST_DIR"));
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains(&format!("FileRepository\\\\{store}\"")), "{store} is not in {source}");
        }
    }

    /// The script that runs in the guest at install has the same map by hand.
    #[test]
    fn the_guests_setup_script_puts_each_discs_folder_where_the_daemon_says() {
        let path = format!("{}/../../guest/setup.ps1", env!("CARGO_MANIFEST_DIR"));
        let script = fs::read_to_string(path).unwrap();
        let folders = [
            (NVIDIA_DISC_FOLDER, NVIDIA_STORE_FOLDER),
            (AMD_DISC_FOLDER, AMD_STORE_FOLDER),
            (INTEL_DISC_FOLDER, INTEL_STORE_FOLDER),
        ];
        for (disc, store) in folders {
            assert!(
                script.contains(&format!("{disc} = '{store}'")),
                "{disc} = '{store}' is not in setup.ps1"
            );
        }
    }
}
