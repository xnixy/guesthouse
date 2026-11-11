//! Where the guest finds Intel's user-mode drivers, and the registry values their loaders ask the
//! adapter for.
//!
//! The paravirtual adapter's kernel driver is Windows' own, and the user-mode drivers are Intel's.
//! The guest asks for a Direct3D driver's path with adapter query type 1, and the other loaders ask
//! the adapter for registry values by name. The file names are the ones Intel's INF gives this
//! card. The files have to be in the guest, under the host driver store, at [`DRIVER_STORE`].
//! Nothing here copies them.

use super::bytes::dword;
use guesthouse_backend::guest_text::{self, utf16_list, utf16_zero};
use guesthouse_backend::{RegistryName, RegistryValue, Status, VendorQuery};
use guesthouse_messages::registry::TYPE_STRING_LIST;

/// The release of Intel's driver these answers are for. The adapter record and the private data
/// change with the release, so any other is refused at start.
pub const GUEST_RELEASES: [&str; 1] = ["32.0.101.9175"];

/// Where the guest is told the driver lives. It rewrites `DriverStore` to its host driver store.
pub const DRIVER_STORE: &str = "C:\\Windows\\System32\\DriverStore\\FileRepository\\iigd_dch.inf_amd64_gpupv";

/// The user-mode drivers by the Direct3D version the guest asks for, as the INF lists them in
/// UserModeDriverName: Direct3D 9, 10, 11 and 12. 10 and 11 share a driver.
const USER_DRIVERS: [&str; 4] = ["igd9trinity64.dll", "igd10iumd64.dll", "igd10iumd64.dll", "igd12umd64.dll"];
/// Where the version sits in the query, KMTUMDVERSION: 0 for Direct3D 9 up to 3 for Direct3D 12.
const VERSION_FIELD: usize = 0;
const NEWEST: usize = USER_DRIVERS.len() - 1;
const KERNEL_DRIVER: &str = "igdkmdn64.sys";

/// The values the loaders ask for, by the names the INF gives them. Registry names ignore case.
const VALUES: [(&str, &str); 5] = [
    ("OpenCLDriverName", "igdrcl64.dll"),
    ("OpenCLDriverNameWow", "igdrcl32.dll"),
    ("VulkanDriverName", "igvk64.json"),
    ("VulkanDriverNameWow", "igvk32.json"),
    ("LevelZeroDriverPath", "ze_intel_gpu64.dll"),
];

/// What became of a driver name query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameAnswer {
    Given(&'static str),
    /// The 32-bit names for Direct3D aren't known yet, so a 32-bit process is refused.
    No32BitName,
}

/// Writes the Direct3D driver's path into `out`, keeping the guest's version dword.
pub fn write_name(query: &VendorQuery<'_>, out: &mut Vec<u8>) -> (Status, NameAnswer) {
    if query.process_is_32_bit {
        return (Status::NOT_SUPPORTED, NameAnswer::No32BitName);
    }
    let version = dword(query.data, VERSION_FIELD).map_or(NEWEST, |version| (version as usize).min(NEWEST));
    let name = USER_DRIVERS[version];
    guest_text::user_driver_name(query, &format!("{DRIVER_STORE}\\{name}"), out);
    (Status::SUCCESS, NameAnswer::Given(name))
}

/// The driver paths, DXCore's attributes, and the loaders' values. The loaders' values go back as
/// a list of one path with no type to match, as which type each loader asks with isn't known yet:
/// read as a single string, the list's second zero is never reached.
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
        RegistryName::Value(name) => {
            let (_, file) = VALUES.iter().find(|(known, _)| known.eq_ignore_ascii_case(name))?;
            Some(RegistryValue { value_type: None, bytes: utf16_list(&[format!("{DRIVER_STORE}\\{file}")]) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> String {
        let units = bytes.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit));
        String::from_utf16_lossy(&units.take_while(|&unit| unit != 0).collect::<Vec<_>>())
    }

    fn name_for(version: u8, process_is_32_bit: bool) -> (Status, NameAnswer, Vec<u8>) {
        let data = [version, 0, 0, 0, 0, 0, 0, 0];
        let query = VendorQuery {
            kind: guest_text::QUERY_USER_DRIVER_NAME,
            size: data.len(),
            data: &data,
            process_is_32_bit,
        };
        let mut out = Vec::new();
        let (status, answer) = write_name(&query, &mut out);
        (status, answer, out)
    }

    #[test]
    fn each_direct3d_version_gets_the_driver_the_inf_lists_for_it() {
        for (version, file) in [(0, "igd9trinity64.dll"), (2, "igd10iumd64.dll"), (3, "igd12umd64.dll")] {
            let (status, answer, out) = name_for(version, false);
            assert_eq!((status, answer), (Status::SUCCESS, NameAnswer::Given(file)));
            assert_eq!(out[0], version, "the version dword is kept");
            assert_eq!(text(&out[4..]), format!("{DRIVER_STORE}\\{file}"));
        }
        assert_eq!(name_for(9, false).1, NameAnswer::Given("igd12umd64.dll"), "past the list is the newest");
    }

    #[test]
    fn a_32_bit_process_is_refused_until_its_names_are_known() {
        let (status, answer, out) = name_for(3, true);
        assert_eq!((status, answer), (Status::NOT_SUPPORTED, NameAnswer::No32BitName));
        assert!(out.is_empty());
    }

    #[test]
    fn the_loaders_values_are_paths_in_the_store_by_any_case() {
        let value = registry(&RegistryName::Value("vulkandrivername".into())).unwrap();
        assert_eq!(value.value_type, None);
        assert_eq!(text(&value.bytes), format!("{DRIVER_STORE}\\igvk64.json"));
        assert!(value.bytes.ends_with(&[0, 0, 0, 0]), "a list of one, closed twice");
        assert!(registry(&RegistryName::Value("LEVELZERODRIVERPATH".into())).is_some());
        assert!(registry(&RegistryName::Value("openglvendorname".into())).is_none());
    }

    #[test]
    fn the_store_the_kernel_driver_and_dxcore_are_answered() {
        assert_eq!(registry(&RegistryName::DriverStorePath).unwrap().bytes, utf16_zero(DRIVER_STORE));
        let image = registry(&RegistryName::DriverImagePath).unwrap();
        assert!(text(&image.bytes).ends_with(KERNEL_DRIVER));
        let dxcore = registry(&RegistryName::Value("dxcoreattributes".into())).unwrap();
        assert_eq!(dxcore.value_type, Some(TYPE_STRING_LIST));
    }
}
