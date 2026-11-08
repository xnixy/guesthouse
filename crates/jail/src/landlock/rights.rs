//! Landlock's rights, and which ABI brought each. Values are the kernel's, from `linux/landlock.h`.

use crate::Grant;

pub const EXECUTE: u64 = 1 << 0;
pub const WRITE_FILE: u64 = 1 << 1;
pub const READ_FILE: u64 = 1 << 2;
pub const READ_DIR: u64 = 1 << 3;
pub const REMOVE_DIR: u64 = 1 << 4;
pub const REMOVE_FILE: u64 = 1 << 5;
/// Bit 6 makes character devices, 10 named pipes and 11 block devices: never granted here.
pub const MAKE_DIR: u64 = 1 << 7;
pub const MAKE_REG: u64 = 1 << 8;
pub const MAKE_SOCK: u64 = 1 << 9;
pub const MAKE_SYM: u64 = 1 << 12;
pub const REFER: u64 = 1 << 13;
pub const TRUNCATE: u64 = 1 << 14;
pub const IOCTL_DEV: u64 = 1 << 15;
pub const RESOLVE_UNIX: u64 = 1 << 16;

/// Everything the first ABI knew, which is every right up to making symlinks.
const FIRST_FS_RIGHTS: u64 = (MAKE_SYM << 1) - 1;

pub const BIND_TCP: u64 = 1 << 0;
pub const CONNECT_TCP: u64 = 1 << 1;
pub const BIND_UDP: u64 = 1 << 2;
pub const SEND_UDP: u64 = 1 << 3;

pub const SCOPE_ABSTRACT_UNIX: u64 = 1 << 0;
pub const SCOPE_SIGNAL: u64 = 1 << 1;

/// The only rights a rule on a file, and not a folder, may carry.
pub const FILE_RIGHTS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV | RESOLVE_UNIX;

const REFER_ABI: u32 = 2;
const TRUNCATE_ABI: u32 = 3;
const TCP_ABI: u32 = 4;
const IOCTL_DEV_ABI: u32 = 5;
const SCOPE_ABI: u32 = 6;
const RESOLVE_UNIX_ABI: u32 = 9;
const UDP_ABI: u32 = 10;

/// The newest ABI this code knows. A newer kernel gets what this one knows of.
pub const NEWEST_KNOWN_ABI: u32 = UDP_ABI;

/// The file rights a kernel with this ABI can be asked to handle.
pub fn fs_rights(abi: u32) -> u64 {
    let added = [
        (REFER_ABI, REFER),
        (TRUNCATE_ABI, TRUNCATE),
        (IOCTL_DEV_ABI, IOCTL_DEV),
        (RESOLVE_UNIX_ABI, RESOLVE_UNIX),
    ];
    let later = added.iter().filter(|(since, _)| abi >= *since).fold(0, |all, (_, right)| all | right);
    if abi == 0 { 0 } else { FIRST_FS_RIGHTS | later }
}

pub fn net_rights(abi: u32) -> u64 {
    let tcp = if abi >= TCP_ABI { BIND_TCP | CONNECT_TCP } else { 0 };
    let udp = if abi >= UDP_ABI { BIND_UDP | SEND_UDP } else { 0 };
    tcp | udp
}

pub fn scopes(abi: u32) -> u64 {
    if abi >= SCOPE_ABI { SCOPE_ABSTRACT_UNIX | SCOPE_SIGNAL } else { 0 }
}

/// What a grant means, before it's cut down to what the kernel has.
pub fn of_grant(grant: Grant) -> u64 {
    const READ: u64 = READ_FILE | READ_DIR;
    match grant {
        Grant::Read => READ,
        Grant::Run => READ | EXECUTE,
        Grant::Write => READ | WRITE_FILE | TRUNCATE,
        Grant::Own => {
            READ | WRITE_FILE
                | TRUNCATE
                | REMOVE_DIR
                | REMOVE_FILE
                | MAKE_DIR
                | MAKE_REG
                | MAKE_SOCK
                | REFER
                | RESOLVE_UNIX
        }
        Grant::Device => READ_FILE | WRITE_FILE | IOCTL_DEV,
        Grant::Socket => RESOLVE_UNIX,
        Grant::List => READ_DIR,
    }
}

/// What a kernel with this ABI leaves out, in words, oldest first.
pub fn missing_at(abi: u32) -> Vec<&'static str> {
    let parts = [
        (TCP_ABI, "TCP ports"),
        (IOCTL_DEV_ABI, "device ioctls"),
        (SCOPE_ABI, "abstract sockets and signals"),
        (RESOLVE_UNIX_ABI, "unix sockets by path"),
        (UDP_ABI, "UDP ports"),
    ];
    parts.iter().filter(|(since, _)| abi < *since).map(|(_, words)| *words).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_abi_handles_what_it_brought() {
        assert_eq!(fs_rights(0), 0);
        assert_eq!(fs_rights(1) & (REFER | TRUNCATE), 0);
        assert_eq!(fs_rights(3) & TRUNCATE, TRUNCATE);
        assert_eq!(fs_rights(4) & IOCTL_DEV, 0);
        assert_eq!(fs_rights(5) & IOCTL_DEV, IOCTL_DEV);
        assert_eq!(fs_rights(8) & RESOLVE_UNIX, 0);
        assert_eq!(fs_rights(NEWEST_KNOWN_ABI) & RESOLVE_UNIX, RESOLVE_UNIX);
        assert_eq!(net_rights(3), 0);
        assert_eq!(net_rights(4), BIND_TCP | CONNECT_TCP);
        assert_eq!(net_rights(10), BIND_TCP | CONNECT_TCP | BIND_UDP | SEND_UDP);
        assert_eq!(scopes(5), 0);
    }

    #[test]
    fn a_run_folder_can_not_hold_devices_or_programs() {
        let (make_char, make_fifo, make_block) = (1 << 6, 1 << 10, 1 << 11);
        let own = of_grant(Grant::Own);
        assert_eq!(own & (EXECUTE | make_char | make_fifo | make_block | IOCTL_DEV | MAKE_SYM), 0);
    }

    #[test]
    fn ubuntu_24_04_is_told_what_it_lacks() {
        assert_eq!(
            missing_at(4),
            ["device ioctls", "abstract sockets and signals", "unix sockets by path", "UDP ports"]
        );
        assert!(missing_at(NEWEST_KNOWN_ABI).is_empty());
    }
}
