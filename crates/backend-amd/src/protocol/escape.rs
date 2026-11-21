//! A driver escape's framing: the ATI_LHESCAPE_HDR header from AMD's open debugger header, and the
//! escape code inside it.
//!
//! The code packs a service, a subfunction and a client: `(service << 24) | (subfunction << 8) |
//! client`. The kernel driver rewrites a few codes before it dispatches them, and answers in the
//! header's ulEscapeReturnCode, where the guest's user-mode driver takes only zero as done.
//!
//! The user-mode driver sends each escape inside a wrapper of [`WRAPPER_SIZE`] bytes, which starts
//! with [`WRAPPER_KIND`] and [`WRAPPER_VERSION`]. The header and payload follow it unchanged, and the
//! guest copies them back and reads only them, so the wrapper is skipped and left as it came. A bare
//! header starts with its own size, never as little as [`WRAPPER_KIND`], so the two can't be mixed up.
//!
//! The payload after the header is a dword byte count, that many bytes of input, a dword byte count
//! and that many bytes of output. Three escapes the guest's user-mode driver sends at start and
//! can't go on without are answered as done, with an output of zeros and a status index of zero:
//! see [`NEEDED_AT_START`]. No other subfunction's payload is known, so every other escape that is
//! framed right is answered as failed. One answered as done would leave the guest reading a payload
//! nobody filled in. The guest goes on past most failures.

use super::bytes::{dword, put_dword};

/// sizeof(ATI_LHESCAPE_HDR). The payload, if any, follows it.
pub const HEADER_SIZE: usize = 0x80;

/// The wrapper in front of the header. Its size field at 0x48 is not read: it holds the inner size
/// from one sender and the whole size from another, and the inner escape is the rest of the buffer.
pub const WRAPPER_SIZE: usize = 0x4C;
/// The wrapper's first dword.
pub const WRAPPER_KIND: u32 = 2;
/// The wrapper's second dword.
pub const WRAPPER_VERSION: u32 = 0x0001_0002;
const WRAPPER_VERSION_AT: usize = 0x04;

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

/// ulEscapeReturnCode for an escape that is done.
const RETURN_DONE: u32 = 0;
/// A payload's byte counts, ahead of its input and its output.
pub(crate) const COUNT_SIZE: usize = 4;
/// ulEscapeReturnCode for a failed escape. Zero is done, and the guest takes any other value as a
/// failure.
const RETURN_FAILED: u32 = 2;

/// The user-mode driver's client number, in the code's low byte.
pub const USER_MODE_CLIENT: u32 = 3;
/// Service 3 subfunctions the graphics user-mode driver sends at start and can't go on without,
/// with the shape of each one's payload. A failure of the first or the third stops the driver; the
/// second's failure is returned to its caller, and whether that stops it isn't known.
const NEEDED_AT_START: [Needed; 3] = [
    // Six qwords back, with the status index between the fifth and the sixth. Their meaning isn't
    // known. The driver copies them and tests none, and zeros are what the kernel driver's own
    // records start as.
    Needed { subfunction: 0x5D, input_bytes: 0x20, output_bytes: 0x60, status_at: 0x28 },
    // A dword, a byte that if set replaces the dword with all ones, then the status index.
    Needed { subfunction: 0x62, input_bytes: 0x20, output_bytes: 0x20, status_at: 0x08 },
    // The driver hands over a block of fields and reads only the status index back.
    Needed { subfunction: 0x6C, input_bytes: 0x100, output_bytes: 0x20, status_at: 0x00 },
];

/// The shape of one escape's payload, and where in its output the status index sits.
#[derive(Debug, Clone, Copy)]
struct Needed {
    subfunction: u32,
    input_bytes: usize,
    output_bytes: usize,
    status_at: usize,
}

/// The status index the kernel driver puts in an escape's output, which is an index and not an
/// NTSTATUS: the guest maps it to the value it returns. Zero is done.
pub const STATUS_DONE: u32 = 0;
/// What the guest's driver returns for each status index, from zero. Only index zero lets it go on
/// from the escapes in [`NEEDED_AT_START`].
pub const STATUS_VALUES: [i32; 7] = [0, -0x47, 9, -0x51, -0x50, -0x52, -0x53];

/// What the guest's driver returns for a status index, or `None` for an index it has no value for.
pub fn guest_status(index: u32) -> Option<i32> {
    STATUS_VALUES.get(index as usize).copied()
}

/// Service 3 subfunctions it sends at start and goes on without, skipping their result. The two
/// service 1 subfunctions moved to service 3 are among them.
const OPTIONAL_AT_START: [u32; 4] = [0x5B, 0x5E, 0x26, 0x3E];

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

    /// Whether the guest's driver can't start without this escape.
    pub fn needed_by_the_guest(&self) -> bool {
        self.needed().is_some()
    }

    fn needed(&self) -> Option<&'static Needed> {
        let user_mode = self.service == SERVICE_3 && self.client == USER_MODE_CLIENT;
        NEEDED_AT_START.iter().find(|needed| user_mode && needed.subfunction == self.subfunction)
    }

    /// Whether the guest's driver sends this at start and goes on when it fails.
    pub fn optional_for_the_guest(&self) -> bool {
        self.service == SERVICE_3 && OPTIONAL_AT_START.contains(&self.subfunction)
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

/// How an escape was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answered {
    pub code: EscapeCode,
    /// The return code was zero, with a payload filled in.
    pub done: bool,
}

/// Fills in the reply to an escape the guest can't start without. Returns false, writing nothing,
/// when the payload isn't the shape the guest's driver sends.
fn reply(data: &mut [u8], needed: &Needed) -> bool {
    let output_count = HEADER_SIZE + COUNT_SIZE + needed.input_bytes;
    let output = output_count + COUNT_SIZE;
    let well_shaped = dword(data, HEADER_SIZE) == Some(needed.input_bytes as u32)
        && dword(data, output_count) == Some(needed.output_bytes as u32)
        && data.len() >= output + needed.output_bytes;
    if !well_shaped {
        return false;
    }
    data[output..output + needed.output_bytes].fill(0);
    put_dword(data, output + needed.status_at, STATUS_DONE);
    put_dword(data, RETURN_CODE, RETURN_DONE);
    true
}

/// Whether the user-mode driver's wrapper is in front of the escape.
fn is_wrapped(data: &[u8]) -> bool {
    data.len() >= WRAPPER_SIZE
        && dword(data, 0) == Some(WRAPPER_KIND)
        && dword(data, WRAPPER_VERSION_AT) == Some(WRAPPER_VERSION)
}

/// The escape inside its wrapper, or the whole buffer when it came without one.
pub fn unwrapped(data: &mut [u8]) -> &mut [u8] {
    if is_wrapped(data) { &mut data[WRAPPER_SIZE..] } else { data }
}

/// [`unwrapped`], for a buffer that is only read.
fn unwrapped_to_read(data: &[u8]) -> &[u8] {
    if is_wrapped(data) { &data[WRAPPER_SIZE..] } else { data }
}

/// One of a payload's two byte counts, as far as the buffer holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// The buffer ends before the count.
    Missing,
    /// The count, with that many bytes after it in the buffer.
    Fits(u32),
    /// The count, with fewer bytes than that after it. Whatever would follow them can't be placed.
    PastEnd(u32),
}

/// How an escape is framed, read from it as it came, for the log. No answer is made from this: the
/// counts are read where the payload's shape puts them, whether or not this escape's payload has
/// that shape, and each says whether the buffer bears it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framing {
    /// The user-mode driver's wrapper is in front.
    pub wrapped: bool,
    /// The code as it was sent, where [`EscapeCode`] is the one the kernel driver dispatches. `None`
    /// for a buffer too short for a header.
    pub code_sent: Option<u32>,
    /// The count ahead of the input.
    pub input: Count,
    /// The count ahead of the output, which is [`Count::Missing`] unless the input fits.
    pub output: Count,
}

/// Reads an escape's framing, inside its wrapper if it has one.
pub fn framing(data: &[u8]) -> Framing {
    let escape = unwrapped_to_read(data);
    let count_at = |at: usize| match dword(escape, at) {
        None => Count::Missing,
        Some(count) => {
            let end = (at + COUNT_SIZE).checked_add(count as usize);
            if end.is_some_and(|end| end <= escape.len()) {
                Count::Fits(count)
            } else {
                Count::PastEnd(count)
            }
        }
    };
    let input = count_at(HEADER_SIZE);
    let output = match input {
        Count::Fits(bytes) => count_at(HEADER_SIZE + COUNT_SIZE + bytes as usize),
        Count::Missing | Count::PastEnd(_) => Count::Missing,
    };
    let code_sent = if escape.len() >= HEADER_SIZE { dword(escape, ESCAPE_CODE) } else { None };
    Framing { wrapped: is_wrapped(data), code_sent, input, output }
}

/// The payload after the header, inside the wrapper if there is one. Empty for a buffer that ends
/// with its header or before.
pub fn payload(data: &[u8]) -> &[u8] {
    unwrapped_to_read(data).get(HEADER_SIZE..).unwrap_or_default()
}

/// Answers an escape in place, inside its wrapper if it has one. Returns how, or why it couldn't be
/// read; an escape too short for its header has nowhere to put an answer and goes back untouched.
pub fn answer(data: &mut [u8]) -> Result<Answered, Refused> {
    answer_with(data, |_, _| false)
}

/// [`answer`], with someone to ask about an escape nothing here answers. `other` is given the
/// dispatched code and the payload after the header, and says whether it wrote an answer there, in
/// which case the escape is done.
pub fn answer_with(
    data: &mut [u8],
    other: impl FnOnce(EscapeCode, &mut [u8]) -> bool,
) -> Result<Answered, Refused> {
    let data = unwrapped(data);
    let code = match parse(data) {
        Ok(code) => code,
        Err(refused) => {
            if data.len() >= HEADER_SIZE {
                put_dword(data, RETURN_CODE, RETURN_FAILED);
            }
            return Err(refused);
        }
    };
    let done =
        code.needed().is_some_and(|needed| reply(data, needed)) || other(code, &mut data[HEADER_SIZE..]);
    put_dword(data, RETURN_CODE, if done { RETURN_DONE } else { RETURN_FAILED });
    Ok(Answered { code, done })
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
        let answered = answer(&mut data).unwrap();
        assert_eq!(answered.code.subfunction, 1);
        assert!(!answered.done);
        assert_eq!(dword(&data, RETURN_CODE), Some(RETURN_FAILED));
        assert_eq!(data[HEADER_SIZE..], payload_before[..]);
    }

    /// An escape the way the guest's driver sends it, with its output filled with a mark so that
    /// what the answer writes shows.
    fn needed_escape(subfunction: u32) -> Vec<u8> {
        let needed = NEEDED_AT_START.iter().find(|needed| needed.subfunction == subfunction).unwrap();
        let output_count = HEADER_SIZE + COUNT_SIZE + needed.input_bytes;
        let mut data = vec![0u8; output_count + COUNT_SIZE + needed.output_bytes];
        put_dword(&mut data, SIZE, HEADER_SIZE as u32);
        put_dword(&mut data, HEADER_VERSION, LEAST_HEADER_VERSION);
        put_dword(&mut data, ESCAPE_CODE, 0x0300_0003 | subfunction << 8);
        put_dword(&mut data, HEADER_SIZE, needed.input_bytes as u32);
        put_dword(&mut data, output_count, needed.output_bytes as u32);
        data[output_count + COUNT_SIZE..].fill(0xCC);
        data
    }

    fn output_of(data: &[u8], subfunction: u32) -> &[u8] {
        let needed = NEEDED_AT_START.iter().find(|needed| needed.subfunction == subfunction).unwrap();
        &data[HEADER_SIZE + 2 * COUNT_SIZE + needed.input_bytes..]
    }

    #[test]
    fn the_six_qwords_escape_is_answered_with_zeros_and_status_done() {
        let mut data = needed_escape(0x5D);
        let answered = answer(&mut data).unwrap();
        assert!(answered.done && answered.code.needed_by_the_guest());
        assert_eq!(dword(&data, RETURN_CODE), Some(0));
        // Buffer offsets 0xA8 on: five qwords, the status index at 0xD0, the sixth qword at 0xD8.
        assert_eq!(HEADER_SIZE + 2 * COUNT_SIZE + 0x20 + 0x28, 0xD0);
        assert_eq!(output_of(&data, 0x5D).len(), 0x60);
        assert!(output_of(&data, 0x5D).iter().all(|&byte| byte == 0));
        assert_eq!(dword(&data, 0xD0), Some(STATUS_DONE));
        // The counts the guest wrote are left as they were.
        assert_eq!(dword(&data, HEADER_SIZE), Some(0x20));
        assert_eq!(dword(&data, 0xA4), Some(0x60));
    }

    #[test]
    fn the_other_two_needed_escapes_are_answered_with_status_done_at_their_own_place() {
        for (subfunction, status_buffer_offset, output_len) in [(0x62, 0xB0, 0x20), (0x6C, 0x188, 0x20)] {
            let mut data = needed_escape(subfunction);
            assert!(answer(&mut data).unwrap().done);
            assert_eq!(dword(&data, RETURN_CODE), Some(0));
            assert_eq!(output_of(&data, subfunction).len(), output_len);
            assert!(output_of(&data, subfunction).iter().all(|&byte| byte == 0));
            assert_eq!(dword(&data, status_buffer_offset), Some(STATUS_DONE));
        }
        // 0x62: the output starts at 0xA8, holding the dword, the byte that would replace it with all
        // ones, then the index.
        assert_eq!(needed_escape(0x62).len(), 0xA8 + 0x20);
        // 0x6C: 0x100 bytes of input, so the output sits at 0x188.
        assert_eq!(needed_escape(0x6C).len(), 0x188 + 0x20);
    }

    #[test]
    fn a_needed_escape_that_is_not_shaped_as_sent_is_failed_and_left_alone() {
        for subfunction in [0x5D, 0x62, 0x6C] {
            let whole = needed_escape(subfunction);
            let mut short = whole.clone();
            short.pop();
            let mut wrong_input = whole.clone();
            put_dword(&mut wrong_input, HEADER_SIZE, 4);
            let mut wrong_output = whole.clone();
            let at = HEADER_SIZE + COUNT_SIZE + dword(&whole, HEADER_SIZE).unwrap() as usize;
            put_dword(&mut wrong_output, at, 4);
            for mut data in [short, wrong_input, wrong_output] {
                let before = data.clone();
                let answered = answer(&mut data).unwrap();
                assert!(!answered.done);
                assert_eq!(dword(&data, RETURN_CODE), Some(RETURN_FAILED));
                data[RETURN_CODE..RETURN_CODE + 4].copy_from_slice(&before[RETURN_CODE..RETURN_CODE + 4]);
                assert_eq!(data, before);
            }
        }
    }

    #[test]
    fn an_escape_nothing_here_answers_is_done_when_someone_else_answers_it() {
        let mut data = escape(0x0200_0103);
        let answered = answer_with(&mut data, |code, payload| {
            assert_eq!((code.service, code.subfunction, payload.len()), (2, 1, 0x20));
            payload[4] = 0x5A;
            true
        });
        assert!(answered.unwrap().done);
        assert_eq!(dword(&data, RETURN_CODE), Some(RETURN_DONE));
        assert_eq!(data[HEADER_SIZE + 4], 0x5A);

        let mut left = escape(0x0200_0103);
        assert!(!answer_with(&mut left, |_, _| false).unwrap().done);
        assert_eq!(dword(&left, RETURN_CODE), Some(RETURN_FAILED));
    }

    #[test]
    fn nobody_else_is_asked_about_an_escape_answered_here_or_one_that_cannot_be_read() {
        let nobody = |_: EscapeCode, _: &mut [u8]| -> bool { panic!("asked") };
        assert!(answer_with(&mut needed_escape(0x5D), nobody).unwrap().done);
        let mut old = escape(0x0200_0103);
        put_dword(&mut old, HEADER_VERSION, 0xFFFF);
        assert!(answer_with(&mut old, nobody).is_err());
        assert!(answer_with(&mut [0xEE; 6], nobody).is_err());
    }

    #[test]
    fn a_needed_subfunction_under_another_service_or_client_is_not_answered() {
        for code in [0x0200_5D03, 0x0300_5D01] {
            let mut data = needed_escape(0x5D);
            put_dword(&mut data, ESCAPE_CODE, code);
            assert!(!answer(&mut data).unwrap().done);
        }
    }

    #[test]
    fn the_status_index_maps_as_the_guest_maps_it() {
        assert_eq!(guest_status(STATUS_DONE), Some(0));
        assert_eq!(guest_status(1), Some(-0x47));
        assert_eq!(guest_status(2), Some(9));
        assert_eq!(guest_status(6), Some(-0x53));
        assert_eq!(guest_status(7), None);
    }

    #[test]
    fn the_escapes_the_guest_cant_do_without_are_told_apart() {
        for code in [0x0300_5D03, 0x0300_6C03, 0x0300_6203] {
            let code = EscapeCode::from_code(code);
            assert_eq!(code.client, USER_MODE_CLIENT);
            assert!(code.needed_by_the_guest());
            assert!(!code.optional_for_the_guest());
        }
        for code in [0x0300_5B03, 0x0300_5E03, 0x0100_2603, 0x0100_3E03] {
            let code = EscapeCode::from_code(code);
            assert!(code.optional_for_the_guest());
            assert!(!code.needed_by_the_guest());
        }
        // The same subfunction under another service is something else.
        assert!(!EscapeCode::from_code(0x0200_5D03).needed_by_the_guest());
        assert!(!EscapeCode::from_code(0x0100_5B03).optional_for_the_guest());
    }

    fn wrapped(inner: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; WRAPPER_SIZE];
        put_dword(&mut data, 0, WRAPPER_KIND);
        put_dword(&mut data, WRAPPER_VERSION_AT, WRAPPER_VERSION);
        put_dword(&mut data, 0x48, inner.len() as u32);
        data.extend_from_slice(inner);
        data
    }

    #[test]
    fn a_wrapped_escape_is_answered_inside_and_the_wrapper_is_left_alone() {
        for subfunction in [0x5D, 0x62, 0x6C] {
            let mut bare = needed_escape(subfunction);
            let mut data = wrapped(&bare);
            let wrapper_before = data[..WRAPPER_SIZE].to_vec();
            assert!(answer(&mut data).unwrap().done);
            assert!(answer(&mut bare).unwrap().done);
            assert_eq!(data[WRAPPER_SIZE..], bare[..]);
            assert_eq!(data[..WRAPPER_SIZE], wrapper_before[..], "{subfunction:#x}");
        }
    }

    /// The first escape a boot showed: 724 bytes, a 256-byte input and a 256-byte output.
    #[test]
    fn the_wrapped_escape_from_the_boot_is_read_and_answered_as_failed() {
        let mut inner = vec![0u8; 0x288];
        put_dword(&mut inner, SIZE, HEADER_SIZE as u32);
        put_dword(&mut inner, HEADER_VERSION, LEAST_HEADER_VERSION);
        put_dword(&mut inner, ESCAPE_CODE, 0x0300_7E03);
        put_dword(&mut inner, HEADER_SIZE, 0x100);
        put_dword(&mut inner, HEADER_SIZE + COUNT_SIZE + 0x100, 0x100);
        let mut data = wrapped(&inner);
        assert_eq!(data.len(), 724);
        let answered = answer(&mut data).unwrap();
        assert_eq!(answered.code, EscapeCode { service: 3, subfunction: 0x7E, client: USER_MODE_CLIENT });
        assert!(!answered.done);
        assert_eq!(dword(&data, WRAPPER_SIZE + RETURN_CODE), Some(RETURN_FAILED));
        assert_eq!(dword(&data, 0), Some(WRAPPER_KIND));
    }

    #[test]
    fn the_framing_is_read_from_inside_the_wrapper() {
        let mut bare = needed_escape(0x5D);
        put_dword(&mut bare, ESCAPE_CODE, 0x0100_2603);
        let read = Framing {
            wrapped: false,
            code_sent: Some(0x0100_2603),
            input: Count::Fits(0x20),
            output: Count::Fits(0x60),
        };
        assert_eq!(framing(&bare), read);
        assert_eq!(framing(&wrapped(&bare)), Framing { wrapped: true, ..read });
        assert_eq!(payload(&bare).len(), 2 * COUNT_SIZE + 0x20 + 0x60);
        assert_eq!(payload(&wrapped(&bare)), payload(&bare));
        // The kernel driver dispatches this one under another service, and the framing still says
        // what was sent.
        assert_eq!(parse(&bare).unwrap().service, SERVICE_3);
    }

    #[test]
    fn a_count_the_buffer_does_not_bear_out_says_so() {
        let whole = needed_escape(0x62);
        let mut short_output = whole.clone();
        short_output.pop();
        assert_eq!(framing(&short_output).input, Count::Fits(0x20));
        assert_eq!(framing(&short_output).output, Count::PastEnd(0x20));

        // The input's count runs past the end, so nothing says where the output's count would be.
        let mut long_input = whole.clone();
        put_dword(&mut long_input, HEADER_SIZE, 0x1000);
        assert_eq!(framing(&long_input).input, Count::PastEnd(0x1000));
        assert_eq!(framing(&long_input).output, Count::Missing);
        put_dword(&mut long_input, HEADER_SIZE, u32::MAX);
        assert_eq!(framing(&long_input).input, Count::PastEnd(u32::MAX));

        let no_output_count = &whole[..HEADER_SIZE + COUNT_SIZE + 0x20 + 3];
        assert_eq!(framing(no_output_count).input, Count::Fits(0x20));
        assert_eq!(framing(no_output_count).output, Count::Missing);

        let header_only = &whole[..HEADER_SIZE];
        assert_eq!(framing(header_only).code_sent, Some(0x0300_6203));
        assert_eq!(
            (framing(header_only).input, framing(header_only).output),
            (Count::Missing, Count::Missing)
        );
        assert!(payload(header_only).is_empty());
    }

    #[test]
    fn a_buffer_too_short_for_a_header_has_no_code_and_no_payload() {
        let short = &needed_escape(0x62)[..HEADER_SIZE - 1];
        let read = Framing { wrapped: false, code_sent: None, input: Count::Missing, output: Count::Missing };
        assert_eq!(framing(short), read);
        assert!(payload(short).is_empty());
        assert_eq!(framing(&[]), read);
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
