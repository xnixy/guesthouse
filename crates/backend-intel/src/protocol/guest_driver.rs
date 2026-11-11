//! Where the guest finds Intel's user-mode drivers, and the registry values their loaders ask the
//! adapter for.
//!
//! The paravirtual adapter's kernel driver is Windows' own, and the user-mode drivers are Intel's.
//! The guest asks for a Direct3D driver's path with adapter query type 1, OpenGL's with type 2, and
//! the other loaders ask the adapter for registry values by name. The file names are the ones Intel's INF gives this
//! card. The files have to be in the guest, under the host driver store, at [`DRIVER_STORE`].
//! Nothing here copies them.

use super::bytes::dword;
use guesthouse_backend::guest_text::{self, utf16_list, utf16_zero};
use guesthouse_backend::{RegistryName, RegistryValue, Status, VendorQuery};
use guesthouse_messages::registry::{TYPE_NUMBER, TYPE_STRING, TYPE_STRING_LIST};

/// The release of Intel's driver these answers are for. The adapter record and the private data
/// change with the release, so any other is refused at start.
pub const GUEST_RELEASES: [&str; 1] = ["32.0.101.9175"];

/// Where the guest is told the driver lives. It rewrites `DriverStore` to its host driver store.
pub const DRIVER_STORE: &str = "C:\\Windows\\System32\\DriverStore\\FileRepository\\iigd_dch.inf_amd64_gpupv";

/// The user-mode drivers by the Direct3D version the guest asks for, as the INF lists them in
/// UserModeDriverName: Direct3D 9, 10, 11 and 12. 10 and 11 share a driver.
const USER_DRIVERS: [&str; 4] = ["igd9trinity64.dll", "igd10iumd64.dll", "igd10iumd64.dll", "igd12umd64.dll"];
/// The same for a 32-bit process, from UserModeDriverNameWow.
const USER_DRIVERS_32_BIT: [&str; 4] =
    ["igd9trinity32.dll", "igd10iumd32.dll", "igd10iumd32.dll", "igd12umd32.dll"];
/// Where the version sits in the query, KMTUMDVERSION: 0 for Direct3D 9 up to 3 for Direct3D 12.
const VERSION_FIELD: usize = 0;
const NEWEST: usize = USER_DRIVERS.len() - 1;
/// Direct3D 12's version in that field. Every version below it is Direct3D 9, 10 or 11.
const DIRECT3D_12: usize = 3;
const KERNEL_DRIVER: &str = "igdkmdn64.sys";

/// The adapter query OpenGL's loader asks for its driver with.
pub const QUERY_OPENGL: u32 = 2;
/// The answer's path field: MAX_PATH UTF-16 units. The version and flags follow it.
const OPENGL_NAME_BYTES: usize = 520;
const OPENGL_DRIVER: &str = "igxe2lpgicd64.dll";
const OPENGL_DRIVER_32_BIT: &str = "igxe2lpgicd32.dll";
/// What the INF gives for the OpenGL driver's version, flags and installed marker. The 32-bit
/// driver has the same.
const OPENGL_VERSION: u32 = 1;
const OPENGL_FLAGS: u32 = 3;
const OPENGL_INSTALLED: u32 = 1;
/// The one OpenGL value that has no 32-bit twin.
const OPENGL_INSTALLED_VALUE: &str = "OpenGLInstalled";

/// A value as the INF writes it, with file names in the driver store.
enum Form {
    File(&'static str),
    Files(&'static [&'static str]),
    Number(u32),
}

/// The values the loaders ask for, by the names the INF gives them. Registry names ignore case.
const VALUES: [(&str, Form); 11] = [
    ("OpenCLDriverName", Form::File("igdrcl64.dll")),
    ("OpenCLDriverNameWow", Form::File("igdrcl32.dll")),
    ("VulkanDriverName", Form::File("igvk64.json")),
    ("VulkanDriverNameWow", Form::File("igvk32.json")),
    ("LevelZeroDriverPath", Form::File("ze_intel_gpu64.dll")),
    ("OpenGLDriverName", Form::Files(&[OPENGL_DRIVER])),
    ("OpenGLDriverNameWow", Form::Files(&[OPENGL_DRIVER_32_BIT])),
    ("OpenGLVersion", Form::Number(OPENGL_VERSION)),
    ("OpenGLVersionWow", Form::Number(OPENGL_VERSION)),
    ("OpenGLFlags", Form::Number(OPENGL_FLAGS)),
    ("OpenGLFlagsWow", Form::Number(OPENGL_FLAGS)),
];

/// What became of a driver name query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameAnswer {
    Given(&'static str),
    /// Direct3D 9, 10 or 11, held back by the software desktop switch.
    Withheld,
}

/// Writes the Direct3D driver's path into `out`, keeping the guest's version dword. With
/// `only_direct3d_12`, the drivers for Direct3D 9, 10 and 11 are refused, so that those programs,
/// the desktop's compositor among them, fall back to drawing in software.
pub fn write_name(
    query: &VendorQuery<'_>,
    only_direct3d_12: bool,
    out: &mut Vec<u8>,
) -> (Status, NameAnswer) {
    let version = dword(query.data, VERSION_FIELD).map_or(NEWEST, |version| (version as usize).min(NEWEST));
    if only_direct3d_12 && version < DIRECT3D_12 {
        return (Status::NOT_SUPPORTED, NameAnswer::Withheld);
    }
    let drivers = if query.process_is_32_bit { USER_DRIVERS_32_BIT } else { USER_DRIVERS };
    let name = drivers[version];
    guest_text::user_driver_name(query, &format!("{DRIVER_STORE}\\{name}"), out);
    (Status::SUCCESS, NameAnswer::Given(name))
}

fn in_store(file: &str) -> String {
    format!("{DRIVER_STORE}\\{file}")
}

/// Answers OpenGL's query: the driver's path, then its version and flags. Gives the driver's name.
pub fn write_opengl_info(query: &VendorQuery<'_>, out: &mut Vec<u8>) -> &'static str {
    let driver = if query.process_is_32_bit { OPENGL_DRIVER_32_BIT } else { OPENGL_DRIVER };
    let start = out.len();
    let mut path = utf16_zero(&in_store(driver));
    path.truncate(OPENGL_NAME_BYTES);
    out.extend_from_slice(&path);
    out.resize(start + OPENGL_NAME_BYTES, 0);
    out.extend_from_slice(&OPENGL_VERSION.to_le_bytes());
    out.extend_from_slice(&OPENGL_FLAGS.to_le_bytes());
    driver
}

/// The driver paths, DXCore's attributes, and the loaders' values, each of the type the INF gives it.
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
        RegistryName::Value(name) if name.eq_ignore_ascii_case(OPENGL_INSTALLED_VALUE) => {
            Some(RegistryValue {
                value_type: Some(TYPE_NUMBER),
                bytes: OPENGL_INSTALLED.to_le_bytes().to_vec(),
            })
        }
        RegistryName::Value(name) => {
            let (_, form) = VALUES.iter().find(|(known, _)| known.eq_ignore_ascii_case(name))?;
            let (value_type, bytes) = match form {
                Form::File(file) => (TYPE_STRING, utf16_zero(&in_store(file))),
                Form::Files(files) => {
                    let paths: Vec<String> = files.iter().map(|file| in_store(file)).collect();
                    (TYPE_STRING_LIST, utf16_list(&paths))
                }
                Form::Number(number) => (TYPE_NUMBER, number.to_le_bytes().to_vec()),
            };
            Some(RegistryValue { value_type: Some(value_type), bytes })
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
        let (status, answer) = write_name(&query, false, &mut out);
        (status, answer, out)
    }

    #[test]
    fn the_software_desktop_switch_holds_back_all_but_direct3d_12() {
        let mut out = Vec::new();
        for (version, wanted) in
            [(0u8, NameAnswer::Withheld), (2, NameAnswer::Withheld), (3, NameAnswer::Given("igd12umd64.dll"))]
        {
            let data = [version, 0, 0, 0];
            let query = VendorQuery {
                kind: guest_text::QUERY_USER_DRIVER_NAME,
                size: 4,
                data: &data,
                process_is_32_bit: false,
            };
            assert_eq!(write_name(&query, true, &mut out).1, wanted, "{version}");
        }
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
    fn a_32_bit_process_gets_the_32_bit_drivers_the_inf_lists() {
        let wanted =
            [(0, "igd9trinity32.dll"), (1, "igd10iumd32.dll"), (2, "igd10iumd32.dll"), (3, "igd12umd32.dll")];
        for (version, file) in wanted {
            let (status, answer, out) = name_for(version, true);
            assert_eq!((status, answer), (Status::SUCCESS, NameAnswer::Given(file)));
            assert_eq!(text(&out[4..]), format!("{DRIVER_STORE}\\{file}"));
        }
    }

    fn opengl_answer(process_is_32_bit: bool) -> (&'static str, Vec<u8>) {
        let query = VendorQuery { kind: QUERY_OPENGL, size: 528, data: &[], process_is_32_bit };
        let mut out = Vec::new();
        (write_opengl_info(&query, &mut out), out)
    }

    #[test]
    fn the_opengl_answer_is_the_path_then_the_version_and_flags() {
        let (driver, out) = opengl_answer(false);
        assert_eq!((driver, out.len()), (OPENGL_DRIVER, OPENGL_NAME_BYTES + 8));
        assert_eq!(text(&out), format!("{DRIVER_STORE}\\{OPENGL_DRIVER}"));
        assert_eq!(out[OPENGL_NAME_BYTES..], [1, 0, 0, 0, 3, 0, 0, 0]);
        let (driver, out) = opengl_answer(true);
        assert_eq!(driver, OPENGL_DRIVER_32_BIT);
        assert_eq!(text(&out), format!("{DRIVER_STORE}\\{OPENGL_DRIVER_32_BIT}"));
    }

    #[test]
    fn the_opengl_registry_values_are_the_infs_with_their_types() {
        let driver = registry(&RegistryName::Value("opengldrivernamewow".into())).unwrap();
        assert_eq!(driver.value_type, Some(TYPE_STRING_LIST));
        assert_eq!(driver.bytes, utf16_list(&[in_store(OPENGL_DRIVER_32_BIT)]));
        let numbers = [
            ("OpenGLVersion", 1u32),
            ("OpenGLVersionWow", 1),
            ("OpenGLFlags", 3),
            ("OpenGLFlagsWow", 3),
            ("OpenGLInstalled", 1),
        ];
        for (name, number) in numbers {
            let value = registry(&RegistryName::Value(name.into())).unwrap();
            assert_eq!(value.value_type, Some(TYPE_NUMBER), "{name}");
            assert_eq!(value.bytes, number.to_le_bytes(), "{name}");
        }
    }

    #[test]
    fn the_loaders_values_are_paths_in_the_store_by_any_case() {
        let value = registry(&RegistryName::Value("vulkandrivername".into())).unwrap();
        assert_eq!(value.value_type, Some(TYPE_STRING), "the INF gives a single string");
        assert_eq!(value.bytes, utf16_zero(&format!("{DRIVER_STORE}\\igvk64.json")));
        for name in ["OpenCLDriverName", "OpenCLDriverNameWow", "VulkanDriverNameWow", "LEVELZERODRIVERPATH"]
        {
            let value = registry(&RegistryName::Value(name.into())).unwrap();
            assert_eq!(value.value_type, Some(TYPE_STRING), "{name}");
        }
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
