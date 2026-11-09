//! A graphics submission's private data: a short header, then a list of records, each with an
//! opcode and its own length.
//!
//! The guest computes all of it and the kernel driver only checks it, so the check refuses what
//! the kernel would and nothing is written back. The compute runtime's AQL and SDMA submissions
//! have no private data of this kind.

use super::adapter_info::SUBMISSION_CHECKSUM_VERSION;
use super::bytes::{dword, index_xor_sum, qword};

const HEADER_SIZE: usize = 8;
const CLIENT: usize = 0x00;
const RECORD_COUNT: usize = 0x04;
/// Client ids run from one to this. Two is the D3D runtime's. The kernel driver asks that its first
/// record be a [`CHECKSUM`] only on a condition it reads from its own state, not from the
/// submission, so that rule isn't checked here.
const LAST_CLIENT: u32 = 10;
/// With no records and nothing past the header, this client id marks an empty submission.
const EMPTY_SUBMISSION_CLIENT: u32 = 0xAA;

// A record, by offset from its start.
const OPCODE: usize = 0x00;
/// Bytes in the record, these eight of opcode and length included.
const LENGTH: usize = 0x04;
const RECORD_HEADER_SIZE: usize = 8;

/// A user-mode DMA buffer.
const DMA_BUFFER: u32 = 0;
const DMA_BUFFER_LENGTH: u32 = 0x40;
/// The buffer's GPU address, which must be set.
const DMA_BUFFER_ADDRESS: usize = 0x18;
/// An indirect buffer.
const INDIRECT_BUFFER: u32 = 1;
const INDIRECT_BUFFER_LENGTH: u32 = 0x40;
/// Address mappings to change along with the submission.
const MAPPING_UPDATE: u32 = 3;
/// A size field inside the record, which must be [`MAPPING_UPDATE_INNER_SIZE`].
const MAPPING_UPDATE_INNER: usize = 0x08;
const MAPPING_UPDATE_INNER_SIZE: u32 = 0x40;
const MAPPING_UPDATE_COUNT: usize = 0x30;
const MAPPING_UPDATE_FIXED: u32 = 0x34;
/// Bytes per mapping.
const MAPPING_UPDATE_ENTRY: u32 = 20;
/// WDDM 2 command information.
const WDDM2_INFO: u32 = 4;
const WDDM2_INFO_LENGTH: u32 = 0x48;
/// The guest's checksum of the submission.
const CHECKSUM: u32 = 5;
const CHECKSUM_LENGTH: u32 = 0x40;
/// The guest's [`index_xor_sum`]. Zero is taken unchecked.
const CHECKSUM_VALUE: usize = 0x08;
/// Bytes the version-0 sum covers past the record's own [`CHECKSUM_LENGTH`], from the record's
/// start. The sum runs over a quarter of that many dwords, which is medium confidence.
const CHECKSUM_COVERED: usize = 0x0C;
const CHECKSUM_VERSION: usize = 0x10;
/// Version 0 sums the submission's own bytes from the record on. Version 1 sums a list the kernel
/// gathers from the DMA buffer records, which is taken unchecked here.
const CHECKSUM_OVER_BYTES: u32 = 0;
/// Work for several engines at once.
const GANG: u32 = 6;
const GANG_VERSION: usize = 0x08;
const GANG_VERSIONS: std::ops::RangeInclusive<u32> = 1..=2;
const GANG_COUNT: usize = 0x10;
/// The record's length with one member, and the length each further member adds.
const GANG_FIRST_LENGTH: u32 = 0x28;
const GANG_MEMBER: u32 = 0x10;
/// Tracking information from the user-mode driver.
const TRACKING: u32 = 8;
const TRACKING_LENGTH: u32 = 0x20;
/// Taken with no check of its length.
const UNCHECKED: u32 = 9;

/// Why the kernel driver would refuse this submission, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// What a valid submission holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub client: u32,
    pub opcodes: Vec<u32>,
}

fn check_record(record: &[u8], opcode: u32) -> Result<(), Refused> {
    let length = record.len() as u32;
    let field = |at: usize| dword(record, at).ok_or(Refused("a record is too short for its fields"));
    let exact =
        |wanted: u32| if length == wanted { Ok(()) } else { Err(Refused("a record has the wrong length")) };
    match opcode {
        DMA_BUFFER => {
            exact(DMA_BUFFER_LENGTH)?;
            if qword(record, DMA_BUFFER_ADDRESS).ok_or(Refused("a record is too short for its fields"))? == 0
            {
                return Err(Refused("a DMA buffer has no address"));
            }
            Ok(())
        }
        INDIRECT_BUFFER => exact(INDIRECT_BUFFER_LENGTH),
        MAPPING_UPDATE => {
            if field(MAPPING_UPDATE_INNER)? != MAPPING_UPDATE_INNER_SIZE {
                return Err(Refused("a mapping update has the wrong inner size"));
            }
            let count = field(MAPPING_UPDATE_COUNT)?;
            let wanted = count
                .checked_mul(MAPPING_UPDATE_ENTRY)
                .and_then(|bytes| bytes.checked_add(MAPPING_UPDATE_FIXED));
            exact(wanted.ok_or(Refused("a record has the wrong length"))?)
        }
        WDDM2_INFO => exact(WDDM2_INFO_LENGTH),
        CHECKSUM => {
            exact(CHECKSUM_LENGTH)?;
            if field(CHECKSUM_VERSION)? > SUBMISSION_CHECKSUM_VERSION {
                return Err(Refused("the checksum version is unknown"));
            }
            Ok(())
        }
        GANG => {
            if !GANG_VERSIONS.contains(&field(GANG_VERSION)?) {
                return Err(Refused("the gang version is unknown"));
            }
            let count = field(GANG_COUNT)?;
            if count == 0 {
                return Err(Refused("a gang has no members"));
            }
            let wanted =
                (count - 1).checked_mul(GANG_MEMBER).and_then(|bytes| bytes.checked_add(GANG_FIRST_LENGTH));
            exact(wanted.ok_or(Refused("a record has the wrong length"))?)
        }
        TRACKING => exact(TRACKING_LENGTH),
        UNCHECKED => Ok(()),
        _ => Err(Refused("the opcode is unknown")),
    }
}

/// Checks a version-0 checksum. `rest` is the submission from the checksum record to its end, and
/// the record itself has been checked.
fn check_sum(rest: &[u8]) -> Result<(), Refused> {
    let field = |at: usize| dword(rest, at).expect("inside a checked record");
    let stored = field(CHECKSUM_VALUE);
    if field(CHECKSUM_VERSION) != CHECKSUM_OVER_BYTES || stored == 0 {
        return Ok(());
    }
    let covered = (field(CHECKSUM_COVERED) as usize).checked_add(CHECKSUM_LENGTH as usize);
    let covered = covered.filter(|&covered| covered <= rest.len());
    let covered = covered.ok_or(Refused("the checksum covers more than was sent"))?;
    if index_xor_sum(rest, covered / 4, CHECKSUM_VALUE) != stored {
        return Err(Refused("the checksum doesn't match"));
    }
    Ok(())
}

/// Checks a submission's private data as the kernel driver would.
pub fn check(data: &[u8]) -> Result<Submission, Refused> {
    let header = |at: usize| dword(data, at).ok_or(Refused("the header is cut short"));
    let client = header(CLIENT)?;
    let count = header(RECORD_COUNT)?;
    if client == EMPTY_SUBMISSION_CLIENT && count == 0 && data.len() == HEADER_SIZE {
        return Ok(Submission { client, opcodes: Vec::new() });
    }
    if !(1..=LAST_CLIENT).contains(&client) {
        return Err(Refused("the client id is out of range"));
    }
    if count == 0 {
        return Err(Refused("there are no records"));
    }
    let mut opcodes = Vec::new();
    let mut at = HEADER_SIZE;
    for _ in 0..count {
        let field = |offset: usize| dword(data, at + offset).ok_or(Refused("a record runs past the end"));
        let opcode = field(OPCODE)?;
        let length = field(LENGTH)? as usize;
        let end = at.checked_add(length).filter(|&end| length >= RECORD_HEADER_SIZE && end <= data.len());
        let end = end.ok_or(Refused("a record runs past the end"))?;
        check_record(&data[at..end], opcode)?;
        if opcode == CHECKSUM {
            check_sum(&data[at..])?;
        }
        opcodes.push(opcode);
        at = end;
    }
    Ok(Submission { client, opcodes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::{put_dword, put_qword};

    fn record(opcode: u32, length: u32) -> Vec<u8> {
        let mut data = vec![0u8; length as usize];
        put_dword(&mut data, OPCODE, opcode);
        put_dword(&mut data, LENGTH, length);
        data
    }

    fn checksum() -> Vec<u8> {
        let mut data = record(CHECKSUM, CHECKSUM_LENGTH);
        put_dword(&mut data, CHECKSUM_VERSION, 1);
        data
    }

    /// A submission led by a version-0 checksum over itself and every record after it.
    fn summed(client: u32, records: &[Vec<u8>]) -> Vec<u8> {
        let mut first = record(CHECKSUM, CHECKSUM_LENGTH);
        let after: usize = records.iter().map(Vec::len).sum();
        put_dword(&mut first, CHECKSUM_COVERED, after as u32);
        let mut all = vec![first];
        all.extend_from_slice(records);
        let mut data = submission(client, &all);
        let sum = index_xor_sum(&data[HEADER_SIZE..], (CHECKSUM_LENGTH as usize + after) / 4, CHECKSUM_VALUE);
        put_dword(&mut data, HEADER_SIZE + CHECKSUM_VALUE, sum);
        data
    }

    fn dma_buffer(address: u64) -> Vec<u8> {
        let mut data = record(DMA_BUFFER, DMA_BUFFER_LENGTH);
        put_qword(&mut data, DMA_BUFFER_ADDRESS, address);
        data
    }

    fn gang(count: u32) -> Vec<u8> {
        let mut data = record(GANG, GANG_FIRST_LENGTH + count.saturating_sub(1) * GANG_MEMBER);
        put_dword(&mut data, GANG_VERSION, 2);
        put_dword(&mut data, GANG_COUNT, count);
        data
    }

    fn mapping_update(count: u32) -> Vec<u8> {
        let mut data = record(MAPPING_UPDATE, MAPPING_UPDATE_FIXED + count * MAPPING_UPDATE_ENTRY);
        put_dword(&mut data, MAPPING_UPDATE_INNER, MAPPING_UPDATE_INNER_SIZE);
        put_dword(&mut data, MAPPING_UPDATE_COUNT, count);
        data
    }

    fn submission(client: u32, records: &[Vec<u8>]) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_SIZE];
        put_dword(&mut data, CLIENT, client);
        put_dword(&mut data, RECORD_COUNT, records.len() as u32);
        records.iter().for_each(|record| data.extend_from_slice(record));
        data
    }

    fn refused(data: &[u8]) -> &'static str {
        check(data).unwrap_err().0
    }

    #[test]
    fn a_usual_submission_is_taken() {
        let records =
            [checksum(), dma_buffer(0x1_0000), record(INDIRECT_BUFFER, 0x40), record(WDDM2_INFO, 0x48)];
        let submission = check(&submission(1, &records)).unwrap();
        assert_eq!(submission.opcodes, [CHECKSUM, DMA_BUFFER, INDIRECT_BUFFER, WDDM2_INFO]);
    }

    #[test]
    fn a_version_0_checksum_is_checked_over_the_records_it_covers() {
        let data = summed(2, &[dma_buffer(0x1_0000), record(INDIRECT_BUFFER, 0x40)]);
        assert!(check(&data).is_ok());
        let mut changed = data.clone();
        let last = changed.len() - 4;
        changed[last] ^= 1;
        assert_eq!(refused(&changed), "the checksum doesn't match");
        let mut too_far = data;
        let covered = dword(&too_far, HEADER_SIZE + CHECKSUM_COVERED).unwrap();
        put_dword(&mut too_far, HEADER_SIZE + CHECKSUM_COVERED, covered + 4);
        assert_eq!(refused(&too_far), "the checksum covers more than was sent");
    }

    #[test]
    fn a_zero_or_version_1_checksum_is_not_summed() {
        let mut zero = summed(2, &[dma_buffer(0x1_0000)]);
        put_dword(&mut zero, HEADER_SIZE + CHECKSUM_VALUE, 0);
        assert!(check(&zero).is_ok());
        let mut gathered = summed(2, &[dma_buffer(0x1_0000)]);
        put_dword(&mut gathered, HEADER_SIZE + CHECKSUM_VERSION, 1);
        assert!(check(&gathered).is_ok());
    }

    #[test]
    fn the_checksum_version_is_bounded_by_what_the_adapter_block_publishes() {
        let mut newest = checksum();
        put_dword(&mut newest, CHECKSUM_VERSION, SUBMISSION_CHECKSUM_VERSION);
        assert!(check(&submission(1, &[newest.clone()])).is_ok());
        put_dword(&mut newest, CHECKSUM_VERSION, SUBMISSION_CHECKSUM_VERSION + 1);
        assert_eq!(refused(&submission(1, &[newest])), "the checksum version is unknown");
    }

    #[test]
    fn a_mapping_update_takes_twenty_bytes_a_mapping() {
        assert_eq!(mapping_update(2).len(), 0x34 + 2 * 20);
        assert!(check(&submission(1, &[mapping_update(2)])).is_ok());
        // Four bytes a mapping, as one earlier reading had it, is refused.
        let mut narrow = record(MAPPING_UPDATE, 0x34 + 2 * 4);
        put_dword(&mut narrow, MAPPING_UPDATE_INNER, MAPPING_UPDATE_INNER_SIZE);
        put_dword(&mut narrow, MAPPING_UPDATE_COUNT, 2);
        assert_eq!(refused(&submission(1, &[narrow])), "a record has the wrong length");
    }

    #[test]
    fn every_known_record_is_taken() {
        let records = [
            checksum(),
            mapping_update(3),
            gang(1),
            gang(4),
            record(TRACKING, TRACKING_LENGTH),
            record(UNCHECKED, 0x18),
        ];
        assert!(check(&submission(10, &records)).is_ok());
    }

    #[test]
    fn the_empty_marker_is_taken_only_alone() {
        assert!(check(&submission(EMPTY_SUBMISSION_CLIENT, &[])).unwrap().opcodes.is_empty());
        let mut padded = submission(EMPTY_SUBMISSION_CLIENT, &[]);
        padded.extend_from_slice(&[0; 4]);
        assert_eq!(refused(&padded), "the client id is out of range");
    }

    #[test]
    fn a_bad_header_is_refused() {
        assert_eq!(refused(&[1, 0, 0]), "the header is cut short");
        assert_eq!(refused(&submission(0, &[checksum()])), "the client id is out of range");
        assert_eq!(refused(&submission(11, &[checksum()])), "the client id is out of range");
        assert_eq!(refused(&submission(1, &[])), "there are no records");
    }

    #[test]
    fn unknown_and_unused_opcodes_are_refused() {
        for opcode in [2, 7, 10, u32::MAX] {
            assert_eq!(refused(&submission(1, &[record(opcode, 0x10)])), "the opcode is unknown");
        }
    }

    #[test]
    fn records_must_have_their_lengths() {
        assert_eq!(refused(&submission(1, &[record(CHECKSUM, 0x48)])), "a record has the wrong length");
        assert_eq!(refused(&submission(1, &[record(TRACKING, 0x28)])), "a record has the wrong length");
        let mut short_gang = gang(3);
        short_gang.truncate(short_gang.len() - GANG_MEMBER as usize);
        let length = short_gang.len() as u32;
        put_dword(&mut short_gang, LENGTH, length);
        assert_eq!(refused(&submission(1, &[short_gang])), "a record has the wrong length");
        let mut long_update = mapping_update(1);
        put_dword(&mut long_update, MAPPING_UPDATE_COUNT, 2);
        assert_eq!(refused(&submission(1, &[long_update])), "a record has the wrong length");
    }

    #[test]
    fn record_fields_are_checked() {
        assert_eq!(refused(&submission(1, &[dma_buffer(0)])), "a DMA buffer has no address");
        assert!(check(&submission(1, &[dma_buffer(1 << 40)])).is_ok());
        let mut version = checksum();
        put_dword(&mut version, CHECKSUM_VERSION, 2);
        assert_eq!(refused(&submission(1, &[version])), "the checksum version is unknown");
        let mut gang_version = gang(1);
        put_dword(&mut gang_version, GANG_VERSION, 3);
        assert_eq!(refused(&submission(1, &[gang_version])), "the gang version is unknown");
        let mut no_members = gang(1);
        put_dword(&mut no_members, GANG_COUNT, 0);
        assert_eq!(refused(&submission(1, &[no_members])), "a gang has no members");
        let mut inner = mapping_update(0);
        put_dword(&mut inner, MAPPING_UPDATE_INNER, 0x20);
        assert_eq!(refused(&submission(1, &[inner])), "a mapping update has the wrong inner size");
    }

    #[test]
    fn records_can_not_run_past_the_end_or_loop() {
        let mut data = submission(1, &[checksum()]);
        put_dword(&mut data, RECORD_COUNT, 2);
        assert_eq!(refused(&data), "a record runs past the end");
        let mut zero_length = submission(1, &[record(UNCHECKED, 0x10)]);
        put_dword(&mut zero_length, HEADER_SIZE + LENGTH, 0);
        assert_eq!(refused(&zero_length), "a record runs past the end");
        let mut huge = submission(1, &[record(UNCHECKED, 0x10)]);
        put_dword(&mut huge, HEADER_SIZE + LENGTH, u32::MAX);
        assert_eq!(refused(&huge), "a record runs past the end");
    }

    #[test]
    fn a_record_too_short_for_its_fields_is_refused_not_read_past() {
        let mut update = record(MAPPING_UPDATE, 0x10);
        put_dword(&mut update, MAPPING_UPDATE_INNER, MAPPING_UPDATE_INNER_SIZE);
        assert_eq!(refused(&submission(1, &[update])), "a record is too short for its fields");
        let mut gang = record(GANG, 0x10);
        put_dword(&mut gang, GANG_VERSION, 1);
        assert_eq!(refused(&submission(1, &[gang])), "a record is too short for its fields");
        assert_eq!(refused(&submission(1, &[record(DMA_BUFFER, 0x10)])), "a record has the wrong length");
    }
}
