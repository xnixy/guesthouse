//! What Intel's INF says a guest on one card needs. Intel's driver is one INF for every card it
//! supports, so the files a card gets are found the way Windows would find them: the install section
//! its hardware id names, then the file lists that section copies, and from those the ones that go
//! into the driver's own folder, which is the folder the guest's driver store copy is made from.

use crate::setup::guest_files::GuestFile;
use std::collections::{BTreeSet, HashMap};

/// A UTF-16 file starts with this. Older INFs are plain 8-bit text.
const UTF16_MARK: [u8; 2] = [0xFF, 0xFE];
/// The directory number for the driver's own folder in the driver store. The other numbers are
/// Windows' system folders, which a guest's loaders find their way to by other means.
const DRIVER_FOLDER: &str = "13";
/// Windows looks for an install section with its platform's suffix first.
const PLATFORM_SUFFIX: &str = ".ntamd64";

/// File lists a guest has no use for: the kernel driver, which the adapter the host gives it has
/// no need of, the libraries for Linux under WSL, content protection, the media frameworks, Intel's
/// services for a desktop, and the compiler for its C-for-Metal kernels. A guest on a paravirtual
/// adapter draws through the user-mode drivers alone.
const NOT_FOR_THE_GUEST: [&str; 8] = [
    "igfx.Miniport_DS_N",
    "igfx.UserMode_WSL",
    "Opm.Copy_DS",
    "MFX_Common.Copy_DS",
    "OneVPL.Copy_DS",
    "DisplayService.Copy_DS",
    "IGSDS.Copy",
    "CMC.Copy",
];

/// Lists whose files belong to Intel's own tools, its control panel and its media software, and
/// go to Windows' folders besides: their names start with these.
const NOT_LOADERS: [&str; 3] = ["ControlApi_", "MSDK_", "IntelAPI."];

/// The text of an INF, which Intel's are in UTF-16.
pub fn decode(data: &[u8]) -> String {
    match data.strip_prefix(&UTF16_MARK) {
        Some(text) => {
            let units: Vec<u16> =
                text.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
            String::from_utf16_lossy(&units)
        }
        None => super::inf::latin1(data),
    }
}

/// `;` starts a comment, unless it's inside quotes.
fn without_comment(line: &str) -> &str {
    let mut quoted = false;
    for (at, letter) in line.char_indices() {
        match letter {
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..at],
            _ => {}
        }
    }
    line
}

/// The INF's sections by lower-case name, each with its lines, comments and blanks gone.
fn sections(inf: &str) -> HashMap<String, Vec<&str>> {
    let mut found: HashMap<String, Vec<&str>> = HashMap::new();
    let mut current = None;
    for line in inf.lines().map(|line| without_comment(line).trim()).filter(|line| !line.is_empty()) {
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            let key = name.trim().to_lowercase();
            found.entry(key.clone()).or_default();
            current = Some(key);
        } else if let Some(lines) = current.as_ref().and_then(|key| found.get_mut(key)) {
            lines.push(line);
        }
    }
    found
}

fn fields(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(|field| field.trim().trim_matches('"')).filter(|field| !field.is_empty())
}

/// The install sections the INF gives this device, whatever its subsystem. A device is listed in a
/// section for each Windows release that has its own, and they copy the same files.
fn install_sections<'a>(sections: &HashMap<String, Vec<&'a str>>, device: u32) -> Vec<&'a str> {
    let wanted = format!("PCI\\VEN_8086&DEV_{device:04X}");
    let mut found: Vec<&str> = Vec::new();
    for line in sections.values().flatten() {
        let Some((_, value)) = line.split_once('=') else { continue };
        let mut parts = fields(value);
        let Some(section) = parts.next() else { continue };
        let lists_device = parts.any(|id| {
            let id = id.to_uppercase();
            id.strip_prefix(&wanted).is_some_and(|rest| rest.is_empty() || rest.starts_with('&'))
        });
        if lists_device && !found.iter().any(|known| known.eq_ignore_ascii_case(section)) {
            found.push(section);
        }
    }
    found
}

/// Whether the INF has a line for this Intel device.
pub fn lists_device(inf: &str, device: u32) -> bool {
    !install_sections(&sections(inf), device).is_empty()
}

/// Where a file list in `[DestinationDirs]` puts its files: the driver's folder, or not.
fn goes_to_driver_folder(sections: &HashMap<String, Vec<&str>>, list: &str) -> bool {
    let Some(directories) = sections.get("destinationdirs") else { return false };
    directories.iter().filter_map(|line| line.split_once('=')).any(|(name, place)| {
        name.trim().eq_ignore_ascii_case(list) && fields(place).collect::<Vec<_>>() == [DRIVER_FOLDER]
    })
}

/// The file lists the card's install sections copy, minus the ones a guest has no use for.
fn copy_lists<'a>(sections: &HashMap<String, Vec<&'a str>>, device: u32) -> Vec<&'a str> {
    let mut found = Vec::new();
    for install in install_sections(sections, device) {
        let decorated = format!("{}{PLATFORM_SUFFIX}", install.to_lowercase());
        let Some(lines) = sections.get(&decorated).or_else(|| sections.get(&install.to_lowercase())) else {
            continue;
        };
        let lists = lines
            .iter()
            .filter_map(|line| line.split_once('='))
            .filter(|(key, _)| key.trim().eq_ignore_ascii_case("CopyFiles"))
            .flat_map(|(_, value)| fields(value));
        found.extend(
            lists.filter(|list| !NOT_FOR_THE_GUEST.iter().any(|known| known.eq_ignore_ascii_case(list))),
        );
    }
    found
}

/// The names of the files a guest on this device gets in its driver folder, sorted.
pub fn guest_files(inf: &str, device: u32) -> Vec<String> {
    let sections = sections(inf);
    let mut names = BTreeSet::new();
    for list in copy_lists(&sections, device) {
        if goes_to_driver_folder(&sections, list) {
            let files = sections.get(&list.to_lowercase()).into_iter().flatten();
            names.extend(files.filter_map(|line| fields(line).next()).map(str::to_owned));
        }
    }
    let mut seen = BTreeSet::new();
    names.into_iter().filter(|name| seen.insert(name.to_lowercase())).collect()
}

/// Where a file list puts its files in Windows' own folders: System32 for directory 11, and SysWOW64
/// for `10,SysWow64`. Anything else is the driver's folder or a place a guest has no use for.
fn windows_folder(sections: &HashMap<String, Vec<&str>>, list: &str) -> Option<&'static str> {
    let directories = sections.get("destinationdirs")?;
    let mut lines = directories.iter().filter_map(|line| line.split_once('='));
    let (_, place) = lines.find(|(name, _)| name.trim().eq_ignore_ascii_case(list))?;
    match fields(place).collect::<Vec<_>>().as_slice() {
        ["11"] => Some("System32"),
        ["10", folder] if folder.eq_ignore_ascii_case("syswow64") => Some("SysWOW64"),
        _ => None,
    }
}

/// The loaders and runtime libraries the INF puts in Windows' system folders for this device: the
/// Vulkan, OpenCL and Level Zero loaders, and the 32-bit C++ runtime the 32-bit drivers load. Each is
/// the folder, the file in the driver package (`source`) and the name it gets there (`name`), which
/// differ where the INF renames. Intel's control library, its media libraries and its tools are left
/// out, as nothing a graphics program does loads them.
pub fn system_files(inf: &str, device: u32) -> Vec<GuestFile> {
    let sections = sections(inf);
    let mut found: Vec<GuestFile> = Vec::new();
    for list in copy_lists(&sections, device) {
        let skipped = NOT_LOADERS
            .iter()
            .any(|prefix| list.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix)));
        let Some(folder) = windows_folder(&sections, list).filter(|_| !skipped) else { continue };
        for line in sections.get(&list.to_lowercase()).into_iter().flatten() {
            // The INF's order is the name, then the file to copy if that differs, so empty fields count.
            let mut parts = line.split(',').map(|field| field.trim().trim_matches('"'));
            let Some(name) = parts.next().filter(|name| name.to_lowercase().ends_with(".dll")) else {
                continue;
            };
            let source = parts.next().filter(|source| !source.is_empty()).unwrap_or(name);
            let known =
                found.iter().any(|file| file.folder == folder && file.name.eq_ignore_ascii_case(name));
            if !known {
                found.push(GuestFile {
                    folder: folder.to_owned(),
                    source: source.to_owned(),
                    name: name.to_owned(),
                });
            }
        }
    }
    found
}

#[cfg(test)]
pub mod stand_in {
    pub const DEVICE: u32 = 0x64A0;
    pub const DRIVER_VERSION: &str = "32.0.101.9175";

    /// The user-mode files a guest on `DEVICE` gets from the INF below, as the INF lists them.
    pub const GUEST_FILES: [&str; 11] = [
        "igd10iumd64.dll",
        "igd12umd32.dll",
        "igd12umd64.dll",
        "igd9trinity64.dll",
        "igdrcl32.dll",
        "igdrcl64.dll",
        "igvk32.dll",
        "igvk32.json",
        "igvk64.dll",
        "igvk64.json",
        "ze_intel_gpu64.dll",
    ];

    /// What the INF below puts in Windows' folders, as folder, file in the package and name there.
    pub const SYSTEM_FILES: [(&str, &str, &str); 6] = [
        ("System32", "vulkan-1-64.dll", "vulkan-1.dll"),
        ("System32", "Intel_OpenCL_ICD64.dll", "OpenCL.dll"),
        ("SysWOW64", "msvcp140_32.dll", "msvcp140.dll"),
        ("SysWOW64", "own.dll", "own.dll"),
        ("SysWOW64", "vulkan-1-32.dll", "vulkan-1.dll"),
        ("SysWOW64", "Intel_OpenCL_ICD32.dll", "OpenCL.dll"),
    ];

    /// An INF in the shape of Intel's, as far as this reads it, with a few files where it has
    /// hundreds: one install section for the device, the kernel driver and Linux libraries it
    /// leaves out, and files that go to Windows' own folders.
    pub fn fake_inf() -> String {
        format!(
            "[Version]\n\
             DriverVer = 11/04/2026,{DRIVER_VERSION}\n\
             [Manufacturer]\n\
             %Intel% = Gfx, NTamd64.10.0...22000\n\
             [Gfx.NTamd64.10.0...22000]\n\
             ; a card of the same family\n\
             %CARD% = FAMILY_A, PCI\\VEN_8086&DEV_64A0\n\
             %CARD% = FAMILY_A, PCI\\VEN_8086&DEV_64A0&SUBSYS_146B1462\n\
             %OTHER% = FAMILY_B, PCI\\VEN_8086&DEV_64A01\n\
             %OTHER% = FAMILY_B, PCI\\VEN_8086&DEV_6420\n\
             %AMD% = FAMILY_C, PCI\\VEN_1002&DEV_64A0\n\
             [FAMILY_A]\n\
             CopyFiles=igfx.Miniport_DS_N, Direct3D, Direct3D32, Loaders, igfx.UserMode_WSL, Vulkan, Compute, Compute32, MFX_Common.Copy_DS, OpenCL.Copy64_Ext, Runtime_Wow, Loaders32, ControlApi_Gpu.Copy64_Ext, MSDK_w10rs1.copy32\n\
             [FAMILY_A.NTamd64]\n\
             CopyFiles=igfx.Miniport_DS_N, Direct3D, Direct3D32, Loaders, igfx.UserMode_WSL, Vulkan, Compute, Compute32, MFX_Common.Copy_DS, OpenCL.Copy64_Ext, Runtime_Wow, Loaders32, ControlApi_Gpu.Copy64_Ext, MSDK_w10rs1.copy32\n\
             [FAMILY_B]\n\
             CopyFiles=Other\n\
             [DestinationDirs]\n\
             igfx.Miniport_DS_N=13\n\
             Direct3D=13\n\
             Direct3D32=13\n\
             Loaders=11\n\
             Loaders32=10, SysWow64\n\
             OpenCL.Copy64_Ext=11\n\
             Runtime_Wow=10,SysWOW64\n\
             ControlApi_Gpu.Copy64_Ext=11\n\
             MSDK_w10rs1.copy32=10, SysWow64\n\
             igfx.UserMode_WSL=13\n\
             Vulkan=13\n\
             Compute=13\n\
             Compute32=13\n\
             MFX_Common.Copy_DS=13\n\
             Other=13\n\
             [igfx.Miniport_DS_N]\n\
             igdkmdn64.sys\n\
             [Direct3D]\n\
             igd12umd64.dll\n\
             igd10iumd64.dll\n\
             igd9trinity64.dll ; the oldest API\n\
             [Direct3D32]\n\
             igd12umd32.dll\n\
             [Loaders]\n\
             vulkan-1.dll, vulkan-1-64.dll,,0x4020\n\
             vulkaninfo.exe, vulkaninfo-64.exe,,0x4020\n\
             [OpenCL.Copy64_Ext]\n\
             OpenCL.dll,Intel_OpenCL_ICD64.dll,,0x4020\n\
             [Runtime_Wow]\n\
             msvcp140.dll,msvcp140_32.dll,,0x00004020 ; renamed on the way\n\
             own.dll,,,%COPYFLAG4000%\n\
             [Loaders32]\n\
             vulkan-1.dll,    vulkan-1-32.dll,,    0x00004020\n\
             vulkaninfo.exe, vulkaninfo-32.exe,,0x4020\n\
             OpenCL.dll,Intel_OpenCL_ICD32.dll,,0x4020\n\
             [ControlApi_Gpu.Copy64_Ext]\n\
             ControlLib.dll,,,0x4020\n\
             [MSDK_w10rs1.copy32]\n\
             libmfxhw32.dll,mfx_loader_dll_hw32.dll,,0x4020\n\
             [igfx.UserMode_WSL]\n\
             libigd12umd64.so\n\
             [Vulkan]\n\
             igvk64.json\n\
             igvk64.dll\n\
             igvk32.json\n\
             igvk32.dll\n\
             [Compute]\n\
             igdrcl64.dll\n\
             ze_intel_gpu64.dll\n\
             [Compute32]\n\
             igdrcl32.dll\n\
             [MFX_Common.Copy_DS]\n\
             mfxplugin64_hw.dll,,,0x4\n\
             [Other]\n\
             not-ours.dll\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::*;
    use super::*;

    #[test]
    fn the_device_is_found_by_its_id_in_any_subsystem_and_by_no_other_id() {
        let inf = fake_inf();
        assert!(lists_device(&inf, DEVICE));
        assert!(lists_device(&inf, 0x6420), "listed once, in the section of another family");
        assert!(!lists_device(&inf, 0x64A1), "an id that only starts the same way");
        assert!(!lists_device(&inf, 0x1234));
        assert!(!lists_device("", DEVICE));
    }

    #[test]
    fn a_guest_gets_the_drivers_own_folder_for_its_card_without_the_kernel_driver() {
        assert_eq!(guest_files(&fake_inf(), DEVICE), GUEST_FILES);
    }

    #[test]
    fn the_loaders_and_runtime_go_to_the_system_folders_under_the_names_the_inf_gives() {
        let found: Vec<(String, String, String)> = system_files(&fake_inf(), DEVICE)
            .into_iter()
            .map(|file| (file.folder, file.source, file.name))
            .collect();
        let wanted: Vec<(String, String, String)> = SYSTEM_FILES
            .iter()
            .map(|(folder, source, name)| ((*folder).to_owned(), (*source).to_owned(), (*name).to_owned()))
            .collect();
        assert_eq!(found, wanted, "no tools, control library or media libraries");
        assert!(system_files(&fake_inf(), 0x6420).is_empty());
        assert!(system_files(&fake_inf(), 0x1234).is_empty());
    }

    #[test]
    fn another_family_gets_its_own_files_and_a_card_the_inf_lacks_gets_none() {
        assert_eq!(guest_files(&fake_inf(), 0x6420), ["not-ours.dll"]);
        assert!(guest_files(&fake_inf(), 0x1234).is_empty());
    }

    #[test]
    fn a_utf16_inf_reads_the_same_as_its_text() {
        let text = fake_inf();
        let mut data = UTF16_MARK.to_vec();
        data.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode(&data), text);
        assert_eq!(decode(text.as_bytes()), text);
        assert_eq!(guest_files(&decode(&data), DEVICE), GUEST_FILES);
    }

    #[test]
    fn a_semicolon_in_quotes_is_not_a_comment() {
        assert_eq!(without_comment("a = \"b;c\" ; d"), "a = \"b;c\" ");
    }

    /// Intel's own INF, named in `GUESTHOUSE_INTEL_INF`, with the card in `GUESTHOUSE_INTEL_DEVICE` as
    /// hex. Run by hand, as the INF is Intel's and not ours. Prints the files, and holds them to the
    /// ones the Intel daemon names.
    #[test]
    #[ignore = "needs Intel's INF"]
    fn a_real_inf_gives_every_file_the_daemon_names() {
        let path = std::env::var("GUESTHOUSE_INTEL_INF").unwrap();
        let device = u32::from_str_radix(&std::env::var("GUESTHOUSE_INTEL_DEVICE").unwrap(), 16).unwrap();
        let inf = decode(&std::fs::read(path).unwrap());
        let files = guest_files(&inf, device);
        println!(
            "{}
{} files",
            files.join(
                "
"
            ),
            files.len()
        );
        for named in crate::setup::intel::NAMED_FILES {
            assert!(files.iter().any(|file| file == named), "{named} is not among them");
        }
        assert!(!files.iter().any(|file| file.ends_with(".sys")), "no kernel driver");
        let system = system_files(&inf, device);
        for file in &system {
            println!("{} {} from {}", file.folder, file.name, file.source);
        }
        let has = |folder: &str, name: &str| {
            system.iter().any(|file| file.folder == folder && file.name.eq_ignore_ascii_case(name))
        };
        for name in ["vulkan-1.dll", "OpenCL.dll", "msvcp140.dll"] {
            assert!(has("SysWOW64", name), "{name} is not in SysWOW64");
        }
        for name in ["vulkan-1.dll", "OpenCL.dll", "ze_loader.dll"] {
            assert!(has("System32", name), "{name} is not in System32");
        }
    }
}
