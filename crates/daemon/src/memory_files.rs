//! The guest's memory as two files named in the environment, for a QEMU that Guesthouse didn't start.
//! The app hands the daemon memfds instead: see [`crate::handover`]. Here QEMU opens the same two paths
//! with `memory-backend-file,share=on`, and the daemon makes them first.
//!
//! All of it happens before the jail goes on. The daemon holds both files open by then, and mapping a
//! file that's open takes no grant, so the jail names neither path.

use crate::guest_memory::WINDOW_SIZE;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The guest's RAM, a file of `GUESTHOUSE_RAM`'s size.
pub const RAM_VARIABLE: &str = "GUESTHOUSE_RAM_FILE";
/// The window, a file of [`WINDOW_SIZE`].
pub const WINDOW_VARIABLE: &str = "GUESTHOUSE_WINDOW_FILE";

const OWNER_ONLY: u32 = 0o600;
const GROUP_AND_OTHERS: u32 = 0o077;
const GIB: u64 = 1 << 30;
const NOT_A_FILE: &str = "it is not a regular file.";

/// Where the guest's two parts are, if the environment says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFiles {
    pub ram: PathBuf,
    pub window: PathBuf,
}

impl MemoryFiles {
    /// `None` if neither variable is set, which leaves the memory to the app.
    pub fn from_environment() -> Result<Option<Self>, String> {
        Self::parse(env::var_os(RAM_VARIABLE), env::var_os(WINDOW_VARIABLE))
    }

    /// Both variables or neither. A variable that's empty is not set.
    pub fn parse(ram: Option<OsString>, window: Option<OsString>) -> Result<Option<Self>, String> {
        let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
        let (ram, window) = match (set(ram), set(window)) {
            (None, None) => return Ok(None),
            (Some(ram), Some(window)) => (PathBuf::from(ram), PathBuf::from(window)),
            (Some(_), None) => return Err(one_alone(RAM_VARIABLE, WINDOW_VARIABLE)),
            (None, Some(_)) => return Err(one_alone(WINDOW_VARIABLE, RAM_VARIABLE)),
        };
        for (variable, path) in [(RAM_VARIABLE, &ram), (WINDOW_VARIABLE, &window)] {
            if !path.is_absolute() {
                return Err(format!("{variable} must be a full path, and {} is not one.", path.display()));
            }
        }
        if ram == window {
            return Err(format!("{RAM_VARIABLE} and {WINDOW_VARIABLE} name the same file."));
        }
        Ok(Some(Self { ram, window }))
    }

    /// Makes and checks both, and gives the window and then the RAM.
    ///
    /// A RAM file made here is claimed in full, as the app claims a memfd, and for the same reason:
    /// RAM fills as the guest touches it, and by the time a PC with too little would show it, the GPU
    /// has pinned what it has and nothing can be swapped to make room. A start either has the memory
    /// or fails. The window is only sized, as it fills as far as the guest's locked allocations go.
    /// A file that's already there is taken as it is, since whoever made it chose how to hold it.
    pub fn open(&self, ram_bytes: u64) -> io::Result<(File, File)> {
        let window = prepare(&self.window, WINDOW_SIZE, false)?;
        let ram = prepare(&self.ram, ram_bytes, true)?;
        Ok((window, ram))
    }
}

fn one_alone(set: &str, missing: &str) -> String {
    format!("{set} is set, and {missing} is not. The guest's memory is two files, so set both or neither.")
}

fn size_text(bytes: u64) -> String {
    if bytes.is_multiple_of(GIB) { format!("{} GiB", bytes / GIB) } else { format!("{bytes} bytes") }
}

fn failed(path: &Path, what: &str) -> io::Error {
    io::Error::other(format!("{}: {what}", path.display()))
}

/// One file at exactly `bytes`: made if it isn't there, and otherwise checked.
fn prepare(path: &Path, bytes: u64, claim: bool) -> io::Result<File> {
    let made = OpenOptions::new().read(true).write(true).create_new(true).mode(OWNER_ONLY).open(path);
    match made {
        Ok(file) => size(&file, path, bytes, claim).map(|()| file).inspect_err(|_| {
            let _ = fs::remove_file(path);
        }),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => check(path, bytes),
        Err(error) => Err(failed(path, &format!("it could not be made: {error}."))),
    }
}

fn size(file: &File, path: &Path, bytes: u64, claim: bool) -> io::Result<()> {
    file.set_len(bytes)
        .map_err(|error| failed(path, &format!("it could not be made {}: {error}.", size_text(bytes))))?;
    if !claim {
        return Ok(());
    }
    // SAFETY: the descriptor is open for as long as `file` is, and the call touches nothing else.
    let done = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, bytes as libc::off_t) };
    match done {
        0 => Ok(()),
        _ => match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOSPC | libc::ENOMEM) => {
                Err(failed(path, &format!("there isn't room for {} here.", size_text(bytes))))
            }
            Some(libc::EOPNOTSUPP) => Err(failed(path, "this file system can't reserve space for a file.")),
            _ => Err(failed(path, &format!("{} could not be reserved.", size_text(bytes)))),
        },
    }
}

/// A file that was there already has to be this guest's memory and nobody else's.
fn check(path: &Path, bytes: u64) -> io::Result<File> {
    // A link would let someone else's file stand in for it.
    let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW).open(path).map_err(
        |error| match error.raw_os_error() {
            Some(libc::EISDIR) => failed(path, NOT_A_FILE),
            _ => failed(path, &format!("it could not be opened: {error}.")),
        },
    )?;
    let found = file.metadata()?;
    // SAFETY: geteuid takes nothing and cannot fail.
    let mine = unsafe { libc::geteuid() };
    if !found.is_file() {
        return Err(failed(path, NOT_A_FILE));
    }
    if found.uid() != mine {
        return Err(failed(path, "it isn't yours, and the guest's memory has to be."));
    }
    if found.permissions().mode() & GROUP_AND_OTHERS != 0 {
        return Err(failed(path, "other users can open it. Run chmod 600 on it, or remove it."));
    }
    if found.len() != bytes {
        let wrong = format!("it is {} and the guest needs {}.", size_text(found.len()), size_text(bytes));
        return Err(failed(
            path,
            &format!("{wrong} Remove it, or set the size of the guest's RAM to match."),
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process;

    const MIB: u64 = 1 << 20;

    fn scratch(name: &str) -> PathBuf {
        let folder = env::temp_dir().join(format!("gh-memory-files-{}-{name}", process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn names(ram: Option<&str>, window: Option<&str>) -> Result<Option<MemoryFiles>, String> {
        MemoryFiles::parse(ram.map(OsString::from), window.map(OsString::from))
    }

    #[test]
    fn neither_variable_leaves_the_memory_to_the_app() {
        assert_eq!(names(None, None), Ok(None));
        assert_eq!(names(Some(""), Some("")), Ok(None));
    }

    #[test]
    fn both_variables_name_the_two_files() {
        let found = names(Some("/dev/shm/ram"), Some("/dev/shm/window")).unwrap().unwrap();
        assert_eq!((found.ram, found.window), ("/dev/shm/ram".into(), "/dev/shm/window".into()));
    }

    #[test]
    fn one_variable_alone_is_refused_in_plain_words() {
        let error = names(Some("/dev/shm/ram"), None).unwrap_err();
        assert_eq!(
            error,
            "GUESTHOUSE_RAM_FILE is set, and GUESTHOUSE_WINDOW_FILE is not. \
             The guest's memory is two files, so set both or neither."
        );
        assert!(
            names(None, Some("/dev/shm/window")).unwrap_err().starts_with("GUESTHOUSE_WINDOW_FILE is set")
        );
        assert!(names(Some("/dev/shm/ram"), Some("")).is_err());
    }

    #[test]
    fn a_relative_path_or_one_file_twice_is_refused() {
        let error = names(Some("ram"), Some("/dev/shm/window")).unwrap_err();
        assert_eq!(error, "GUESTHOUSE_RAM_FILE must be a full path, and ram is not one.");
        assert!(names(Some("/dev/shm/a"), Some("/dev/shm/a")).unwrap_err().contains("the same file"));
    }

    #[test]
    fn a_missing_file_is_made_private_sized_and_claimed() {
        let folder = scratch("made");
        let path = folder.join("ram");
        let file = prepare(&path, 4 * MIB, true).unwrap();
        let found = file.metadata().unwrap();
        assert_eq!((found.len(), found.permissions().mode() & 0o777), (4 * MIB, OWNER_ONLY));
        assert!(found.blocks() * 512 >= 4 * MIB, "it must take up its room now, and not later");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn the_window_is_sized_and_claims_nothing() {
        let folder = scratch("window");
        let file = prepare(&folder.join("window"), 4 * MIB, false).unwrap();
        let found = file.metadata().unwrap();
        assert_eq!((found.len(), found.blocks()), (4 * MIB, 0));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_file_that_is_there_with_the_right_size_is_kept_as_it_is() {
        let folder = scratch("kept");
        let path = folder.join("ram");
        prepare(&path, MIB, true).unwrap();
        fs::write(&path, b"held").unwrap();
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(MIB).unwrap();
        prepare(&path, MIB, true).unwrap();
        assert_eq!(&fs::read(&path).unwrap()[..4], b"held");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_file_of_the_wrong_size_is_refused_and_left_alone() {
        let folder = scratch("wrong-size");
        let path = folder.join("ram");
        prepare(&path, MIB, false).unwrap();
        let error = prepare(&path, 2 * MIB, true).unwrap_err().to_string();
        assert!(error.contains("it is 1048576 bytes and the guest needs 2097152 bytes."), "{error}");
        assert_eq!(fs::metadata(&path).unwrap().len(), MIB);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn sizes_are_said_in_gib_when_they_are_whole() {
        assert_eq!(size_text(16 * GIB), "16 GiB");
        assert_eq!(size_text(GIB + 1), "1073741825 bytes");
    }

    #[test]
    fn something_that_is_not_a_regular_file_is_refused() {
        let folder = scratch("not-a-file");
        let error = prepare(&folder, MIB, true).unwrap_err().to_string();
        assert!(error.contains("it is not a regular file."), "{error}");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_link_is_not_followed() {
        let folder = scratch("link");
        let real = folder.join("real");
        prepare(&real, MIB, false).unwrap();
        let link = folder.join("link");
        symlink(&real, &link).unwrap();
        let error = prepare(&link, MIB, false).unwrap_err().to_string();
        assert!(error.contains("it could not be opened"), "{error}");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn a_file_other_users_can_open_is_refused() {
        let folder = scratch("open-to-others");
        let path = folder.join("ram");
        prepare(&path, MIB, false).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = prepare(&path, MIB, false).unwrap_err().to_string();
        assert!(error.contains("other users can open it"), "{error}");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn someone_elses_file_is_refused() {
        // Owned by root, so it is someone else's unless this runs as root, and then it is open to
        // other users. Either way it can't stand in for the guest's memory.
        let error = prepare(Path::new("/etc/passwd"), MIB, false).unwrap_err().to_string();
        assert!(error.contains("isn't yours") || error.contains("other users can open it"), "{error}");
    }

    #[test]
    fn a_failed_make_leaves_no_file_behind() {
        let folder = scratch("failed");
        let path = folder.join("ram");
        // More than any file system has room for, so claiming the new file fails.
        assert!(prepare(&path, 1 << 50, true).is_err());
        assert!(!path.exists());
        fs::remove_dir_all(folder).unwrap();
    }
}
