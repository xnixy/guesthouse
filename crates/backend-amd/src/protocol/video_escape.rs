//! The three escapes of a video session, which AMD's kernel driver answers from its own books: none
//! of them reaches the hardware.
//!
//! The guest's video driver sends them from user mode under service 3, around a decoder's or an
//! encoder's life. Their payload is not the counted input and output of [`super::escape`]. It is one
//! block of [`BLOCK_BYTES`] bytes that starts with its own size, and for [`THROUGHPUT`] a second
//! block of that size after it, which the answer goes in.
//!
//! - [`SESSION_START`] comes once the context on the codec node is made. The driver gives the
//!   session an ID at [`SESSION`]: a count that starts at one and is never zero.
//! - [`THROUGHPUT`] keeps a sum for each engine of what its sessions need, in macroblocks a second.
//!   Its kind says what changes: [`KIND_BOOK`] adds the session's demand at [`DEMAND`],
//!   [`KIND_RELEASE`] takes it off again, and [`KIND_ASK`] changes nothing. Every kind is answered
//!   with the engine's sum at [`BOOKED`] and the most the chip takes at [`MOST`]. The video runtime
//!   refuses to encode when it reads a most of zero.
//! - [`SESSION_END`] comes when the session is over, with its ID at [`SESSION`], and the driver
//!   forgets it.
//!
//! This module only reads the three and writes their answers. The books are in [`crate::video`].

use super::bytes::{dword, put_dword};
use super::escape::{EscapeCode, USER_MODE_CLIENT};

/// The service all three are sent under.
const SERVICE: u32 = 3;
pub const SESSION_START: u32 = 0x4D;
pub const SESSION_END: u32 = 0x4E;
pub const THROUGHPUT: u32 = 0xAC;

/// One block of the payload, and what its first dword says.
const BLOCK_BYTES: usize = 0x40;
const SIZE: usize = 0x00;
/// The session's ID: written here in the answer to [`SESSION_START`], and sent here with the other
/// two.
const SESSION: usize = 0x04;
/// What the session needs, with [`KIND_BOOK`].
const DEMAND: usize = 0x08;
/// AMD's number for the engine, as the adapter block gives it for the codec node.
const ENGINE: usize = 0x0C;
const KIND: usize = 0x10;
const KIND_BOOK: u32 = 0;
const KIND_RELEASE: u32 = 1;
const KIND_ASK: u32 = 2;
/// The answer to [`THROUGHPUT`], at the start of the second block.
const BOOKED: usize = BLOCK_BYTES;
const MOST: usize = BLOCK_BYTES + 0x04;

/// What a [`THROUGHPUT`] escape changes for its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The session needs this much from here on.
    Book(u32),
    /// The session needs nothing any more.
    Release,
    /// Nothing changes: the sum and the most are only asked for.
    Ask,
}

/// What the guest's driver asks with one of the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// A session starts and is to be given its ID.
    Start,
    /// The session with this ID is over.
    End {
        session: u32,
    },
    Throughput {
        session: u32,
        engine: u32,
        change: Change,
    },
}

/// Reads one of the three from an escape's code and its payload. `None` for any other escape, and
/// for one of these that isn't shaped as the guest's driver sends it.
pub fn read(code: EscapeCode, payload: &[u8]) -> Option<Asked> {
    if code.service != SERVICE || code.client != USER_MODE_CLIENT {
        return None;
    }
    let blocks = if code.subfunction == THROUGHPUT { 2 } else { 1 };
    if payload.len() < blocks * BLOCK_BYTES || dword(payload, SIZE) != Some(BLOCK_BYTES as u32) {
        return None;
    }
    let session = dword(payload, SESSION)?;
    match code.subfunction {
        SESSION_START => Some(Asked::Start),
        SESSION_END => Some(Asked::End { session }),
        THROUGHPUT => {
            let change = match dword(payload, KIND)? {
                KIND_BOOK => Change::Book(dword(payload, DEMAND)?),
                KIND_RELEASE => Change::Release,
                KIND_ASK => Change::Ask,
                _ => return None,
            };
            Some(Asked::Throughput { session, engine: dword(payload, ENGINE)?, change })
        }
        _ => None,
    }
}

/// Writes a new session's ID into the payload of a [`SESSION_START`] that [`read`] took.
pub fn give_session(payload: &mut [u8], session: u32) {
    put_dword(payload, SESSION, session);
}

/// Writes an engine's sum and the most it takes into the payload of a [`THROUGHPUT`] that [`read`]
/// took. The rest of the answer's block is left as it came.
pub fn give_throughput(payload: &mut [u8], booked: u32, most: u32) {
    put_dword(payload, BOOKED, booked);
    put_dword(payload, MOST, most);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const CODEC_ENGINE: u32 = 41;

    fn code(subfunction: u32) -> EscapeCode {
        EscapeCode { service: SERVICE, subfunction, client: USER_MODE_CLIENT }
    }

    pub(crate) fn start() -> (EscapeCode, Vec<u8>) {
        let mut payload = vec![0u8; BLOCK_BYTES];
        put_dword(&mut payload, SIZE, BLOCK_BYTES as u32);
        (code(SESSION_START), payload)
    }

    pub(crate) fn end(session: u32) -> (EscapeCode, Vec<u8>) {
        let (_, mut payload) = start();
        put_dword(&mut payload, SESSION, session);
        (code(SESSION_END), payload)
    }

    /// A throughput escape, its answer's block filled with a mark so that what an answer writes
    /// shows.
    pub(crate) fn throughput(session: u32, change: Change) -> (EscapeCode, Vec<u8>) {
        let (kind, demand) = match change {
            Change::Book(demand) => (KIND_BOOK, demand),
            Change::Release => (KIND_RELEASE, 0),
            Change::Ask => (KIND_ASK, 0),
        };
        let mut payload = vec![0u8; 2 * BLOCK_BYTES];
        put_dword(&mut payload, SIZE, BLOCK_BYTES as u32);
        put_dword(&mut payload, SESSION, session);
        put_dword(&mut payload, DEMAND, demand);
        put_dword(&mut payload, ENGINE, CODEC_ENGINE);
        put_dword(&mut payload, KIND, kind);
        payload[BLOCK_BYTES..].fill(0xCC);
        (code(THROUGHPUT), payload)
    }

    /// The sum and the most an answer to a throughput escape holds.
    pub(crate) fn answered(payload: &[u8]) -> (u32, u32) {
        (dword(payload, BOOKED).unwrap(), dword(payload, MOST).unwrap())
    }

    pub(crate) fn session_given(payload: &[u8]) -> u32 {
        dword(payload, SESSION).unwrap()
    }

    #[test]
    fn the_three_are_read_as_the_guest_sends_them() {
        let (code, payload) = start();
        assert_eq!(read(code, &payload), Some(Asked::Start));
        let (code, payload) = end(7);
        assert_eq!(read(code, &payload), Some(Asked::End { session: 7 }));
        for change in [Change::Book(209_635), Change::Release, Change::Ask] {
            let (code, payload) = throughput(7, change);
            let asked = Asked::Throughput { session: 7, engine: CODEC_ENGINE, change };
            assert_eq!(read(code, &payload), Some(asked));
        }
        let (code, mut payload) = throughput(7, Change::Ask);
        put_dword(&mut payload, KIND, 3);
        assert_eq!(read(code, &payload), None, "a kind nobody has sent");
    }

    #[test]
    fn another_escape_or_one_under_another_service_or_client_is_left_alone() {
        let (start_code, payload) = start();
        for other in [
            EscapeCode { subfunction: 0x4C, ..start_code },
            EscapeCode { service: 2, ..start_code },
            EscapeCode { client: 5, ..start_code },
        ] {
            assert_eq!(read(other, &payload), None, "{other:?}");
        }
    }

    #[test]
    fn a_payload_that_is_cut_short_or_gives_another_size_is_left_alone() {
        let whole = [start(), end(1), throughput(1, Change::Ask)];
        for (code, payload) in whole {
            assert!(read(code, &payload).is_some());
            assert_eq!(read(code, &payload[..payload.len() - 1]), None, "{code:?}");
            assert_eq!(read(code, &[]), None);
            let mut other_size = payload.clone();
            put_dword(&mut other_size, SIZE, 0x44);
            assert_eq!(read(code, &other_size), None, "{code:?}");
        }
        // A throughput escape needs its second block, where the answer goes.
        let (code, payload) = throughput(1, Change::Ask);
        assert_eq!(read(code, &payload[..BLOCK_BYTES]), None);
    }

    #[test]
    fn an_answer_writes_only_its_own_dwords() {
        let (_, mut payload) = start();
        give_session(&mut payload, 3);
        assert_eq!(session_given(&payload), 3);
        assert_eq!(dword(&payload, SIZE), Some(0x40));
        assert!(payload[8..].iter().all(|&byte| byte == 0));

        let (_, mut payload) = throughput(3, Change::Book(100));
        let before = payload.clone();
        give_throughput(&mut payload, 100, 2_937_600);
        assert_eq!(answered(&payload), (100, 2_937_600));
        assert_eq!(payload[..BLOCK_BYTES], before[..BLOCK_BYTES]);
        assert!(payload[BLOCK_BYTES + 8..].iter().all(|&byte| byte == 0xCC));
    }
}
