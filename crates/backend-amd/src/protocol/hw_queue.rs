//! A new hardware queue's private data, UMDKMDIF_CREATEHWQUEUE_PRIVATE_DATA in AMD's open compute
//! client.
//!
//! What is known of it is what the kernel driver reads to build a queue it schedules itself, from
//! the guest's buffer as it came. Several of its fields point into the guest process's own memory,
//! which the host can't follow, so those are only checked for being set. A queue the guest would
//! feed from user mode is made by a different path in the kernel driver, which writes a doorbell
//! back. Nothing here backs such a queue, so no doorbell is written. The AQL queues the compute
//! runtime asks for are refused, as they need a compute engine this backend doesn't give the guest
//! yet.

use super::bytes::qword;

/// The highest field the kernel driver reads is a dword at 0x5C.
const LEAST_SIZE: usize = 0x60;
/// A pointer to the guest's queue description, which must be set.
const DESCRIPTION_POINTER: usize = 0x30;
/// A byte, nonzero for an AQL queue.
const AQL_MARK: usize = 0x58;

/// Why the kernel driver, or this backend, would refuse this queue, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// Checks a queue's private data. Nothing is written back.
pub fn check(data: &[u8]) -> Result<(), Refused> {
    if data.len() < LEAST_SIZE {
        return Err(Refused("the private data is too short"));
    }
    if qword(data, DESCRIPTION_POINTER) == Some(0) {
        return Err(Refused("the queue has no description"));
    }
    if data[AQL_MARK] != 0 {
        return Err(Refused("AQL queues aren't offered"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::put_qword;

    fn queue() -> Vec<u8> {
        let mut data = vec![0u8; LEAST_SIZE];
        put_qword(&mut data, DESCRIPTION_POINTER, 0x7FF0_0000_1000);
        data
    }

    #[test]
    fn a_described_queue_is_taken_and_left_as_it_came() {
        let data = queue();
        let before = data.clone();
        assert_eq!(check(&data), Ok(()));
        assert_eq!(data, before);
    }

    #[test]
    fn the_aql_mark_is_one_byte() {
        let mut data = queue();
        data[AQL_MARK + 1..].fill(0xFF);
        assert_eq!(check(&data), Ok(()));
    }

    #[test]
    fn short_undescribed_and_aql_queues_are_refused() {
        assert_eq!(check(&queue()[..LEAST_SIZE - 1]), Err(Refused("the private data is too short")));
        let mut empty = queue();
        put_qword(&mut empty, DESCRIPTION_POINTER, 0);
        assert_eq!(check(&empty), Err(Refused("the queue has no description")));
        let mut aql = queue();
        aql[AQL_MARK] = 1;
        assert_eq!(check(&aql), Err(Refused("AQL queues aren't offered")));
    }
}
