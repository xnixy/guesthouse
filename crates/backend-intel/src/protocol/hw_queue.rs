//! A new hardware queue's private data, CREATEHWQUEUE_PVTDATA: [`SIZE`] bytes.
//!
//! The kernel driver doesn't check its size. It reads the priority and one flag, and writes a
//! 64-bit value back at [`WRITTEN_BACK`] for queues that use the newer native fences, which the
//! guest's driver keeps with the queue. What that value is isn't known yet, so nothing is written
//! and the guest gets back what it sent.

use super::bytes::{dword, qword};

pub const SIZE: usize = 0x18;
const PRIORITY: usize = 0x00;
/// Where the kernel driver writes its value back.
pub const WRITTEN_BACK: usize = 0x08;
const FLAGS: usize = 0x10;
const FLAG_BIT: u32 = 1;

/// The ranges [`Queue::parse`] reads.
pub const DECODED: [(usize, usize); 3] =
    [(PRIORITY, PRIORITY + 4), (WRITTEN_BACK, WRITTEN_BACK + 8), (FLAGS, FLAGS + 4)];

/// What a queue's data says. Fields past the data's end read as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Queue {
    pub priority: u32,
    pub written_back: u64,
    pub flag: bool,
}

impl Queue {
    pub fn parse(data: &[u8]) -> Self {
        Self {
            priority: dword(data, PRIORITY).unwrap_or(0),
            written_back: qword(data, WRITTEN_BACK).unwrap_or(0),
            flag: dword(data, FLAGS).unwrap_or(0) & FLAG_BIT != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::{put_dword, put_qword};

    #[test]
    fn the_three_fields_are_read_and_a_short_queue_reads_as_zero() {
        let mut data = vec![0u8; SIZE];
        put_dword(&mut data, PRIORITY, 1);
        put_qword(&mut data, WRITTEN_BACK, 0x1234);
        put_dword(&mut data, FLAGS, 3);
        assert_eq!(Queue::parse(&data), Queue { priority: 1, written_back: 0x1234, flag: true });
        assert_eq!(Queue::parse(&data[..4]), Queue { priority: 1, ..Queue::default() });
    }
}
