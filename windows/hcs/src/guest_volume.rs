//! A guest's volume while its disk is attached to the host, and the only way the host reads, writes
//! or removes anything on it.
//!
//! A guest's disk holds what the guest put there, links and mount points included, which name
//! places as the guest sees them, so the host follows none of them. Every path here is opened one
//! part at a time, each part relative to the handle of the folder before it, with the link itself
//! opened rather than what it names, and a part that turns out to be a link or a mount point is
//! refused. Nothing is opened again by its whole path after it was looked at, so what was checked
//! is what is used.
//!
//! - A file is written as a new file beside it, which then takes its name, so nothing the guest
//!   left there is ever opened for writing.
//! - A link where something is removed is removed itself, and what it names is left alone.
//! - Names are always single parts: a part with a separator, a stream's colon, `.` or `..` is
//!   refused.

#![allow(clippy::disallowed_methods)] // the one place a guest's disk is opened

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_BASIC_INFORMATION, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DISPOSITION_INFORMATION,
    FILE_FULL_DIR_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_STANDARD_INFORMATION,
    FILE_SYNCHRONOUS_IO_NONALERT, FileBasicInformation, FileDispositionInformation,
    FileFullDirectoryInformation, FileRenameInformation, FileStandardInformation, NtCreateFile,
    NtQueryDirectoryFile, NtQueryInformationFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, LocalFree, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
    STATUS_NO_MORE_FILES, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, ImpersonateSelf, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, RevertToSelf,
    SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME, SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_IMPERSONATE, TOKEN_QUERY,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SetKernelObjectSecurity, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, GetVolumeInformationByHandleW,
    READ_CONTROL, SYNCHRONIZE, WRITE_DAC,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken, SetThreadToken};

/// How deep a folder removed or given permissions may go. A guest's own folders are far
/// shallower; past this the work stops with an error rather than going on.
const MOST_DEPTH: usize = 64;
/// Windows' file times count 100 ns steps from 1601; this many of them is 1970.
const UNIX_EPOCH_IN_FILE_TIME: i64 = 116_444_736_000_000_000;
/// The folder a listing is read in, a part at a time.
const LISTING_BYTES: usize = 64 * 1024;

/// What is at a path on a guest's volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    File {
        len: u64,
        modified: SystemTime,
    },
    Folder,
    /// A link or a mount point, which is never followed.
    Link,
}

/// A volume's file system and label, as its root says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSystem {
    pub name: String,
    pub label: String,
}

/// Who may do what in a folder `GuestVolume::give_only` sets: an SDDL trustee such as `SY`, and the
/// rights it gets, such as `FA`.
pub type Grant<'a> = (&'a str, &'a str);

/// A guest's volume, opened at its root.
pub struct GuestVolume {
    root: OwnedHandle,
    path: PathBuf,
}

/// A folder handle the walk is at: the volume's root, or a folder opened from it.
enum At<'a> {
    Root(&'a OwnedHandle),
    Opened(OwnedHandle),
}

impl At<'_> {
    fn raw(&self) -> HANDLE {
        match self {
            At::Root(handle) => handle.as_raw_handle(),
            At::Opened(handle) => handle.as_raw_handle(),
        }
    }
}

impl GuestVolume {
    /// Opens the volume whose root is `root`, such as `\\?\Volume{...}\` or `E:\`. The root itself
    /// may not be a link.
    pub fn open(root: &Path) -> io::Result<GuestVolume> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(root)?;
        let root_handle = OwnedHandle::from(file);
        let seen = look(&root_handle)?;
        if seen.link {
            return Err(link_refused(&root.display().to_string()));
        }
        if seen.attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", root.display()),
            ));
        }
        Ok(GuestVolume { root: root_handle, path: root.to_owned() })
    }

    /// The root this was opened at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path on the volume as the host would write it, for messages.
    pub fn shown(&self, relative: impl AsRef<Path>) -> String {
        self.path.join(relative).display().to_string()
    }

    /// The volume's file system, such as `NTFS` or `FAT32`, and its label.
    pub fn file_system(&self) -> io::Result<FileSystem> {
        let mut label = [0u16; 261];
        let mut name = [0u16; 261];
        // SAFETY: the handle is open, and each buffer is as long as the count given with it.
        let done = unsafe {
            GetVolumeInformationByHandleW(
                self.root.as_raw_handle(),
                label.as_mut_ptr(),
                label.len() as u32,
                null_mut(),
                null_mut(),
                null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        if done == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileSystem { name: from_wide(&name), label: from_wide(&label) })
    }

    /// What is at `relative`, `None` when nothing is. A link on the way there is an error.
    pub fn entry(&self, relative: impl AsRef<Path>) -> io::Result<Option<Entry>> {
        let relative = relative.as_ref();
        let (folder, name) = split(relative)?;
        let at = match self.walk(&folder, false) {
            Ok(at) => at,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let handle = match open_at(at.raw(), name, FILE_READ_ATTRIBUTES, FILE_OPEN, 0) {
            Ok(handle) => handle,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(self.context(relative, error)),
        };
        if look(&handle)?.link {
            return Ok(Some(Entry::Link));
        }
        let basic = basic(&handle)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Ok(Some(Entry::Folder));
        }
        let standard: FILE_STANDARD_INFORMATION = query(&handle, FileStandardInformation)?;
        Ok(Some(Entry::File { len: standard.EndOfFile as u64, modified: system_time(basic.LastWriteTime) }))
    }

    /// Whether `relative` is a file, and not a link or a folder.
    pub fn is_file(&self, relative: impl AsRef<Path>) -> bool {
        matches!(self.entry(relative), Ok(Some(Entry::File { .. })))
    }

    /// Whether `relative` is a folder, and not a link to one.
    pub fn is_dir(&self, relative: impl AsRef<Path>) -> bool {
        matches!(self.entry(relative), Ok(Some(Entry::Folder)))
    }

    /// The file at `relative`, up to `most` bytes of it.
    pub fn read(&self, relative: impl AsRef<Path>, most: u64) -> io::Result<Vec<u8>> {
        let relative = relative.as_ref();
        let (folder, name) = split(relative)?;
        let at = self.walk(&folder, false)?;
        let handle = open_at(
            at.raw(),
            name,
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
        )
        .map_err(|error| self.context(relative, error))?;
        if look(&handle)?.link {
            return Err(link_refused(&self.shown(relative)));
        }
        let mut bytes = Vec::new();
        File::from(handle).take(most).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// The names in the folder at `relative`, with what each is.
    pub fn read_dir(&self, relative: impl AsRef<Path>) -> io::Result<Vec<(OsString, Entry)>> {
        let relative = relative.as_ref();
        let at = self.walk(&parts(relative)?, false)?;
        listing(at.raw()).map_err(|error| self.context(relative, error))
    }

    /// Makes the folder at `relative` and the folders on the way to it, where they aren't there.
    pub fn create_dir_all(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        self.walk(&parts(relative.as_ref())?, true).map(drop)
    }

    /// Writes `contents` as the file at `relative`, making the folders on the way to it.
    pub fn write(&self, relative: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
        self.replace(relative.as_ref(), None, |file| io::Write::write_all(file, contents.as_ref()))
    }

    /// Copies the host's file `from` to `relative`, with its time, making the folders on the way.
    pub fn copy_in(&self, from: &Path, relative: impl AsRef<Path>) -> io::Result<()> {
        let mut source = File::open(from)?;
        let modified = source.metadata()?.last_write_time() as i64;
        self.replace(relative.as_ref(), Some(modified), |file| io::copy(&mut source, file).map(drop))
    }

    /// Removes the file at `relative`. A link there is removed itself.
    pub fn remove_file(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        let relative = relative.as_ref();
        let (folder, name) = split(relative)?;
        let at = self.walk(&folder, false)?;
        let handle = open_at(
            at.raw(),
            name,
            DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
        )
        .map_err(|error| self.context(relative, error))?;
        remove_handle(&handle).map_err(|error| self.context(relative, error))
    }

    /// Removes the folder at `relative` with everything in it. A link in it, or at `relative`
    /// itself, is removed itself, and what it names is left alone.
    pub fn remove_tree(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        let relative = relative.as_ref();
        let (folder, name) = split(relative)?;
        let at = self.walk(&folder, false)?;
        remove_at(at.raw(), name, 0).map_err(|error| self.context(relative, error))
    }

    /// Makes the folder at `relative` if it isn't there, and lets only `grants` into it and into
    /// everything in it, whatever it allowed before. The folder's own permissions are no longer
    /// taken from the folder it is in, and what is in it takes the folder's. A link at `relative`
    /// or anywhere in it is removed, as a folder only these may write to holds none.
    pub fn give_only(&self, relative: impl AsRef<Path>, grants: &[Grant]) -> io::Result<()> {
        let relative = relative.as_ref();
        let (folder, name) = split(relative)?;
        let parent = self.walk(&folder, true)?;
        // What a user of the guest set on something in the folder can't keep it from being set
        // right, so this step reads and writes past permissions, as a backup does. Only this
        // thread, and only until this returns.
        let _privileged = Privileged::enable();
        let access = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC;
        let create = || open_at(parent.raw(), name, access, FILE_CREATE, FILE_DIRECTORY_FILE);
        let mut handle = match open_at(parent.raw(), name, access | DELETE, FILE_OPEN, 0) {
            Ok(handle) => Ok(handle),
            Err(error) if error.kind() == io::ErrorKind::NotFound => create(),
            Err(error) => Err(error),
        }
        .map_err(|error| self.context(relative, error))?;
        let seen = look(&handle)?;
        let attributes = seen.attributes;
        if seen.link {
            remove_handle(&handle)?;
            drop(handle);
            handle = create().map_err(|error| self.context(relative, error))?;
        } else if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is a file, not a folder", self.shown(relative)),
            ));
        }
        set_security(&handle, &descriptor("PAI", "OICI", grants), PROTECTED_DACL_SECURITY_INFORMATION)
            .map_err(|error| self.context(relative, error))?;
        let inherited =
            Inherited { folder: descriptor("AI", "OICIID", grants), file: descriptor("AI", "ID", grants) };
        inherit_below(handle.as_raw_handle(), &inherited, 0).map_err(|error| self.context(relative, error))
    }

    /// Opens the folders `parts` name from the root, one at a time, each refused if it is a link.
    /// With `create`, those that aren't there are made.
    fn walk(&self, parts: &[&OsStr], create: bool) -> io::Result<At<'_>> {
        let mut at = At::Root(&self.root);
        let mut shown = self.path.clone();
        for part in parts {
            shown.push(part);
            let disposition = if create { FILE_OPEN_IF } else { FILE_OPEN };
            let handle = open_at(
                at.raw(),
                part,
                FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES,
                disposition,
                FILE_DIRECTORY_FILE,
            )
            .map_err(|error| with_path(&shown.display().to_string(), error))?;
            if look(&handle)?.link {
                return Err(link_refused(&shown.display().to_string()));
            }
            at = At::Opened(handle);
        }
        Ok(at)
    }

    /// Writes a new file beside `relative` with `fill`, then gives it `relative`'s name in place of
    /// what was there. What is there may be a file or nothing: a link or a folder is refused.
    fn replace(
        &self,
        relative: &Path,
        modified: Option<i64>,
        fill: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        let (folder, name) = split(relative)?;
        let at = self.walk(&folder, true)?;
        match open_at(at.raw(), name, FILE_READ_ATTRIBUTES, FILE_OPEN, 0) {
            Ok(there) => {
                let seen = look(&there)?;
                let attributes = seen.attributes;
                if seen.link {
                    return Err(link_refused(&self.shown(relative)));
                }
                if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::IsADirectory,
                        format!("{} is a folder, not a file", self.shown(relative)),
                    ));
                }
                // A read-only file would refuse to be replaced, so it's cleared first.
                if attributes & FILE_ATTRIBUTE_READONLY != 0 {
                    drop(there);
                    let there =
                        open_at(at.raw(), name, FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, FILE_OPEN, 0)
                            .map_err(|error| self.context(relative, error))?;
                    if look(&there)?.link {
                        return Err(link_refused(&self.shown(relative)));
                    }
                    let cleared = (attributes & !FILE_ATTRIBUTE_READONLY).max(FILE_ATTRIBUTE_NORMAL);
                    set_basic_raw(
                        there.as_raw_handle(),
                        FILE_BASIC_INFORMATION { FileAttributes: cleared, ..zeroed_basic() },
                    )?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(self.context(relative, error)),
        }
        let temporary = temporary_name(name);
        let handle = open_at(
            at.raw(),
            &temporary,
            FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | FILE_READ_ATTRIBUTES | DELETE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
        )
        .map_err(|error| self.context(relative, error))?;
        let mut file = File::from(handle);
        let done = fill(&mut file)
            .and_then(|()| match modified {
                Some(time) => {
                    set_basic(&file, FILE_BASIC_INFORMATION { LastWriteTime: time, ..zeroed_basic() })
                }
                None => Ok(()),
            })
            .and_then(|()| rename_in_place(&file, name));
        if done.is_err() {
            let _ = dispose(file.as_raw_handle());
        }
        done.map_err(|error| self.context(relative, error))
    }

    fn context(&self, relative: &Path, error: io::Error) -> io::Error {
        with_path(&self.shown(relative), error)
    }
}

thread_local! {
    /// Whether this thread opens with backup intent, while it holds `Privileged`.
    static BACKUP: Cell<bool> = const { Cell::new(false) };
}

fn backup_intent() -> u32 {
    if BACKUP.get() { FILE_OPEN_FOR_BACKUP_INTENT } else { 0 }
}

/// The backup and restore privileges, on this thread only, while this is held. The thread takes a
/// copy of the process's token with them on, and has the token it had before back when this is
/// dropped, however the work ends. A process that doesn't hold them, as one not run as the system
/// or an elevated administrator, goes on without them.
struct Privileged {
    /// The thread's own token before, when it had one.
    before: Option<HANDLE>,
    impersonating: bool,
}

/// `TOKEN_PRIVILEGES` with room for two.
#[repr(C)]
struct TwoPrivileges {
    count: u32,
    privileges: [LUID_AND_ATTRIBUTES; 2],
}

impl Privileged {
    fn enable() -> Privileged {
        let mut before: HANDLE = null_mut();
        // SAFETY: plain calls on this thread; `before` is closed in `drop`.
        let had = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_IMPERSONATE, 1, &mut before) } != 0;
        let before = had.then_some(before);
        // SAFETY: plain call; it gives this thread a copy of the process's token.
        if unsafe { ImpersonateSelf(SecurityImpersonation) } == 0 {
            return Privileged { before, impersonating: false };
        }
        let privileged = Privileged { before, impersonating: true };
        let mut token: HANDLE = null_mut();
        // SAFETY: as above; the token is closed below.
        if unsafe {
            OpenThreadToken(GetCurrentThread(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, 1, &mut token)
        } == 0
        {
            return privileged;
        }
        let mut wanted = TwoPrivileges {
            count: 2,
            privileges: [LUID_AND_ATTRIBUTES { Luid: Default::default(), Attributes: SE_PRIVILEGE_ENABLED };
                2],
        };
        // SAFETY: the names are the system's constants, and each LUID is written in place.
        let found = unsafe {
            LookupPrivilegeValueW(null(), SE_BACKUP_NAME, &mut wanted.privileges[0].Luid) != 0
                && LookupPrivilegeValueW(null(), SE_RESTORE_NAME, &mut wanted.privileges[1].Luid) != 0
        };
        if found {
            // SAFETY: the structure is a TOKEN_PRIVILEGES of two. A privilege the token doesn't
            // hold is left off, which isn't an error here.
            unsafe {
                AdjustTokenPrivileges(token, 0, (&raw const wanted).cast(), 0, null_mut(), null_mut());
            }
        }
        // SAFETY: ours, and not used after this.
        unsafe { CloseHandle(token) };
        BACKUP.set(true);
        privileged
    }
}

impl Drop for Privileged {
    fn drop(&mut self) {
        BACKUP.set(false);
        let before = self.before.take();
        if !self.impersonating {
            if let Some(before) = before {
                // SAFETY: ours, and not used after this.
                unsafe { CloseHandle(before) };
            }
            return;
        }
        // SAFETY: plain calls on this thread, and `before` is ours.
        let back = match before {
            Some(before) => unsafe {
                let set = SetThreadToken(null(), before);
                CloseHandle(before);
                set
            },
            None => unsafe { RevertToSelf() },
        };
        // A thread that can't put its token back mustn't go on with the privileges.
        if back == 0 {
            std::process::abort();
        }
    }
}

/// The DACLs `give_only` gives what is in the folder, as SDDL.
struct Inherited {
    folder: String,
    file: String,
}

/// An SDDL DACL giving each grant, with these DACL flags and these ACE flags.
fn descriptor(dacl_flags: &str, ace_flags: &str, grants: &[Grant]) -> String {
    let aces: String =
        grants.iter().map(|(trustee, rights)| format!("(A;{ace_flags};{rights};;;{trustee})")).collect();
    format!("D:{dacl_flags}{aces}")
}

/// Gives everything in the folder `folder` the inherited permissions, and removes links in it.
fn inherit_below(folder: HANDLE, inherited: &Inherited, depth: usize) -> io::Result<()> {
    if depth >= MOST_DEPTH {
        return Err(io::Error::other("its folders go deeper than the host goes"));
    }
    for (name, entry) in listing(folder)? {
        if entry == Entry::Link {
            remove_at(folder, &name, depth + 1)?;
            continue;
        }
        let access = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC;
        let handle = match open_at(folder, &name, access, FILE_OPEN, 0) {
            Ok(handle) => handle,
            // What the folder's own permissions can't be set on can't stay in it. Removing it needs
            // only the folder's, which are set by now.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                remove_at(folder, &name, depth + 1)?;
                continue;
            }
            Err(error) => return Err(with_name(&name, error)),
        };
        let seen = look(&handle)?;
        let attributes = seen.attributes;
        if seen.link {
            drop(handle);
            remove_at(folder, &name, depth + 1)?;
        } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            set_security(&handle, &inherited.folder, UNPROTECTED_DACL_SECURITY_INFORMATION)
                .map_err(|error| with_name(&name, error))?;
            inherit_below(handle.as_raw_handle(), inherited, depth + 1)
                .map_err(|error| with_name(&name, error))?;
        } else {
            set_security(&handle, &inherited.file, UNPROTECTED_DACL_SECURITY_INFORMATION)
                .map_err(|error| with_name(&name, error))?;
        }
    }
    Ok(())
}

/// Removes `name` in the folder `folder`: a file or a link as it is, a folder with what is in it.
fn remove_at(folder: HANDLE, name: &OsStr, depth: usize) -> io::Result<()> {
    // Only what the folder it is in lets do: a file whose own permissions let nobody in still goes.
    check_name(name)?;
    let handle = open_relative(folder, name, DELETE | FILE_READ_ATTRIBUTES, FILE_OPEN, 0, false)
        .map_err(|error| with_name(name, error))?;
    let seen = look(&handle)?;
    if seen.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 && !seen.link {
        if depth >= MOST_DEPTH {
            return Err(io::Error::other("its folders go deeper than the host goes"));
        }
        // The same folder again, through its handle, to list it.
        let listed =
            reopen(&handle, FILE_LIST_DIRECTORY | FILE_TRAVERSE).map_err(|error| with_name(name, error))?;
        for (child, _) in listing(listed.as_raw_handle())? {
            remove_at(listed.as_raw_handle(), &child, depth + 1).map_err(|error| with_name(name, error))?;
        }
    }
    remove_handle(&handle).map_err(|error| with_name(name, error))
}

/// Removes what `handle` has open once it's closed, clearing read-only first, as a file with it
/// isn't removed.
fn remove_handle(handle: &impl AsRawHandle) -> io::Result<()> {
    let raw = handle.as_raw_handle();
    let attributes = basic_raw(raw)?.FileAttributes;
    if attributes & FILE_ATTRIBUTE_READONLY != 0 {
        let cleared = (attributes & !FILE_ATTRIBUTE_READONLY).max(FILE_ATTRIBUTE_NORMAL);
        let writable = reopen_raw(raw, FILE_WRITE_ATTRIBUTES)?;
        set_basic_raw(
            writable.as_raw_handle(),
            FILE_BASIC_INFORMATION { FileAttributes: cleared, ..zeroed_basic() },
        )?;
    }
    dispose(raw)
}

fn dispose(handle: HANDLE) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: the handle is open with DELETE, and the information is the size given.
    nt(unsafe {
        NtSetInformationFile(
            handle,
            &mut status,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFORMATION>() as u32,
            FileDispositionInformation,
        )
    })
}

/// Gives the file `file` has open the name `name` in the folder it is in, in place of what has it.
fn rename_in_place(file: &File, name: &OsStr) -> io::Result<()> {
    let units: Vec<u16> = name.encode_wide().collect();
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let bytes = name_offset + units.len() * 2;
    // u64s, so the buffer is aligned for the structure's handle.
    let mut buffer = vec![0u64; bytes.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: the buffer is zeroed, aligned and long enough for the structure and the name after
    // it. A bare name with no root renames the file in the folder it is in.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = null_mut();
        (*info).FileNameLength = (units.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            units.as_ptr(),
            buffer.as_mut_ptr().cast::<u8>().add(name_offset).cast(),
            units.len(),
        );
    }
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: the handle is open with DELETE, and the buffer is `bytes` long at least.
    nt(unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            buffer.as_ptr().cast(),
            bytes as u32,
            FileRenameInformation,
        )
    })
}

/// A name for a new file beside `name` that nothing has.
fn temporary_name(name: &OsStr) -> OsString {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".{}-{stamp}-{}.part", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    temporary
}

/// Opens `name` in the folder `folder`, the link itself if it is one, never what it names.
fn open_at(
    folder: HANDLE,
    name: &OsStr,
    access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    check_name(name)?;
    open_relative(folder, name, access, disposition, options, true)
}

/// Opens what `handle` has open again, with `access`, through the handle itself.
fn reopen(handle: &OwnedHandle, access: u32) -> io::Result<OwnedHandle> {
    reopen_raw(handle.as_raw_handle(), access)
}

fn reopen_raw(handle: HANDLE, access: u32) -> io::Result<OwnedHandle> {
    open_relative(handle, OsStr::new(""), access, FILE_OPEN, 0, true)
}

/// `NtCreateFile` relative to `folder`, never following a link. An empty name is `folder` itself.
/// A handle that isn't `synchronous` is only for asking and setting what a file is, which needs no
/// SYNCHRONIZE right: the folder's permissions alone let what is in it be looked at and removed.
fn open_relative(
    folder: HANDLE,
    name: &OsStr,
    access: u32,
    disposition: u32,
    options: u32,
    synchronous: bool,
) -> io::Result<OwnedHandle> {
    let (access, options) = if synchronous {
        (access | SYNCHRONIZE, options | FILE_SYNCHRONOUS_IO_NONALERT)
    } else {
        (access, options)
    };
    let units: Vec<u16> = name.encode_wide().collect();
    let length = u16::try_from(units.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidFilename, "the name is too long"))?;
    let object_name =
        UNICODE_STRING { Length: length, MaximumLength: length, Buffer: units.as_ptr().cast_mut() };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: folder,
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut handle: HANDLE = null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: every pointer is to a value that outlives the call; the name is `length` bytes.
    nt(unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            options | FILE_OPEN_REPARSE_POINT | backup_intent(),
            null(),
            0,
        )
    })?;
    // SAFETY: the call succeeded, so the handle is open and ours.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Refuses a name that isn't a single plain part of a path.
fn check_name(name: &OsStr) -> io::Result<()> {
    let units: Vec<u16> = name.encode_wide().collect();
    let plain = !units.is_empty()
        && name != "."
        && name != ".."
        && !units.iter().any(|&unit| {
            unit == 0 || unit == u16::from(b'\\') || unit == u16::from(b'/') || unit == u16::from(b':')
        });
    if plain {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidFilename, format!("{} is not a plain name", name.display())))
    }
}

/// The parts of a relative path, each a plain name.
fn parts(relative: &Path) -> io::Result<Vec<&OsStr>> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => check_name(part).map(|()| part),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidFilename,
                format!("{} is not a plain path inside the volume", relative.display()),
            )),
        })
        .collect()
}

/// A relative path's folders and its last part.
fn split(relative: &Path) -> io::Result<(Vec<&OsStr>, &OsStr)> {
    let mut parts = parts(relative)?;
    let name = parts
        .pop()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidFilename, "an empty path names nothing"))?;
    Ok((parts, name))
}

/// The names in the open folder `folder`, but `.` and `..`, with what each is.
fn listing(folder: HANDLE) -> io::Result<Vec<(OsString, Entry)>> {
    let mut found = Vec::new();
    let mut buffer = vec![0u64; LISTING_BYTES / 8];
    let mut restart = true;
    loop {
        let mut status = IO_STATUS_BLOCK::default();
        // SAFETY: the handle is an open folder, and the buffer is as long as the length given.
        let code = unsafe {
            NtQueryDirectoryFile(
                folder,
                null_mut(),
                None,
                null(),
                &mut status,
                buffer.as_mut_ptr().cast(),
                LISTING_BYTES as u32,
                FileFullDirectoryInformation,
                false,
                null(),
                restart,
            )
        };
        restart = false;
        if code == STATUS_NO_MORE_FILES {
            return Ok(found);
        }
        nt(code)?;
        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the folder's listing put whole entries in the buffer, each at the offset the
            // one before gives; `read_unaligned`, as the name's length isn't a multiple of 8.
            let info = unsafe { base.add(offset).cast::<FILE_FULL_DIR_INFORMATION>().read_unaligned() };
            let name_at = offset + std::mem::offset_of!(FILE_FULL_DIR_INFORMATION, FileName);
            let units = info.FileNameLength as usize / 2;
            // SAFETY: as above; the name is `FileNameLength` bytes after the fixed part.
            let name: Vec<u16> = (0..units)
                .map(|at| unsafe { base.add(name_at + at * 2).cast::<u16>().read_unaligned() })
                .collect();
            let name = OsString::from_wide(&name);
            if name != "." && name != ".." {
                // With the reparse attribute, EaSize holds the reparse tag.
                let entry = if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    && redirects_name(info.EaSize)
                {
                    Entry::Link
                } else if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    Entry::Folder
                } else {
                    Entry::File { len: info.EndOfFile as u64, modified: system_time(info.LastWriteTime) }
                };
                found.push((name, entry));
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }
    }
}

/// A handle's attributes, and whether it is a link: a reparse point whose tag redirects its name,
/// as a symbolic link, a junction or a mount point does. Other reparse points, such as a compressed
/// or deduplicated file's, are files and folders like any other.
struct Look {
    attributes: u32,
    link: bool,
}

/// `FILE_ATTRIBUTE_TAG_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct AttributeTag {
    attributes: u32,
    tag: u32,
}

/// `FileAttributeTagInformation`.
const ATTRIBUTE_TAG_INFORMATION: i32 = 35;
/// The bit of a reparse tag that says it redirects a name: `IsReparseTagNameSurrogate`.
const NAME_SURROGATE: u32 = 0x2000_0000;

fn redirects_name(tag: u32) -> bool {
    tag & NAME_SURROGATE != 0
}

fn look(handle: &impl AsRawHandle) -> io::Result<Look> {
    let info: AttributeTag = query_raw(handle.as_raw_handle(), ATTRIBUTE_TAG_INFORMATION)?;
    let link = info.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && redirects_name(info.tag);
    Ok(Look { attributes: info.attributes, link })
}

fn basic(handle: &impl AsRawHandle) -> io::Result<FILE_BASIC_INFORMATION> {
    basic_raw(handle.as_raw_handle())
}

fn basic_raw(handle: HANDLE) -> io::Result<FILE_BASIC_INFORMATION> {
    query_raw(handle, FileBasicInformation)
}

fn query<T: Default>(handle: &impl AsRawHandle, class: i32) -> io::Result<T> {
    query_raw(handle.as_raw_handle(), class)
}

fn query_raw<T: Default>(handle: HANDLE, class: i32) -> io::Result<T> {
    let mut info = T::default();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: the handle is open, and the structure is the class's and of the size given.
    nt(unsafe {
        NtQueryInformationFile(handle, &mut status, (&raw mut info).cast(), size_of::<T>() as u32, class)
    })?;
    Ok(info)
}

fn set_basic(file: &File, info: FILE_BASIC_INFORMATION) -> io::Result<()> {
    set_basic_raw(file.as_raw_handle(), info)
}

/// Sets what of `info` isn't zero: a zero time or attributes leave those as they are.
fn set_basic_raw(handle: HANDLE, info: FILE_BASIC_INFORMATION) -> io::Result<()> {
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: the handle is open with FILE_WRITE_ATTRIBUTES, and the structure is the size given.
    nt(unsafe {
        NtSetInformationFile(
            handle,
            &mut status,
            (&raw const info).cast(),
            size_of::<FILE_BASIC_INFORMATION>() as u32,
            FileBasicInformation,
        )
    })
}

fn zeroed_basic() -> FILE_BASIC_INFORMATION {
    FILE_BASIC_INFORMATION {
        CreationTime: 0,
        LastAccessTime: 0,
        LastWriteTime: 0,
        ChangeTime: 0,
        FileAttributes: 0,
    }
}

/// Sets the DACL SDDL `dacl` gives on what `handle` has open, and no more of its security.
fn set_security(handle: &OwnedHandle, dacl: &str, protection: u32) -> io::Result<()> {
    let text: Vec<u16> = OsStr::new(dacl).encode_wide().chain([0]).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the text ends in a zero, and the descriptor made is freed below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle is open with WRITE_DAC, and the descriptor is the one just made.
    let set = unsafe {
        SetKernelObjectSecurity(handle.as_raw_handle(), DACL_SECURITY_INFORMATION | protection, descriptor)
    };
    let error = (set == 0).then(io::Error::last_os_error);
    // SAFETY: the descriptor was allocated by the conversion, with LocalAlloc.
    unsafe { LocalFree(descriptor) };
    error.map_or(Ok(()), Err)
}

fn nt(status: NTSTATUS) -> io::Result<()> {
    if status >= 0 {
        Ok(())
    } else {
        // SAFETY: a plain lookup of a status code.
        Err(io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32))
    }
}

fn link_refused(shown: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{shown} is a link or a mount point, and the host follows no links on a guest's disk"),
    )
}

fn with_path(shown: &str, error: io::Error) -> io::Error {
    if error.to_string().contains(shown) {
        return error;
    }
    io::Error::new(error.kind(), format!("{shown}: {error}"))
}

fn with_name(name: &OsStr, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", name.display()))
}

fn system_time(file_time: i64) -> SystemTime {
    let since = file_time - UNIX_EPOCH_IN_FILE_TIME;
    let steps = Duration::from_nanos(since.unsigned_abs().saturating_mul(100));
    if since >= 0 { UNIX_EPOCH + steps } else { UNIX_EPOCH - steps }
}

fn from_wide(units: &[u16]) -> String {
    let end = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

#[cfg(test)]
pub(crate) mod tests;
