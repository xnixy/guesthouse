//! The files a card's driver puts outside the driver store, which a Windows guest needs as well.
//! GPU-PV lends the guest the driver's own folder in the host's driver store, but a driver also
//! installs files elsewhere: loaders a program finds by name, such as Vulkan's and OpenCL's, its
//! maker's media and compute libraries in System32 and SysWOW64, and on NVIDIA's the files Linux
//! guests and the driver's settings use. They're named in the driver's install file, the INF in its
//! folder, by where each group of files goes and which file of the folder each one is.
//!
//! A guest's own copy of a file is never replaced: one Guesthouse didn't put there is left as it
//! is. The files Guesthouse put there are listed in the guest, so a newer driver's can replace
//! them.

use crate::error::{Error, Result};
use crate::guest_volume::{Entry, GuestVolume};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The list of files Guesthouse put in a guest, from the root of its volume.
pub const PLACED_LIST: &str = r"Guesthouse\driver-files.txt";

/// Where NVIDIA's driver puts its NVAPI. Guesthouse's wrapper goes there in its place, when there is
/// one: NVIDIA's refuses the call DLSS needs on a paravirtualized card, and the wrapper loads it from
/// the driver's folder and answers that call.
const NVAPI_TARGET: &str = r"Windows\System32\nvapi64.dll";

/// A file of the driver's folder, and where in a guest it goes, from the root of its volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub source: PathBuf,
    pub target: PathBuf,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Placed {
    pub copied: usize,
    /// Already there, the same as the driver's.
    pub kept: usize,
    /// The guest's own, left as they are.
    pub left: Vec<PathBuf>,
}

/// The folder a destination directory number of an INF stands for, from the root of a Windows
/// volume: 10 is Windows, 11 System32, 12 its drivers folder, 16425 SysWOW64. 13, the driver
/// store, isn't one: those files are in the folder the guest is lent.
fn folder_of(dirid: u32) -> Option<&'static str> {
    match dirid {
        10 => Some("Windows"),
        11 => Some(r"Windows\System32"),
        12 => Some(r"Windows\System32\drivers"),
        16425 => Some(r"Windows\SysWOW64"),
        _ => None,
    }
}

/// An INF's text: UTF-16 when it starts with that byte order mark, as many drivers' are, else UTF-8
/// or what of it reads.
fn text_of(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// A line without its comment: everything from a `;` that isn't inside quotes.
fn without_comment(line: &str) -> &str {
    let mut quoted = false;
    for (at, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..at],
            _ => {}
        }
    }
    line
}

/// Each section of an INF, by its name in lower case, with its lines.
fn sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut all: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let line = without_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            all.push((name.trim().to_ascii_lowercase(), Vec::new()));
        } else if let Some((_, lines)) = all.last_mut() {
            lines.push(line.to_owned());
        }
    }
    all
}

/// The lines of every section of this name, as an INF may repeat one.
fn lines_of<'a>(all: &'a [(String, Vec<String>)], name: &str) -> Vec<&'a String> {
    all.iter().filter(|(section, _)| section == name).flat_map(|(_, lines)| lines).collect()
}

fn unquoted(field: &str) -> &str {
    field.trim().trim_matches('"').trim()
}

/// Where the files an INF sends outside the driver store go, in a guest, from the root of its
/// volume, with the name each has in the driver's folder. A file whose name or place is written
/// with a string the INF defines elsewhere isn't followed.
pub fn outside_files(inf: &str) -> Vec<(PathBuf, String)> {
    let all = sections(inf);
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for line in lines_of(&all, "destinationdirs") {
        let Some((section, place)) = line.split_once('=') else { continue };
        let section = unquoted(section).to_ascii_lowercase();
        if section == "defaultdestdir" {
            continue;
        }
        let mut place = place.split(',');
        let Some(folder) = place.next().and_then(|id| unquoted(id).parse().ok()).and_then(folder_of) else {
            continue;
        };
        let mut folder = PathBuf::from(folder);
        if let Some(sub) = place.next().map(unquoted).filter(|sub| !sub.is_empty()) {
            if sub.contains('%') || sub.contains("..") {
                continue;
            }
            folder.push(sub);
        }
        for entry in lines_of(&all, &section) {
            let mut fields = entry.split(',');
            let target = unquoted(fields.next().unwrap_or(""));
            let source = fields.next().map(unquoted).filter(|source| !source.is_empty()).unwrap_or(target);
            let plain = |name: &str| {
                !name.is_empty() && !name.contains(['%', '\\', '/', ':']) && !name.contains("..")
            };
            if !plain(target) || !plain(source) {
                continue;
            }
            let goes = folder.join(target);
            if seen.insert(goes.to_string_lossy().to_ascii_lowercase()) {
                found.push((goes, source.to_owned()));
            }
        }
    }
    found
}

/// The driver folder's install file: the folder is named for it, `NAME.inf_amd64_HASH`.
#[allow(clippy::disallowed_methods)] // the host's own files
fn inf_in(driver_folder: &Path) -> Option<PathBuf> {
    let name = driver_folder.file_name()?.to_str()?;
    let at = name.to_ascii_lowercase().find(".inf_")?;
    let inf = driver_folder.join(&name[..at + 4]);
    inf.is_file().then_some(inf)
}

/// Finds a file of the driver's folder by name, in the folder or below it, whatever its case.
#[allow(clippy::disallowed_methods)] // the host's own files
fn find_in(folder: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind)
                    if kind.is_file() && entry.file_name().to_string_lossy().eq_ignore_ascii_case(name) =>
                {
                    return Some(path);
                }
                _ => {}
            }
        }
    }
    None
}

/// What of `driver_folder`'s install file goes outside the driver store, as files of the folder and
/// where each goes in a guest. Files the folder doesn't have are left out.
#[allow(clippy::disallowed_methods)] // the host's own files
pub fn placements(driver_folder: &Path) -> Result<Vec<Placement>> {
    let Some(inf) = inf_in(driver_folder) else { return Ok(Vec::new()) };
    let bytes = fs::read(&inf).map_err(|error| Error::io(format!("Reading {}", inf.display()), error))?;
    Ok(outside_files(&text_of(&bytes))
        .into_iter()
        .filter_map(|(target, source)| Some(Placement { source: find_in(driver_folder, &source)?, target }))
        .collect())
}

/// The most of the list of placed files that is read: far more than any driver's files.
const MOST_LIST_BYTES: u64 = 1024 * 1024;

/// Puts the files of `driver_folder` that its driver installs outside the driver store on the
/// guest's Windows volume, where the driver puts them, leaving the guest's own files alone: a file
/// Guesthouse didn't put there, and a link or folder where one would go, whatever the list says.
/// `nvapi_wrapper` is the NVAPI wrapper's file, which goes where NVIDIA's NVAPI would.
#[allow(clippy::disallowed_methods)] // the host's own files
pub fn place(driver_folder: &Path, guest: &GuestVolume, nvapi_wrapper: Option<&Path>) -> Result<Placed> {
    let list = guest.read(PLACED_LIST, MOST_LIST_BYTES).unwrap_or_default();
    let mut ours: BTreeSet<String> = String::from_utf8_lossy(&list)
        .lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .filter(|line| !line.is_empty())
        .collect();
    let mut placed = Placed::default();
    let fail = |what: &Path, error| Error::io(format!("Placing {}", guest.shown(what)), error);
    for mut placement in placements(driver_folder)? {
        if let Some(wrapper) = nvapi_wrapper
            && placement.target.to_string_lossy().eq_ignore_ascii_case(NVAPI_TARGET)
        {
            placement.source = wrapper.to_owned();
        }
        let key = placement.target.to_string_lossy().to_ascii_lowercase();
        // What can't be looked at, as a folder on the way is a link, is the guest's as well.
        match guest.entry(&placement.target) {
            Ok(None) => {}
            Ok(Some(Entry::File { len, .. })) if ours.contains(&key) => {
                let source = fs::read(&placement.source);
                if let Ok(source) = source
                    && source.len() as u64 == len
                    && guest.read(&placement.target, len + 1).is_ok_and(|there| there == source)
                {
                    placed.kept += 1;
                    continue;
                }
            }
            Ok(Some(_)) | Err(_) => {
                placed.left.push(placement.target);
                continue;
            }
        }
        guest
            .copy_in(&placement.source, &placement.target)
            .map_err(|error| fail(&placement.target, error))?;
        ours.insert(key);
        placed.copied += 1;
    }
    if placed.copied > 0 {
        let text: String = ours.iter().map(|line| format!("{line}\r\n")).collect();
        guest.write(PLACED_LIST, text).map_err(|error| fail(Path::new(PLACED_LIST), error))?;
    }
    Ok(placed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest_volume::tests::junction;

    /// Parts of Intel's Arc driver's install file, as a tester's showed them.
    const INTEL: &str = "\
[Version]
Signature = \"$WINDOWS NT$\"
[DestinationDirs]
DefaultDestDir = 13
MSVCP140_VCRUNTIME140_Wow = 10,SysWOW64
Vulkan.CopyRT = 11
OpenCL_Gpu.Copy64_Ext = 11 ; the loader
MSDK_w10rs1.copy64 = 11
Kept.Here = 13
Elsewhere = 16422
[MSVCP140_VCRUNTIME140_Wow]
msvcp140.dll,msvcp140_32.dll,,0x00004020
[Vulkan.CopyRT]
vulkan-1.dll,    vulkan-1-64.dll,,    0x00004020
vulkaninfo.exe,    vulkaninfo-64.exe,,    0x00004020
[OpenCL_Gpu.Copy64_Ext]
OpenCL.dll,Intel_OpenCL_ICD64.dll,,0x00004020
[MSDK_w10rs1.copy64]
mfxplugin64_hw.dll,,,%COPYFLAG4000%
%NAME%.dll
[Kept.Here]
igd12umd64.dll
[Elsewhere]
tool.exe
";

    #[test]
    fn the_files_outside_the_driver_store_and_their_places() {
        let found = outside_files(INTEL);
        let found: Vec<(String, &str)> = found
            .iter()
            .map(|(target, source)| (target.to_string_lossy().into_owned(), source.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (r"Windows\SysWOW64\msvcp140.dll".to_owned(), "msvcp140_32.dll"),
                (r"Windows\System32\vulkan-1.dll".to_owned(), "vulkan-1-64.dll"),
                (r"Windows\System32\vulkaninfo.exe".to_owned(), "vulkaninfo-64.exe"),
                (r"Windows\System32\OpenCL.dll".to_owned(), "Intel_OpenCL_ICD64.dll"),
                (r"Windows\System32\mfxplugin64_hw.dll".to_owned(), "mfxplugin64_hw.dll"),
            ]
        );
    }

    #[test]
    fn utf16_install_files_read_too() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            "[DestinationDirs]\r\nA = 11\r\n[A]\r\nx.dll\r\n".encode_utf16().flat_map(u16::to_le_bytes),
        );
        let found = outside_files(&text_of(&bytes));
        assert_eq!(found, [(PathBuf::from(r"Windows\System32\x.dll"), "x.dll".to_owned())]);
    }

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("guesthouse-driver-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_guests_own_file_is_left_and_ours_are_updated() {
        let host = scratch("host");
        let folder = host.join("card.inf_amd64_0123456789abcdef");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(
            folder.join("card.inf"),
            "[DestinationDirs]\nA = 11\nB = 10,SysWOW64\n[A]\nloader.dll,loader-64.dll\n[B]\nvcruntime140.dll,vc32.dll\n",
        )
        .unwrap();
        fs::write(folder.join("sub").join("loader-64.dll"), b"loader one").unwrap();
        fs::write(folder.join("vc32.dll"), b"driver's runtime").unwrap();
        let guest_root = scratch("guest");
        fs::create_dir_all(guest_root.join(r"Windows\SysWOW64")).unwrap();
        fs::write(guest_root.join(r"Windows\SysWOW64\vcruntime140.dll"), b"windows' own runtime").unwrap();
        let guest = GuestVolume::open(&guest_root).unwrap();

        let placed = place(&folder, &guest, None).unwrap();
        assert_eq!(placed.copied, 1);
        assert_eq!(placed.left, [PathBuf::from(r"Windows\SysWOW64\vcruntime140.dll")]);
        assert_eq!(fs::read(guest_root.join(r"Windows\System32\loader.dll")).unwrap(), b"loader one");
        assert_eq!(
            fs::read(guest_root.join(r"Windows\SysWOW64\vcruntime140.dll")).unwrap(),
            b"windows' own runtime"
        );

        assert_eq!(place(&folder, &guest, None).unwrap().kept, 1);
        fs::write(folder.join("sub").join("loader-64.dll"), b"loader two").unwrap();
        assert_eq!(place(&folder, &guest, None).unwrap().copied, 1, "a newer driver's file replaces ours");
        assert_eq!(fs::read(guest_root.join(r"Windows\System32\loader.dll")).unwrap(), b"loader two");

        let _ = fs::remove_dir_all(host);
        drop(guest);
        let _ = fs::remove_dir_all(guest_root);
    }

    #[test]
    fn the_nvapi_wrapper_goes_where_nvidias_nvapi_would() {
        let host = scratch("host-nvapi");
        let folder = host.join("nv_dispi.inf_amd64_0123456789abcdef");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("nv_dispi.inf"),
            "[DestinationDirs]\nA = 11\nB = 10,SysWOW64\n[A]\nnvapi64.dll\n[B]\nnvapi.dll\n",
        )
        .unwrap();
        fs::write(folder.join("nvapi64.dll"), b"nvidia's 64").unwrap();
        fs::write(folder.join("nvapi.dll"), b"nvidia's 32").unwrap();
        let wrapper = host.join("nvapi64-wrapper.dll");
        fs::write(&wrapper, b"wrapper").unwrap();
        let guest_root = scratch("guest-nvapi");
        let guest = GuestVolume::open(&guest_root).unwrap();

        assert_eq!(place(&folder, &guest, None).unwrap().copied, 2);
        assert_eq!(fs::read(guest_root.join(NVAPI_TARGET)).unwrap(), b"nvidia's 64");
        assert_eq!(place(&folder, &guest, Some(&wrapper)).unwrap().copied, 1, "ours, so it's replaced");
        assert_eq!(fs::read(guest_root.join(NVAPI_TARGET)).unwrap(), b"wrapper");
        assert_eq!(fs::read(guest_root.join(r"Windows\SysWOW64\nvapi.dll")).unwrap(), b"nvidia's 32");
        assert_eq!(place(&folder, &guest, Some(&wrapper)).unwrap().kept, 2);

        let _ = fs::remove_dir_all(host);
        drop(guest);
        let _ = fs::remove_dir_all(guest_root);
    }

    #[test]
    fn a_link_where_a_file_goes_is_left_even_when_the_list_names_it() {
        let host = scratch("host-links");
        let folder = host.join("card.inf_amd64_0123456789abcdef");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("card.inf"),
            "[DestinationDirs]\nA = 11\nB = 10,SysWOW64\n[A]\nloader.dll\n[B]\nruntime.dll\n",
        )
        .unwrap();
        fs::write(folder.join("loader.dll"), b"driver's loader").unwrap();
        fs::write(folder.join("runtime.dll"), b"driver's runtime").unwrap();
        let elsewhere = host.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let hosts_own = host.join("hosts-own.dll");
        fs::write(&hosts_own, b"the host's own").unwrap();
        let guest_root = scratch("guest-links");
        fs::create_dir_all(guest_root.join(r"Windows\System32")).unwrap();
        fs::create_dir_all(guest_root.join("Guesthouse")).unwrap();
        // The guest's list says both are Guesthouse's, so they'd be written over.
        fs::write(
            guest_root.join(PLACED_LIST),
            "windows\\system32\\loader.dll\r\nwindows\\syswow64\\runtime.dll\r\n",
        )
        .unwrap();
        std::os::windows::fs::symlink_file(&hosts_own, guest_root.join(r"Windows\System32\loader.dll"))
            .unwrap();
        junction(&guest_root.join(r"Windows\SysWOW64"), &elsewhere);
        let guest = GuestVolume::open(&guest_root).unwrap();

        let placed = place(&folder, &guest, None);
        assert_eq!(
            fs::read(&hosts_own).unwrap(),
            b"the host's own",
            "nothing was written through the file link"
        );
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing was written through the junction");
        let placed = placed.unwrap();
        assert_eq!(placed.copied, 0);
        assert_eq!(
            placed.left,
            [PathBuf::from(r"Windows\System32\loader.dll"), PathBuf::from(r"Windows\SysWOW64\runtime.dll")]
        );

        drop(guest);
        let _ = fs::remove_dir_all(guest_root);
        let _ = fs::remove_dir_all(host);
    }

    /// What this PC's own cards' drivers would place, printed: `cargo test -p guesthouse-hcs
    /// placements_of_this_pcs_cards -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn placements_of_this_pcs_cards() {
        for card in crate::gpu::cards().unwrap() {
            let Some(folder) = card.driver_folder else { continue };
            let found = placements(&folder).unwrap();
            println!("{}: {} files from {}", card.name, found.len(), folder.display());
            for placement in found {
                println!("  {} <- {}", placement.target.display(), placement.source.display());
            }
        }
    }
}
