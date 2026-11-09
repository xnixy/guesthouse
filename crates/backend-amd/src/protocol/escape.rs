//! A driver escape's framing: the ATI_LHESCAPE_HDR header from AMD's open debugger header, and the
//! escape code inside it.
//!
//! The code packs a service, a subfunction and a client: `(service << 24) | (subfunction << 8) |
//! client`. The kernel driver rewrites a few codes before it dispatches them, and answers in the
//! header's ulEscapeReturnCode. No subfunction's own payload is known yet, so every escape that is
//! framed right is answered as failed, which the guest's driver can fall back from. One answered as
//! done would leave the guest reading a payload nobody filled in.

use super::bytes::{dword, put_dword};

/// sizeof(ATI_LHESCAPE_HDR). The payload, if any, follows it.
pub const HEADER_SIZE: usize = 0x80;

// ATI_LHESCAPE_HDR.
const SIZE: usize = 0x00;
/// ulHeaderVersion: major in the high half, minor in the low.
const HEADER_VERSION: usize = 0x04;
/// The oldest header the kernel takes, 1.0.
const LEAST_HEADER_VERSION: u32 = 0x0001_0000;
const ESCAPE_CODE: usize = 0x08;
/// ulEscapeReturnCode.
const RETURN_CODE: usize = 0x48;

// The escape code's parts.
const SERVICE_SHIFT: u32 = 24;
const SUBFUNCTION_SHIFT: u32 = 8;
const BYTE: u32 = 0xff;
/// The services the kernel dispatches. Any other fails.
const DISPATCHED_SERVICES: [u32; 5] = [1, 2, 3, 5, 6];
/// One code the kernel answers as another.
const RENAMED_CODE: (u32, u32) = (0x0300_0803, 0x0100_0503);
/// Subfunctions of service 1 the kernel moves to service 3.
const MOVED_TO_SERVICE_3: [u32; 2] = [0x26, 0x3E];
const SERVICE_1: u32 = 1;
const SERVICE_3: u32 = 3;

/// ulEscapeReturnCode for a failed escape. Zero is done.
const RETURN_FAILED: u32 = 2;

/// Why an escape can't be read, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// An escape's code, as the kernel driver dispatches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscapeCode {
    pub service: u32,
    pub subfunction: u32,
    pub client: u32,
}

impl EscapeCode {
    pub fn from_code(code: u32) -> Self {
        let code = dispatched_code(code);
        Self {
            service: code >> SERVICE_SHIFT & BYTE,
            subfunction: code >> SUBFUNCTION_SHIFT & BYTE,
            client: code & BYTE,
        }
    }

    pub fn is_dispatched(&self) -> bool {
        DISPATCHED_SERVICES.contains(&self.service)
    }
}

/// The code the kernel driver dispatches in place of the one sent.
fn dispatched_code(code: u32) -> u32 {
    if code == RENAMED_CODE.0 {
        return RENAMED_CODE.1;
    }
    let service = code >> SERVICE_SHIFT & BYTE;
    let subfunction = code >> SUBFUNCTION_SHIFT & BYTE;
    if service == SERVICE_1 && MOVED_TO_SERVICE_3.contains(&subfunction) {
        return code & !(BYTE << SERVICE_SHIFT) | SERVICE_3 << SERVICE_SHIFT;
    }
    code
}

/// Reads an escape's header.
pub fn parse(data: &[u8]) -> Result<EscapeCode, Refused> {
    if data.len() < HEADER_SIZE {
        return Err(Refused("shorter than its header"));
    }
    let field = |at: usize| dword(data, at).expect("inside the header");
    if (field(SIZE) as usize) < HEADER_SIZE {
        return Err(Refused("its size is less than its header"));
    }
    if field(HEADER_VERSION) < LEAST_HEADER_VERSION {
        return Err(Refused("its header is too old"));
    }
    Ok(EscapeCode::from_code(field(ESCAPE_CODE)))
}

/// Answers an escape in place. Returns its code, or why it couldn't be read; an escape too short
/// for its header has nowhere to put an answer and goes back untouched.
pub fn answer(data: &mut [u8]) -> Result<EscapeCode, Refused> {
    let parsed = parse(data);
    if data.len() >= HEADER_SIZE {
        put_dword(data, RETURN_CODE, RETURN_FAILED);
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escape(code: u32) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_SIZE + 0x20];
        put_dword(&mut data, SIZE, (HEADER_SIZE + 0x20) as u32);
        put_dword(&mut data, HEADER_VERSION, LEAST_HEADER_VERSION);
        put_dword(&mut data, ESCAPE_CODE, code);
        data
    }

    #[test]
    fn the_code_splits_into_service_subfunction_and_client() {
        let code = EscapeCode::from_code(0x0200_1203);
        assert_eq!(code, EscapeCode { service: 2, subfunction: 0x12, client: 3 });
        assert!(code.is_dispatched());
        assert!(!EscapeCode::from_code(0x0400_0103).is_dispatched());
        assert!(!EscapeCode::from_code(0x0700_0103).is_dispatched());
    }

    #[test]
    fn renamed_and_moved_codes_are_dispatched_as_the_kernel_does() {
        assert_eq!(
            EscapeCode::from_code(RENAMED_CODE.0),
            EscapeCode { service: 1, subfunction: 0x05, client: 3 }
        );
        assert_eq!(EscapeCode::from_code(0x0100_2603).service, 3);
        assert_eq!(EscapeCode::from_code(0x0100_3E03).service, 3);
        assert_eq!(EscapeCode::from_code(0x0100_2703).service, 1);
        assert_eq!(EscapeCode::from_code(0x0200_2603).service, 2);
    }

    #[test]
    fn a_framed_escape_is_answered_as_failed() {
        let mut data = escape(0x0200_0103);
        let payload_before = data[HEADER_SIZE..].to_vec();
        assert_eq!(answer(&mut data).unwrap().subfunction, 1);
        assert_eq!(dword(&data, RETURN_CODE), Some(RETURN_FAILED));
        assert_eq!(data[HEADER_SIZE..], payload_before[..]);
    }

    #[test]
    fn a_badly_framed_escape_is_refused() {
        let mut short = escape(0x0200_0103);
        short.truncate(HEADER_SIZE - 1);
        let before = short.clone();
        assert_eq!(answer(&mut short), Err(Refused("shorter than its header")));
        assert_eq!(short, before);
        let mut old = escape(0x0200_0103);
        put_dword(&mut old, HEADER_VERSION, 0xFFFF);
        assert_eq!(answer(&mut old), Err(Refused("its header is too old")));
        assert_eq!(dword(&old, RETURN_CODE), Some(RETURN_FAILED));
        let mut small = escape(0x0200_0103);
        put_dword(&mut small, SIZE, 0x40);
        assert_eq!(parse(&small), Err(Refused("its size is less than its header")));
    }
}
