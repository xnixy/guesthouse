//! GPU address requests: mapping, reserving and updating, and the memory query.

use crate::Status;
use crate::wire::{Malformed, clamped, put_u32, put_u64, u32_at, u64_at};

/// Bytes in the guest's GPU pages.
pub const PAGE_SIZE: u64 = 4096;

/// A request to map an allocation.
#[derive(Debug, Clone, Copy)]
pub struct MapRequest {
    /// Where the guest wants it, or zero for wherever the host likes.
    pub address: u64,
    /// When the host chooses, the lowest address that will do.
    pub lowest: u64,
    /// When the host chooses, where the mapping must have ended by, or zero for no limit.
    pub limit: u64,
    /// Zero means the guest is only setting a range aside.
    pub allocation: u32,
    pub first_page: u64,
    pub pages: u64,
}

impl MapRequest {
    const ADDRESS: usize = 0x20;
    const LOWEST: usize = 0x28;
    const LIMIT: usize = 0x30;
    const ALLOCATION: usize = 0x38;
    const FIRST_PAGE: usize = 0x40;
    const PAGES: usize = 0x48;

    pub fn parse(message: &[u8]) -> Result<Self, Malformed> {
        Ok(Self {
            address: u64_at(message, Self::ADDRESS)?,
            lowest: u64_at(message, Self::LOWEST)?,
            limit: u64_at(message, Self::LIMIT)?,
            allocation: u32_at(message, Self::ALLOCATION)?,
            first_page: u64_at(message, Self::FIRST_PAGE)?,
            pages: u64_at(message, Self::PAGES)?,
        })
    }
}

/// Writes the answer shared by mapping, residency and eviction: two values and a status.
pub fn two_values_and_status(first: u64, second: u64, status: Status, out: &mut Vec<u8>) {
    put_u64(out, first);
    put_u64(out, second);
    put_u32(out, status.0);
    put_u32(out, 0);
}

/// A request to set GPU addresses aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveRequest {
    /// Exactly where, or zero to leave that to the host.
    pub base: u64,
    /// The lowest address that will do.
    pub lowest: u64,
    /// Where it must have ended by, or zero for no limit.
    pub limit: u64,
    pub size: u64,
}

impl ReserveRequest {
    const BASE: usize = 0x20;
    const LOWEST: usize = 0x28;
    const LIMIT: usize = 0x30;
    const SIZE: usize = 0x38;

    pub fn parse(message: &[u8]) -> Result<Self, Malformed> {
        Ok(Self {
            base: u64_at(message, Self::BASE)?,
            lowest: u64_at(message, Self::LOWEST)?,
            limit: u64_at(message, Self::LIMIT)?,
            size: u64_at(message, Self::SIZE)?,
        })
    }
}

/// The start of a range the guest gives back.
pub fn freed_address(message: &[u8]) -> Result<u64, Malformed> {
    u64_at(message, 0x28)?;
    u64_at(message, 0x20)
}

/// A batch of address changes. It waits for a fence to reach a value, unless the guest says it
/// mustn't wait, and the fence goes one past that value when it's done.
#[derive(Debug, Clone, Copy)]
pub struct UpdateRequest<'a> {
    pub fence_value: u64,
    pub fence: u32,
    pub count: usize,
    /// The guest asked for the changes now, whatever the fence is at.
    pub no_wait: bool,
    operations: &'a [u8],
}

impl<'a> UpdateRequest<'a> {
    const FENCE_VALUE: usize = 0x18;
    const FENCE: usize = 0x28;
    const COUNT: usize = 0x2C;
    const FLAGS: usize = 0x30;
    const NO_WAIT: u32 = 1;
    const OPERATIONS: usize = 0x38;
    pub const OPERATION_SIZE: usize = 64;

    pub fn parse(message: &'a [u8]) -> Result<Self, Malformed> {
        let count = u32_at(message, Self::COUNT)? as usize;
        Ok(Self {
            fence_value: u64_at(message, Self::FENCE_VALUE)?,
            fence: u32_at(message, Self::FENCE)?,
            count,
            no_wait: u32_at(message, Self::FLAGS)? & Self::NO_WAIT != 0,
            operations: clamped(message, Self::OPERATIONS, count.saturating_mul(Self::OPERATION_SIZE)),
        })
    }

    /// Whether the message really holds as many operations as it says.
    pub fn is_complete(&self) -> bool {
        self.operations.len() == self.count.saturating_mul(Self::OPERATION_SIZE)
    }

    /// The raw operations, to keep when the update has to wait.
    pub fn raw_operations(&self) -> &'a [u8] {
        self.operations
    }
}

/// One change of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOperation {
    /// `driver_protection` is the driver's own word on how it wants the pages, which only a protected
    /// map carries.
    Map {
        address: u64,
        size: u64,
        allocation: u32,
        offset: u64,
        driver_protection: Option<u64>,
    },
    Unmap {
        address: u64,
        size: u64,
    },
    Copy {
        source: u64,
        size: u64,
        destination: u64,
    },
    Unknown(u32),
}

impl UpdateOperation {
    const KIND_MAP: u32 = 0;
    const KIND_UNMAP: u32 = 1;
    const KIND_COPY: u32 = 2;
    const KIND_MAP_PROTECTED: u32 = 3;
    const ADDRESS: usize = 0x08;
    const SIZE: usize = 0x10;
    const THIRD: usize = 0x18;
    const OFFSET: usize = 0x20;
    /// After the offset come the allocation range's size and the protection, then the driver's value.
    const DRIVER_PROTECTION: usize = 0x38;
    /// Reads past here would pass the end of an operation cut short by the message.
    const FULL_SIZE: usize = 0x40;

    pub fn parse(operation: &[u8]) -> Result<Self, Malformed> {
        let kind = u32_at(operation, 0)?;
        let address = u64_at(operation, Self::ADDRESS)?;
        let size = u64_at(operation, Self::SIZE)?;
        let third = u64_at(operation, Self::THIRD)?;
        Ok(match kind {
            Self::KIND_MAP | Self::KIND_MAP_PROTECTED => {
                if operation.len() < Self::FULL_SIZE {
                    return Err(Malformed);
                }
                let offset = u64_at(operation, Self::OFFSET)?;
                let driver_protection = match kind {
                    Self::KIND_MAP_PROTECTED => Some(u64_at(operation, Self::DRIVER_PROTECTION)?),
                    _ => None,
                };
                Self::Map { address, size, allocation: third as u32, offset, driver_protection }
            }
            Self::KIND_UNMAP => Self::Unmap { address, size },
            Self::KIND_COPY => Self::Copy { source: address, size, destination: third },
            other => Self::Unknown(other),
        })
    }
}

/// Which pool of memory a usage query is about: zero is video memory.
pub fn memory_group(message: &[u8]) -> Result<u32, Malformed> {
    u32_at(message, 0x1C)
}

/// Answers a usage query: the budget, what's in use, what this process has reserved, and how much
/// it may reserve.
pub fn memory_answer(budget: u64, usage: u64, reserved: u64, out: &mut Vec<u8>) {
    put_u64(out, budget);
    put_u64(out, usage);
    put_u64(out, reserved);
    put_u64(out, available_for_reservation(budget));
}

/// How much of a budget a process may reserve.
pub fn available_for_reservation(budget: u64) -> u64 {
    budget / 2
}

/// A process asking that some memory be kept for it, which `SetVideoMemoryReservation` sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationRequest {
    /// Zero is video memory, as in a usage query.
    pub group: u32,
    pub bytes: u64,
}

impl ReservationRequest {
    const GROUP: usize = 0x24;
    const BYTES: usize = 0x28;

    pub fn parse(message: &[u8]) -> Result<Self, Malformed> {
        Ok(Self { group: u32_at(message, Self::GROUP)?, bytes: u64_at(message, Self::BYTES)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::set_u64;

    #[test]
    fn map_operation_reads_its_allocation_from_the_low_half() {
        let mut bytes = [0; 64];
        bytes[0] = 3;
        set_u64(&mut bytes, 8, 0x1000);
        set_u64(&mut bytes, 16, 0x2000);
        set_u64(&mut bytes, 24, 0xAAAA_0000_0040);
        set_u64(&mut bytes, 32, 0x10);
        let expected = UpdateOperation::Map {
            address: 0x1000,
            size: 0x2000,
            allocation: 0x40,
            offset: 0x10,
            driver_protection: Some(0),
        };
        assert_eq!(UpdateOperation::parse(&bytes), Ok(expected));
    }

    #[test]
    fn only_a_protected_map_has_the_drivers_protection() {
        let mut bytes = [0; 64];
        set_u64(&mut bytes, 0x38, 0x0300);
        let mut driver_protection = |kind: u8| {
            bytes[0] = kind;
            match UpdateOperation::parse(&bytes) {
                Ok(UpdateOperation::Map { driver_protection, .. }) => driver_protection,
                other => panic!("not a map: {other:?}"),
            }
        };
        assert_eq!(driver_protection(3), Some(0x0300));
        assert_eq!(driver_protection(0), None);
    }

    #[test]
    fn a_cut_short_map_is_malformed_but_an_unmap_is_not() {
        let mut bytes = [0; 32];
        assert!(UpdateOperation::parse(&bytes).is_err());
        bytes[0] = 1;
        assert!(UpdateOperation::parse(&bytes).is_ok());
    }

    #[test]
    fn unknown_operations_are_kept_for_the_log() {
        let mut bytes = [0; 64];
        bytes[0] = 9;
        assert_eq!(UpdateOperation::parse(&bytes), Ok(UpdateOperation::Unknown(9)));
    }

    #[test]
    fn answer_shapes() {
        let mut out = Vec::new();
        two_values_and_status(1, 2, Status::SUCCESS, &mut out);
        assert_eq!(out.len(), 24);
        let mut out = Vec::new();
        memory_answer(100, 7, 0, &mut out);
        assert_eq!(u64_at(&out, 24), Ok(50));
    }

    #[test]
    fn a_reservation_reads_its_group_and_size() {
        let mut bytes = [0; 0x38];
        bytes[0x24] = 1;
        set_u64(&mut bytes, 0x28, 0x4000_0000);
        assert_eq!(
            ReservationRequest::parse(&bytes),
            Ok(ReservationRequest { group: 1, bytes: 0x4000_0000 })
        );
    }
}
