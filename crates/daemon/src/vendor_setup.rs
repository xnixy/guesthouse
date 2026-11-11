//! What a guest on an AMD or Intel card was set up with. The app's `setup.json` stays the NVIDIA
//! card's record, which this daemon does not read: the other vendors' drivers for a Windows guest
//! are each described by a file of their own in the same store, which the app writes when it
//! unpacks that vendor's installer. The card is each machine's own and comes in its variable from
//! [`crate::card_choice`]; a `pci` in the file is only for a build with no NVIDIA backend, started
//! by hand.

use crate::store::store;
use guesthouse_backend::{DriverVersion, PciAddress, Setup};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

/// A vendor's setup file, and the vendor's name for the messages about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorFile {
    pub path: &'static str,
    pub vendor: &'static str,
}

pub const AMD: VendorFile = VendorFile { path: "setup/amd.json", vendor: "AMD" };
pub const INTEL: VendorFile = VendorFile { path: "setup/intel.json", vendor: "Intel" };

impl VendorFile {
    /// Reads the file from the store.
    pub fn read(&self) -> Result<Setup, String> {
        let path = store().join(self.path);
        let vendor = self.vendor;
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("{vendor} setup has not been written ({}: {error}).", path.display()))?;
        self.parse(&text)
    }

    fn parse(&self, text: &str) -> Result<Setup, String> {
        let vendor = self.vendor;
        let setup: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let folder = setup["driver_folder"]
            .as_str()
            .ok_or_else(|| format!("The {vendor} setup names no driver folder."))?;
        let driver_folder = PathBuf::from(folder);
        if !driver_folder.is_dir() {
            return Err(format!("The {vendor} driver folder {} is not there.", driver_folder.display()));
        }
        let version = setup["driver_version"]
            .as_str()
            .ok_or_else(|| format!("The {vendor} setup has no guest driver version."))?;
        let guest_driver_version = DriverVersion::parse(version)
            .ok_or_else(|| format!("The {vendor} guest driver version {version} is not four numbers."))?;
        let card_address = match setup["pci"].as_str() {
            Some(text) => Some(
                PciAddress::parse(text)
                    .ok_or_else(|| format!("The {vendor} card address {text} is not a PCI address."))?,
            ),
            None => None,
        };
        Ok(Setup {
            guest_branch: guest_driver_version.to_string(),
            guest_driver_version,
            driver_folder,
            card: None,
            card_address,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process;

    #[test]
    fn the_version_and_the_card_address_come_from_the_file() {
        let folder = env::temp_dir().join(format!("guesthouse-amd-setup-{}", process::id()));
        fs::create_dir_all(&folder).unwrap();
        let text = format!(
            "{{\"driver_folder\": {:?}, \"driver_version\": \"32.0.32015.12012\", \"pci\": \"0000:7c:00.0\"}}",
            folder.to_str().unwrap()
        );
        let setup = AMD.parse(&text).unwrap();
        assert_eq!(setup.guest_driver_version.to_string(), "32.0.32015.12012");
        assert_eq!(setup.card_address.map(|address| address.to_string()).as_deref(), Some("0000:7c:00.0"));
        assert_eq!(setup.card, None);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn an_intel_file_reads_the_same_way() {
        let folder = env::temp_dir().join(format!("guesthouse-intel-setup-{}", process::id()));
        fs::create_dir_all(&folder).unwrap();
        let text = format!(
            "{{\"driver_folder\": {:?}, \"driver_version\": \"32.0.101.9175\"}}",
            folder.to_str().unwrap()
        );
        let setup = INTEL.parse(&text).unwrap();
        assert_eq!(setup.guest_branch, "32.0.101.9175");
        assert_eq!(setup.card_address, None);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_version_that_is_not_four_numbers_is_refused() {
        let folder = env::temp_dir().join(format!("guesthouse-amd-setup-bad-{}", process::id()));
        fs::create_dir_all(&folder).unwrap();
        let text =
            format!("{{\"driver_folder\": {:?}, \"driver_version\": \"617.42\"}}", folder.to_str().unwrap());
        assert!(AMD.parse(&text).unwrap_err().contains("four numbers"));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_missing_driver_folder_is_refused_in_the_vendors_name() {
        let text = "{\"driver_folder\": \"/no/such/intel-driver\", \"driver_version\": \"32.0.101.9175\"}";
        assert_eq!(
            INTEL.parse(text).unwrap_err(),
            "The Intel driver folder /no/such/intel-driver is not there."
        );
    }
}
