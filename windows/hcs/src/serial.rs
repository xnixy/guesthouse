//! A machine's serial port, as HCS serves it on a named pipe: one connection, read and written at
//! once. A pipe opened plainly is synchronous, and Windows lets one operation at a time through a
//! synchronous handle, so a write would wait behind a read until the guest next said something.
//! Opened overlapped, a read and a write can each be in flight.
//!
//! The pipe is only taken once the process Windows names as its server is known to be the one it
//! should be: the machine's worker, by the account it runs as. Windows names the process that made
//! the pipe's first instance, whichever instance answered. It's opened for identification only, so
//! whoever serves it may learn who opened it but never act as them.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA,
    ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LUID, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, ImpersonateSelf, LUID_AND_ATTRIBUTES,
    LookupPrivilegeValueW, RevertToSelf, SE_PRIVILEGE_ENABLED, SecurityImpersonation,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
    TokenImpersonation, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, ReadFile, SECURITY_IDENTIFICATION, WriteFile,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken,
    PROCESS_QUERY_LIMITED_INFORMATION, SetThreadToken,
};

/// How many random bytes name each boot's pipes.
const TAG_BYTES: usize = 16;
/// The system's own account.
const SYSTEM_SID: &str = "S-1-5-18";

/// Who must be serving a pipe for it to be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Server {
    /// Any process running as this account, by its SID's text: a machine's worker, whose account is
    /// `crate::worker::account`.
    Account(String),
    /// This one process, by its ID.
    Process(u32),
}

impl fmt::Display for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Server::Account(sid) => write!(f, "a process running as {sid}"),
            Server::Process(id) => write!(f, "process {id}"),
        }
    }
}

/// Why a serial port wasn't opened.
#[derive(Debug)]
pub enum OpenError {
    /// The pipe couldn't be opened, as when it isn't there yet.
    Io(io::Error),
    /// The pipe was there, but served by a process other than the one asked for, so it was let go
    /// with nothing read or written.
    OtherServer {
        expected: Server,
        process: u32,
        /// The account it runs as, or why that couldn't be read. Only read when an account is
        /// expected.
        account: Option<Result<String, String>>,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Io(error) => error.fmt(f),
            OpenError::OtherServer { expected, process, account } => {
                write!(f, "it's served by process {process}")?;
                match account {
                    Some(Ok(account)) => write!(f, ", running as {account}")?,
                    Some(Err(why)) => write!(f, ", whose account couldn't be read ({why})")?,
                    None => {}
                }
                write!(f, ", not {expected}, so it was let go")
            }
        }
    }
}

impl std::error::Error for OpenError {}

impl From<io::Error> for OpenError {
    fn from(error: io::Error) -> Self {
        OpenError::Io(error)
    }
}

/// A new random word for one boot's pipe names: 16 bytes from the system's random number
/// generator, as hex.
pub fn pipe_tag() -> io::Result<String> {
    let mut bytes = [0u8; TAG_BYTES];
    // SAFETY: the buffer has room for the bytes asked for; no algorithm handle with this flag.
    let status = unsafe {
        BCryptGenRandom(null_mut(), bytes.as_mut_ptr(), TAG_BYTES as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    if status != 0 {
        return Err(io::Error::other(format!("Windows gave no random bytes (status {status:#x})")));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The serial port's pipe, opened for reading and writing at once.
pub struct SerialPort {
    pipe: File,
}

impl SerialPort {
    /// Connects to the port's pipe, if `server` serves it. Whoever serves it may learn who opened
    /// it, and nothing more.
    #[allow(clippy::disallowed_methods)] // a named pipe, not a file on a disk
    pub fn open(pipe: &str, server: &Server) -> Result<SerialPort, OpenError> {
        let pipe = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(pipe)?;
        let process = server_process(&pipe)?;
        let served = match server {
            Server::Process(id) => (process == *id, None),
            Server::Account(sid) => {
                let account = process_account(process).map_err(|error| error.to_string());
                (account.as_deref() == Ok(sid.as_str()), Some(account))
            }
        };
        match served {
            (true, _) => Ok(SerialPort { pipe }),
            (false, account) => Err(OpenError::OtherServer { expected: server.clone(), process, account }),
        }
    }

    /// Reads what the guest has written, waiting for it: 0 once the guest has closed the port.
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.operation(buffer.as_mut_ptr(), buffer.len(), true)
    }

    /// Writes all of `bytes` to the guest, while a read may be waiting.
    pub fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = self.operation(bytes.as_ptr().cast_mut(), bytes.len(), false)?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[written..];
        }
        Ok(())
    }

    /// One overlapped read or write, waited for: how many bytes went. A pipe whose other end has
    /// gone reads as 0, the end.
    fn operation(&self, buffer: *mut u8, length: usize, read: bool) -> io::Result<usize> {
        let length = u32::try_from(length).unwrap_or(u32::MAX);
        // SAFETY: a new manual-reset event of ours, closed below.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a zeroed OVERLAPPED is a valid one to start an operation with.
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = event;
        let handle = self.pipe.as_raw_handle();
        let mut moved = 0u32;
        // SAFETY: the buffer, which the caller lends for `length` bytes, and the OVERLAPPED both
        // outlive the operation, which is waited for below before this returns.
        let result = unsafe {
            let started = if read {
                ReadFile(handle, buffer, length, std::ptr::null_mut(), &mut operation)
            } else {
                WriteFile(handle, buffer, length, std::ptr::null_mut(), &mut operation)
            };
            let error = if started == 0 { GetLastError() } else { 0 };
            if started == 0 && error != ERROR_IO_PENDING && error != ERROR_MORE_DATA {
                Err(error)
            } else if GetOverlappedResult(handle, &operation, &mut moved, 1) == 0 {
                match GetLastError() {
                    ERROR_MORE_DATA => Ok(()),
                    other => Err(other),
                }
            } else {
                Ok(())
            }
        };
        // SAFETY: the event is ours and no operation waits on it any more.
        unsafe { CloseHandle(event) };
        match result {
            Ok(()) => Ok(moved as usize),
            Err(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) if read => Ok(0),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

/// The ID of the process serving the pipe `pipe` is open on.
fn server_process(pipe: &File) -> io::Result<u32> {
    let mut process = 0u32;
    // SAFETY: the handle is open, and the ID comes back in a place for it.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut process) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(process)
}

/// The SID's text of the account process `process` runs as. A machine's worker lets only the system
/// read its token, so when this process's own identity is refused, as an elevated administrator's is,
/// the token is read again with the system's identity, taken on for that read alone.
fn process_account(process: u32) -> io::Result<String> {
    match read_account(process) {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            as_system(|| read_account(process))?
        }
        read => read,
    }
}

/// The SID's text of the account process `process` runs as, read with this thread's identity.
fn read_account(process: u32) -> io::Result<String> {
    // SAFETY: plain call; the handle is closed below.
    let opened = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process) };
    if opened.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut token: HANDLE = null_mut();
    // SAFETY: the process's handle is open, and the token comes back in a place for it.
    let got = unsafe { OpenProcessToken(opened, TOKEN_QUERY, &mut token) } != 0;
    let error = io::Error::last_os_error();
    // SAFETY: ours, and not used again.
    unsafe { CloseHandle(opened) };
    if !got {
        return Err(error);
    }
    let account = token_user(token);
    // SAFETY: ours, and not used again.
    unsafe { CloseHandle(token) };
    account.ok_or_else(|| io::Error::other("its token names no user"))
}

/// Runs `work` on this thread with the system's identity, and is this process again after it. The
/// identity is winlogon's token, the one process the system runs that an administrator may take a
/// token from; opening it needs the debug privilege, which is turned on for this thread alone.
fn as_system<T>(work: impl FnOnce() -> T) -> io::Result<T> {
    let taking = (|| {
        // SAFETY: plain call; it gives this thread a copy of the process's token, dropped by
        // RevertToSelf below.
        if unsafe { ImpersonateSelf(SecurityImpersonation) } == 0 {
            return Err(io::Error::last_os_error());
        }
        enable_debug()?;
        let system = system_token()?;
        // SAFETY: the thread's own handle and a token of ours, which the thread holds a reference
        // to once set, so it's closed here.
        let set = unsafe { SetThreadToken(null(), system) } != 0;
        let error = io::Error::last_os_error();
        // SAFETY: ours, and not used again.
        unsafe { CloseHandle(system) };
        if set { Ok(()) } else { Err(error) }
    })();
    let done = taking.map(|()| work());
    // SAFETY: plain call. A thread that can't be this process again mustn't go on as the system.
    if unsafe { RevertToSelf() } == 0 {
        std::process::abort();
    }
    done.map_err(|error| {
        io::Error::new(error.kind(), format!("the system's identity couldn't be taken on: {error}"))
    })
}

/// Turns on the debug privilege in this thread's token, which it holds while it takes on another's.
fn enable_debug() -> io::Result<()> {
    let mut token: HANDLE = null_mut();
    // SAFETY: this thread's token, opened as the process, and closed below.
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, 1, &mut token) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let name: Vec<u16> = "SeDebugPrivilege".encode_utf16().chain(Some(0)).collect();
    let mut privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: LUID { LowPart: 0, HighPart: 0 },
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: the name ends in a zero; the LUID comes back in a place for it.
    let found =
        unsafe { LookupPrivilegeValueW(null(), name.as_ptr(), &mut privileges.Privileges[0].Luid) } != 0;
    // SAFETY: the token is open for adjusting, and the privileges are as long as they say.
    let adjusted = found
        && unsafe { AdjustTokenPrivileges(token, 0, &privileges, 0, null_mut(), null_mut()) } != 0
        // SAFETY: plain call, right after the one whose error it reads: a privilege the token
        // doesn't have is a success with this error.
        && unsafe { GetLastError() } == 0;
    let error = io::Error::last_os_error();
    // SAFETY: ours, and not used again.
    unsafe { CloseHandle(token) };
    if adjusted { Ok(()) } else { Err(error) }
}

/// A token of the system's, for impersonating: winlogon's, checked to be the system's.
fn system_token() -> io::Result<HANDLE> {
    // SAFETY: plain call; the snapshot is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a zeroed entry is a valid one to fill, once its size is said.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut found = Err(io::Error::new(io::ErrorKind::NotFound, "no winlogon runs as the system"));
    // SAFETY: the snapshot is open and the entry says its size.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more && found.is_err() {
        let length = entry.szExeFile.iter().position(|&unit| unit == 0).unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case("winlogon.exe") {
            found = impersonation_token(entry.th32ProcessID);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: ours, and not used again.
    unsafe { CloseHandle(snapshot) };
    found
}

/// Process `process`'s token, copied for impersonating, if it's the system's.
fn impersonation_token(process: u32) -> io::Result<HANDLE> {
    // SAFETY: plain call; the handle is closed below.
    let opened = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process) };
    if opened.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut token: HANDLE = null_mut();
    // SAFETY: the process's handle is open, and the token comes back in a place for it.
    let got = unsafe { OpenProcessToken(opened, TOKEN_DUPLICATE | TOKEN_QUERY, &mut token) } != 0;
    let error = io::Error::last_os_error();
    // SAFETY: ours, and not used again.
    unsafe { CloseHandle(opened) };
    if !got {
        return Err(error);
    }
    let mut copy: HANDLE = null_mut();
    let copied = token_user(token).as_deref() == Some(SYSTEM_SID)
        // SAFETY: the token is open for duplicating, and the copy comes back in a place for it.
        && unsafe {
            DuplicateTokenEx(token, TOKEN_IMPERSONATE | TOKEN_QUERY, null(), SecurityImpersonation, TokenImpersonation, &mut copy)
        } != 0;
    // SAFETY: ours, and not used again.
    unsafe { CloseHandle(token) };
    if copied {
        Ok(copy)
    } else {
        Err(io::Error::other(format!("process {process} isn't the system's winlogon")))
    }
}

/// The SID's text of the user `token` is for.
fn token_user(token: HANDLE) -> Option<String> {
    let mut needed = 0u32;
    // SAFETY: asking only for the size, with no buffer.
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    // u64s, so the TOKEN_USER at its start is aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the buffer has room for `needed` bytes.
    if unsafe { GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) } == 0
    {
        return None;
    }
    // SAFETY: Windows wrote a TOKEN_USER at the buffer's start, whose SID is in the buffer too.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = null_mut();
    // SAFETY: the SID is valid; Windows allocates the text, which is freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return None;
    }
    // SAFETY: Windows wrote a string ending in a zero there.
    let length = (0..).take_while(|&at| unsafe { *text.add(at) } != 0).count();
    // SAFETY: `length` u16s are there, as just counted.
    let words = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    // SAFETY: Windows allocated it with LocalAlloc, and it's not used again.
    unsafe { LocalFree(text.cast()) };
    Some(words)
}

#[cfg(test)]
mod tests;
