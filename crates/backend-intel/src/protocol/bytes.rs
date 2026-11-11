//! Little-endian reads and writes over the guest's buffers.
//!
//! Reads return `None` past the end, as a guest buffer can be any length. Writes are only made into
//! buffers this backend sized itself, so they index directly.

pub fn byte(data: &[u8], at: usize) -> Option<u8> {
    data.get(at).copied()
}

pub fn word(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes(bytes.try_into().expect("two bytes")))
}

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

/// Bits `low..low + width` of a dword.
pub fn bits(value: u32, low: u32, width: u32) -> u32 {
    (value >> low) & ((1 << width) - 1)
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
        assert_eq!(word(&data, 4), None);
        assert_eq!(byte(&data, 4), Some(2));
        assert_eq!(dword(&data, usize::MAX - 1), None);
    }

    #[test]
    fn a_field_of_bits_is_cut_out_of_its_dword() {
        assert_eq!(bits(0x0007_0000 | 0x23D7, 16, 6), 7);
        assert_eq!(bits(0xFFFF_FFFF, 27, 5), 0x1F);
    }
}
