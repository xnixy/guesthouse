//! Little-endian reads and writes over the guest's buffers, and the one checksum AMD's driver uses
//! across several of them.
//!
//! Reads return `None` past the end, as a guest buffer can be any length. Writes are only made into
//! buffers this backend sized itself, or after a length check, so they index directly.

pub fn dword(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

pub fn qword(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
}

pub fn put_word(data: &mut [u8], at: usize, value: u16) {
    data[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn put_dword(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put_qword(data: &mut [u8], at: usize, value: u64) {
    data[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// The sum, over the first `dwords` dwords, of each dword's index XOR its value, with the dword at
/// byte `skip` counted as zero. That dword is where the sum itself is kept, so a buffer checks out
/// when the sum equals what is stored there. A buffer shorter than `dwords` counts its missing
/// dwords as zero.
pub fn index_xor_sum(data: &[u8], dwords: usize, skip: usize) -> u32 {
    (0..dwords).fold(0u32, |sum, index| {
        let at = index * 4;
        let value = if at == skip { 0 } else { dword(data, at).unwrap_or(0) };
        sum.wrapping_add(index as u32 ^ value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_past_the_end_are_none() {
        let data = [1, 0, 0, 0, 2];
        assert_eq!(dword(&data, 0), Some(1));
        assert_eq!(dword(&data, 2), None);
        assert_eq!(qword(&data, 0), None);
        assert_eq!(dword(&data, usize::MAX - 1), None);
    }

    #[test]
    fn the_checksum_leaves_out_its_own_dword() {
        let mut data = vec![0u8; 16];
        put_dword(&mut data, 0, 5);
        put_dword(&mut data, 8, 0xFFFF_FFFF);
        // 0^5 + 1^0 + 2^(skipped) + 3^0
        assert_eq!(index_xor_sum(&data, 4, 8), 5 + 1 + 2 + 3);
    }

    #[test]
    fn a_stored_sum_checks_out() {
        let mut data = vec![0u8; 64];
        for index in 0..16 {
            put_dword(&mut data, index * 4, (index as u32).wrapping_mul(0x0101_0101));
        }
        let sum = index_xor_sum(&data, 16, 12);
        put_dword(&mut data, 12, sum);
        assert_eq!(index_xor_sum(&data, 16, 12), dword(&data, 12).unwrap());
    }
}
