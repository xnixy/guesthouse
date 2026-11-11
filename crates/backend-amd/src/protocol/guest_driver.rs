//! Where the guest finds AMD's user-mode driver.
//!
//! The paravirtual adapter's kernel driver is Windows' own. The user-mode driver is AMD's, and the
//! guest asks for its path with adapter query type 1. It keeps the version dword it sent and reads
//! a UTF-16 path after that. The same query is how a guest is pointed at its driver today.
//!
//! The files have to be in the guest, under the host driver store, at the path given here. Nothing
//! here copies them.

use super::bytes::dword;
use guesthouse_backend::guest_text::{self, utf16_list, utf16_zero};
use guesthouse_backend::{RegistryName, RegistryValue, VendorQuery};
use guesthouse_messages::registry::TYPE_STRING_LIST;
use std::sync::atomic::{AtomicBool, Ordering};

/// The Adrenalin drivers these answers are for. AMD ships one release as two drivers, the first for
/// RDNA3 and later cards and the second for RDNA1 and RDNA2, so each has its own version. Any other
/// is refused at start.
pub const GUEST_RELEASES: [&str; 2] = ["32.0.32015.12012", "32.0.21045.20006"];

/// Where the guest is told the driver lives. It rewrites `DriverStore` to its host driver store.
pub const DRIVER_STORE: &str = "C:\\Windows\\System32\\DriverStore\\FileRepository\\amdxc.inf_amd64_gpupv";

/// AMD's user-mode drivers by the Direct3D version the guest asks for, as its INF lists them in
/// UserModeDriverName: Direct3D 9, 10, 11 and 12. 10 and 11 share a driver.
const USER_DRIVERS: [&str; 4] = ["atidx9loader64.dll", "amdxx64.dll", "amdxx64.dll", "amdxc64.dll"];
/// The same for a 32-bit process, from UserModeDriverNameWow.
const USER_DRIVERS_32_BIT: [&str; 4] = ["atidx9loader32.dll", "amdxx32.dll", "amdxx32.dll", "amdxc32.dll"];
/// Where the version sits in the query, KMTUMDVERSION: 0 for Direct3D 9 up to 3 for Direct3D 12.
const VERSION_FIELD: usize = 0;
/// A version past the list gets the newest driver.
const NEWEST: usize = USER_DRIVERS.len() - 1;
const KERNEL_DRIVER: &str = "amdkmdag.sys";

static LOGGED_DRIVER_NAME: AtomicBool = AtomicBool::new(false);

/// Writes the user-mode driver path into `out`, keeping the guest's version dword.
pub fn write_name(query: &VendorQuery<'_>, out: &mut Vec<u8>) {
    let version = dword(query.data, VERSION_FIELD).map_or(NEWEST, |version| (version as usize).min(NEWEST));
    let drivers = if query.process_is_32_bit { USER_DRIVERS_32_BIT } else { USER_DRIVERS };
    let name = drivers[version];
    let path = format!("{DRIVER_STORE}\\{name}");
    // The guest asks again for every process it starts. The first answer is the one that matters.
    if !LOGGED_DRIVER_NAME.swap(true, Ordering::Relaxed) {
        log::info!("guest asked for the user-mode driver, answered with {name}");
    } else {
        log::debug!("guest asked for the user-mode driver, answered with {name}");
    }
    guest_text::user_driver_name(query, &path, out);
}

/// The driver paths, the attribute list DXCore asks for, and the values the Vulkan and OpenGL
/// loaders look up. Any other registry name has no answer.
pub fn registry(name: &RegistryName) -> Option<RegistryValue> {
    match name {
        RegistryName::DriverStorePath => {
            Some(RegistryValue { value_type: None, bytes: utf16_zero(DRIVER_STORE) })
        }
        RegistryName::DriverImagePath => {
            let path = format!("{DRIVER_STORE}\\{KERNEL_DRIVER}");
            Some(RegistryValue { value_type: None, bytes: utf16_zero(&path) })
        }
        RegistryName::Value(name) if name.eq_ignore_ascii_case(guest_text::DXCORE_ATTRIBUTES_VALUE) => {
            let attributes = utf16_list(guest_text::DXCORE_GRAPHICS_ATTRIBUTES);
            Some(RegistryValue { value_type: Some(TYPE_STRING_LIST), bytes: attributes })
        }
        RegistryName::Value(name) => super::api_drivers::registry_value(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guest's version dword, ahead of the path.
    const VERSION_FIELD: usize = 4;

    fn name_of(out: &[u8]) -> String {
        let units = out[VERSION_FIELD..].as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit));
        let units: Vec<u16> = units.take_while(|&unit| unit != 0).collect();
        String::from_utf16_lossy(&units)
    }

    const DIRECT3D_9: u8 = 0;
    const DIRECT3D_11: u8 = 2;
    const DIRECT3D_12: u8 = 3;

    fn name_for(version: u8, process_is_32_bit: bool) -> String {
        let mut out = Vec::new();
        let data = [version, 0, 0, 0, 1, 2, 3, 4];
        let query = VendorQuery {
            kind: guest_text::QUERY_USER_DRIVER_NAME,
            size: data.len(),
            data: &data,
            process_is_32_bit,
        };
        write_name(&query, &mut out);
        assert_eq!(out[0], version);
        name_of(&out)
    }

    #[test]
    fn each_direct3d_version_gets_the_driver_amd_lists_for_it() {
        assert!(name_for(DIRECT3D_12, false).ends_with("\\amdxc64.dll"));
        assert!(name_for(DIRECT3D_11, false).ends_with("\\amdxx64.dll"));
        assert!(name_for(DIRECT3D_9, false).ends_with("\\atidx9loader64.dll"));
        assert!(name_for(DIRECT3D_12, false).starts_with(DRIVER_STORE));
    }

    #[test]
    fn a_32_bit_process_gets_the_32_bit_drivers() {
        assert!(name_for(DIRECT3D_12, true).ends_with("\\amdxc32.dll"));
        assert!(name_for(DIRECT3D_11, true).ends_with("\\amdxx32.dll"));
    }

    #[test]
    fn a_version_past_the_list_gets_the_newest_driver() {
        assert!(name_for(9, false).ends_with("\\amdxc64.dll"));
    }

    #[test]
    fn the_driver_paths_are_under_the_store_and_nothing_else_is_answered() {
        let store = registry(&RegistryName::DriverStorePath).unwrap();
        let image = registry(&RegistryName::DriverImagePath).unwrap();
        assert_eq!(store.bytes, utf16_zero(DRIVER_STORE));
        assert!(name_text(&image.bytes).ends_with(KERNEL_DRIVER));
        assert!(registry(&RegistryName::Value("vulkan".into())).is_none());
        let attributes = registry(&RegistryName::Value("dxcoreattributes".into())).unwrap();
        assert_eq!(attributes.value_type, Some(TYPE_STRING_LIST));
        let units = attributes.bytes.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit));
        let text = String::from_utf16_lossy(&units.collect::<Vec<_>>());
        assert!(text.contains("B69EB219-3DED-4464-979F-A00BD4687006"), "{text}");
        assert!(text.contains("8C47866B-7583-450D-F0F0-6BADA895AF4B"), "{text}");
    }

    fn name_text(bytes: &[u8]) -> String {
        let units = bytes.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit));
        String::from_utf16_lossy(&units.take_while(|&unit| unit != 0).collect::<Vec<_>>())
    }
}
