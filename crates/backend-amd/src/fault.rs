//! Whether a space's work has faulted on the card.
//!
//! A fault is work reaching a GPU address with nothing mapped there. amdgpu drops the access and
//! lets the work go on, but work that faulted has lost what it was reading or writing, and what the
//! same program sends next can hang its engine. A hung engine is reset by the kernel, and a reset
//! that fails takes the host's desktop with it. So a space whose work faulted gets nothing more run.
//!
//! amdgpu keeps the last fault of each open file's address space, and each space here is an open
//! file of its own, so a look says whether this space's work faulted. Kernels before amdgpu 3.55
//! don't have the query, and nothing is known there.

use crate::info;
use crate::ioctl::{get32, get64};
use std::io;
use std::os::fd::AsRawFd;

/// AMDGPU_INFO_GPUVM_FAULT.
const QUERY_LAST_FAULT: u32 = 0x23;

// drm_amdgpu_info_gpuvm_fault, the 16-byte reply: the address, the fault status register, and
// which of the card's page table sets it was in.
const FAULT_ADDRESS: usize = 0;
const FAULT_STATUS: usize = 8;
const FAULT_REPLY_SIZE: usize = 16;

/// One fault, as amdgpu recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    /// The page the work reached.
    pub address: u64,
    /// The card's fault status register, which the kernel's log decodes.
    pub status: u32,
}

/// The last fault in the address space of `file`, if it has had one.
pub fn last(file: &impl AsRawFd) -> io::Result<Option<Fault>> {
    let mut reply = [0u8; FAULT_REPLY_SIZE];
    info::query(file, QUERY_LAST_FAULT, &mut reply)?;
    Ok(parse(&reply))
}

/// A space that never faulted reads as all zeros.
fn parse(reply: &[u8; FAULT_REPLY_SIZE]) -> Option<Fault> {
    let fault = Fault { address: get64(reply, FAULT_ADDRESS), status: get32(reply, FAULT_STATUS) };
    (fault.address != 0 || fault.status != 0).then_some(fault)
}

/// What is known of one space's faults.
#[derive(Debug, Default)]
pub struct Watch {
    found: Option<Fault>,
    /// The kernel refused the query, so it isn't asked again.
    cannot_ask: bool,
}

impl Watch {
    /// The fault this space's work had, if one was found.
    pub fn found(&self) -> Option<Fault> {
        self.found
    }

    /// Looks for a fault, unless one was found already or the kernel can't say.
    pub fn look(&mut self, file: &impl AsRawFd) -> Option<Fault> {
        if self.found.is_none() && !self.cannot_ask {
            self.take(last(file));
        }
        self.found
    }

    /// Takes what a look read.
    fn take(&mut self, read: io::Result<Option<Fault>>) {
        match read {
            Ok(fault) => self.found = fault,
            Err(error) => {
                log::debug!("amdgpu doesn't say whether work faulted here: {error}");
                self.cannot_ask = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(address: u64, status: u32) -> [u8; FAULT_REPLY_SIZE] {
        let mut bytes = [0u8; FAULT_REPLY_SIZE];
        bytes[FAULT_ADDRESS..FAULT_ADDRESS + 8].copy_from_slice(&address.to_le_bytes());
        bytes[FAULT_STATUS..FAULT_STATUS + 4].copy_from_slice(&status.to_le_bytes());
        bytes
    }

    #[test]
    fn a_space_that_never_faulted_reads_as_none() {
        assert_eq!(parse(&reply(0, 0)), None);
    }

    #[test]
    fn a_fault_carries_its_page_and_status() {
        let fault = parse(&reply(0x4_5d23_1000, 0x0064_1c51));
        assert_eq!(fault, Some(Fault { address: 0x4_5d23_1000, status: 0x0064_1c51 }));
    }

    #[test]
    fn a_fault_once_found_stays_found() {
        let found = Fault { address: 0x1000, status: 1 };
        let mut watch = Watch::default();
        watch.take(Ok(None));
        assert_eq!(watch.found(), None);
        watch.take(Ok(Some(found)));
        assert_eq!(watch.found(), Some(found));
    }

    #[test]
    fn a_kernel_without_the_query_is_asked_once() {
        let mut watch = Watch::default();
        watch.take(Err(io::Error::from_raw_os_error(libc::EINVAL)));
        assert!(watch.cannot_ask);
        assert_eq!(watch.found(), None);
    }
}
