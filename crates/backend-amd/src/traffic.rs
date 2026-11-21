//! One log line for each exchange with the guest's driver, in a fixed form a tester's log can be
//! searched by: `amd <kind> name=value ...`.
//!
//! The kinds are `escape`, `query`, `registry`, `submit`, `context`, `queue`, `video` and `session`.
//! All of the lines are at debug, so they are in the log only with `GUESTHOUSE_VERBOSE`, and nothing is
//! read for them without it. A line names only what the protocol modules read; the rest of a buffer
//! is in its `amd dump` line, whole up to 4 KiB. A submission has no dump, as a busy guest makes
//! thousands a second. A registry lookup left to the core has neither line: a guest makes them by
//! the hundred thousand as it starts, and the core logs each name it finds no value for. A value
//! this backend gives has a `registry` line with its name.
//!
//! Work for the video ring has its `submit` line like any other, then a dump of each command
//! buffer as the daemon copied it, and once it has been waited for a `video` line that says how it
//! ended. There are a few dozen of those a second at most, and the commands are what a person
//! needs when the card's video engine didn't take them. An escape of a video session has a
//! `session` line with what the books made of it, ahead of its `escape` line, which shows what was
//! sent.
//!
//! Nothing here writes to a buffer of the guest's or decides an answer: every function takes what
//! it logs by shared reference, after the answer is made or, for an escape, before and after.

use crate::protocol::command_data::Submission;
use crate::protocol::context::{self, Context};
use crate::protocol::escape::{self, Answered, Count, Framing};
use crate::protocol::hw_queue;
use crate::protocol::video_escape::{Asked, Change};
use crate::space_vm::Ran;
use crate::submit::Ib;
use crate::video::Outcome;
use crate::video_ib::Copied;
use crate::watch::{Hex, HexNumber, Kept, OrDash, PREFIX, Spaced, bit, dump, dwords, watching};
use guesthouse_backend::{RegistryName, RegistryValue, Status, VendorQuery, Work, WorkSource};
use guesthouse_messages::adapter::AdapterQuery;
use std::fmt;
use std::io;
use std::time::Duration;

/// How many bytes of an escape's payload a line shows.
const ESCAPE_BYTES_SHOWN: usize = 32;
/// How many bytes of the data that came with a query a line shows.
const QUERY_BYTES_SHOWN: usize = 32;
/// How many dwords a line shows of a submission that was refused: its header, and most of what its
/// first record can hold.
const REFUSED_DWORDS_SHOWN: usize = 16;

/// An escape as the guest sent it, kept from before its answer was written into the same buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscapeAsked {
    length: usize,
    framing: Framing,
    payload: Kept<ESCAPE_BYTES_SHOWN>,
}

impl EscapeAsked {
    fn read(data: &[u8]) -> Self {
        Self { length: data.len(), framing: escape::framing(data), payload: Kept::of(escape::payload(data)) }
    }
}

/// Dumps an escape as it came, and keeps what its line will say of it. Called before the escape is
/// answered. With debug lines off it reads nothing and returns `None`.
pub fn escape_asked(process: u32, data: &[u8]) -> Option<EscapeAsked> {
    if !watching() {
        return None;
    }
    dump(format_args!("escape process={process:#x}"), data);
    Some(EscapeAsked::read(data))
}

/// Logs an escape's line, once it is answered.
pub fn escape_answered(
    process: u32,
    asked: Option<EscapeAsked>,
    answered: &Result<Answered, escape::Refused>,
) {
    if let Some(asked) = asked {
        log::debug!("{}", EscapeLine { process, asked, answered });
    }
}

/// An escape's line. `code` is the code as sent, and `service`, `subfunction` and `client` are the
/// parts of the one the kernel driver dispatches, which differ for the few codes it renames.
struct EscapeLine<'a> {
    process: u32,
    asked: EscapeAsked,
    answered: &'a Result<Answered, escape::Refused>,
}

impl fmt::Display for EscapeLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { process, asked, answered } = self;
        let code = answered.as_ref().ok().map(|answered| answered.code);
        write!(
            f,
            "{PREFIX} escape process={process:#x} length={} wrapped={} code={} service={} subfunction={} \
             client={} input_bytes={} output_bytes={} {} payload={}",
            asked.length,
            bit(asked.framing.wrapped),
            OrDash(asked.framing.code_sent.map(HexNumber)),
            OrDash(code.map(|code| code.service)),
            OrDash(code.map(|code| HexNumber(code.subfunction))),
            OrDash(code.map(|code| code.client)),
            CountText(asked.framing.input),
            CountText(asked.framing.output),
            EscapeAnswer(answered),
            asked.payload
        )
    }
}

/// A payload's byte count: the count, `-` where the buffer ends before it, and a mark where the
/// buffer hasn't that many bytes after it.
struct CountText(Count);

impl fmt::Display for CountText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Count::Missing => f.write_str("-"),
            Count::Fits(count) => write!(f, "{count}"),
            Count::PastEnd(count) => write!(f, "{count}(past-end)"),
        }
    }
}

/// How an escape was answered, as `answer=`, with why for one that couldn't be read.
struct EscapeAnswer<'a>(&'a Result<Answered, escape::Refused>);

impl fmt::Display for EscapeAnswer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(answered) if answered.done => f.write_str("answer=done"),
            Ok(answered) if answered.code.needed_by_the_guest() => f.write_str("answer=needed-but-misshaped"),
            Ok(_) => f.write_str("answer=failed"),
            Err(refused) => write!(f, "answer=unreadable why=\"{}\"", refused.0),
        }
    }
}

/// Logs an adapter query this backend was offered, and what came of it: `answer` is the status it
/// answered with, or `None` for a query it left to the core. Then dumps the data that came with it.
pub fn query(query: &VendorQuery<'_>, answer: Option<Status>) {
    // The core reads a registry lookup itself and logs the name it finds no value for, and a guest
    // makes them by the hundred thousand as it starts, so one left to the core gets nothing here.
    if !watching() || (answer.is_none() && query.kind == AdapterQuery::REGISTRY) {
        return;
    }
    log::debug!("{}", QueryLine { query, answer });
    dump(format_args!("query kind={}", query.kind), query.data);
}

/// A query's line. The program is quoted, as a name can hold a space, and empty when the guest gave
/// none.
struct QueryLine<'a> {
    query: &'a VendorQuery<'a>,
    answer: Option<Status>,
}

impl fmt::Display for QueryLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { query, answer } = self;
        write!(
            f,
            "{PREFIX} query kind={} size={} wow64={} program=\"{}\" data_bytes={} ",
            query.kind,
            query.size,
            bit(query.process_is_32_bit),
            query.program,
            query.data.len()
        )?;
        match answer {
            Some(status) => write!(f, "answer=given status={:#x}", status.0)?,
            None => f.write_str("answer=left-to-the-core")?,
        }
        write!(f, " data={}", Hex { bytes: query.data, most: QUERY_BYTES_SHOWN })
    }
}

/// Logs a registry value this backend gives, which nothing else names: the lookup has no line as a
/// query, and the core logs only the names no value was found for. What the guest gets of the value
/// is the core's to say, as it refuses a lookup that asks for another type.
pub fn registry(name: &RegistryName, value: &RegistryValue) {
    log::debug!("{}", RegistryLine { name, value });
}

/// A registry value's line. A driver path has no name the guest spells, and no type to match.
struct RegistryLine<'a> {
    name: &'a RegistryName,
    value: &'a RegistryValue,
}

impl fmt::Display for RegistryLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.name {
            RegistryName::DriverStorePath => "<driver store path>",
            RegistryName::DriverImagePath => "<driver image path>",
            RegistryName::Value(name) => name,
        };
        write!(
            f,
            "{PREFIX} registry name=\"{name}\" type={} bytes={}",
            OrDash(self.value.value_type),
            self.value.bytes.len()
        )
    }
}

/// Logs a submission whose private data was taken: what the check read from it, and the command
/// buffers it names. Whether it runs follows from the line and from any warning after it: one with
/// no buffers, or that may not run, or on a queue that failed, is finished without running. One for
/// a node the card hasn't got has only its warning, as there is no engine family to name.
pub fn submission(work: &Work<'_>, engine_family: u32, submission: &Submission, buffers: &[Ib]) {
    log::debug!("{}", SubmitLine { work, engine_family, submission, buffers });
}

/// Logs a submission whose private data was refused, with how it starts.
pub fn submission_refused(work: &Work<'_>, why: &str) {
    log::debug!("{}", RefusedSubmitLine { work, why });
}

/// How a check of private data came out, as `check=`, with why for data that was refused.
enum Check<'a> {
    Taken,
    /// Taken with nothing checked, as the data is too short to hold what a check reads.
    Unread,
    Refused(&'a str),
}

impl fmt::Display for Check<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Taken => f.write_str("check=taken"),
            Self::Unread => f.write_str("check=unread"),
            Self::Refused(why) => write!(f, "check=refused why=\"{why}\""),
        }
    }
}

/// What the core says of a submission, which both of its lines start with.
struct WorkText<'a>(&'a Work<'a>);

impl fmt::Display for WorkText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let work = self.0;
        let source = match work.source {
            WorkSource::HardwareQueue => "hardware-queue",
            WorkSource::Context => "context",
        };
        write!(
            f,
            "{PREFIX} submit source={source} queue={:#x} node={} hint={:#x} fence_value={} length={}",
            work.queue,
            work.node,
            work.client_hint,
            work.fence_value,
            work.private.len()
        )
    }
}

struct SubmitLine<'a> {
    work: &'a Work<'a>,
    /// amdgpu's `HW_IP_*` behind the node.
    engine_family: u32,
    submission: &'a Submission,
    buffers: &'a [Ib],
}

impl fmt::Display for SubmitLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { work, engine_family, submission, buffers } = self;
        write!(
            f,
            "{} {} client={} opcodes=[{}] engine_family={engine_family} space={} buffers=[{}] may_run={} \
             queue_failed={}",
            WorkText(work),
            Check::Taken,
            submission.client,
            Spaced(submission.opcodes.iter()),
            work.space.0,
            Spaced(buffers.iter().map(BufferText)),
            bit(work.may_run),
            bit(work.queue_failed)
        )
    }
}

/// A command buffer, as `address:bytes`.
struct BufferText<'a>(&'a Ib);

impl fmt::Display for BufferText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}:{:#x}", self.0.address, self.0.bytes)
    }
}

struct RefusedSubmitLine<'a> {
    work: &'a Work<'a>,
    why: &'a str,
}

impl fmt::Display for RefusedSubmitLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { work, why } = self;
        let first = dwords(work.private, REFUSED_DWORDS_SHOWN);
        write!(f, "{} {} first=[{first}]", WorkText(work), Check::Refused(why))
    }
}

/// Logs a new context's private data and what the check made of it, and dumps the data.
pub fn context(process: u32, node: u32, private: &[u8], checked: &Result<Option<Context>, context::Refused>) {
    if !watching() {
        return;
    }
    log::debug!("{}", ContextLine { process, node, private, checked });
    dump(format_args!("context process={process:#x} node={node}"), private);
}

/// A context's line. The two flag dwords are as they came, so a refused context shows them too. The
/// client id and the extension are what the check read, and `-` where it took or read nothing.
struct ContextLine<'a> {
    process: u32,
    node: u32,
    private: &'a [u8],
    checked: &'a Result<Option<Context>, context::Refused>,
}

impl fmt::Display for ContextLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { process, node, private, checked } = self;
        let check = match checked {
            Ok(Some(_)) => Check::Taken,
            Ok(None) => Check::Unread,
            Err(refused) => Check::Refused(refused.0),
        };
        let taken = checked.as_ref().ok().copied().flatten();
        let words = context::flag_words(private);
        write!(
            f,
            "{PREFIX} context process={process:#x} node={node} length={} {check} flags={} session_flags={} \
             client_id={} extension={}",
            private.len(),
            OrDash(words.map(|words| HexNumber(words.flags))),
            OrDash(words.map(|words| HexNumber(words.session_flags))),
            OrDash(taken.and_then(|context| context.client_id)),
            OrDash(taken.map(|context| bit(context.has_extension)))
        )
    }
}

/// Dumps the command buffers of work for the video ring, as they were copied out of the guest's
/// pages. Called before the copy is run, so the log has them if the run never comes back.
pub fn video_commands(work: &Work<'_>, copied: &Copied) {
    if !watching() {
        return;
    }
    for (index, buffer) in copied.buffers().enumerate() {
        let bytes: Vec<u8> = buffer.iter().flat_map(|dword| dword.to_le_bytes()).collect();
        let (queue, fence_value) = (work.queue, work.fence_value);
        dump(format_args!("video queue={queue:#x} fence_value={fence_value} buffer={index}"), &bytes);
    }
}

/// Logs how a run on the video ring ended, after `took` of waiting for it.
pub fn video_ran(work: &Work<'_>, copied: &Copied, took: Duration, ran: &io::Result<Ran>) {
    log::debug!("{}", VideoLine { work, buffers: copied.runs(), took, ran });
}

/// A video run's line. The buffers are where the daemon's copies were run from, which is the same
/// place for every run, so their lengths are what tells runs apart.
struct VideoLine<'a> {
    work: &'a Work<'a>,
    buffers: &'a [Ib],
    took: Duration,
    ran: &'a io::Result<Ran>,
}

impl fmt::Display for VideoLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { work, buffers, took, ran } = self;
        write!(
            f,
            "{PREFIX} video queue={:#x} fence_value={} space={} buffers=[{}] took_us={} {}",
            work.queue,
            work.fence_value,
            work.space.0,
            Spaced(buffers.iter().map(BufferText)),
            took.as_micros(),
            RanText(ran)
        )
    }
}

/// How a run that was waited for ended, as `ran=`, with where for one that faulted and why for one
/// that wasn't run.
pub struct RanText<'a>(pub &'a io::Result<Ran>);

impl fmt::Display for RanText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(Ran::Retired) => f.write_str("ran=retired"),
            Ok(Ran::Refused) => f.write_str("ran=refused"),
            Ok(Ran::Hung) => f.write_str("ran=hung"),
            Ok(Ran::Faulted(fault)) => {
                write!(f, "ran=faulted address={:#x} status={:#x}", fault.address, fault.status)
            }
            Err(error) => write!(f, "ran=not-run why=\"{error}\""),
        }
    }
}

/// Logs what the books made of one of a video session's escapes: what was asked, what the guest
/// reads back, and how many sessions are open after it.
pub fn session(process: u32, asked: Asked, outcome: Outcome, open: usize) {
    log::debug!("{}", SessionLine { process, asked, outcome, open });
}

struct SessionLine {
    process: u32,
    asked: Asked,
    outcome: Outcome,
    open: usize,
}

impl fmt::Display for SessionLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { process, asked, outcome, open } = self;
        write!(f, "{PREFIX} session process={process:#x} ")?;
        match asked {
            Asked::Start => f.write_str("asked=start")?,
            Asked::End { session } => write!(f, "asked=end session={session}")?,
            Asked::Throughput { session, engine, change } => {
                let what = match change {
                    Change::Book(_) => "book",
                    Change::Release => "release",
                    Change::Ask => "ask",
                };
                write!(f, "asked={what} session={session} engine={engine}")?;
                if let Change::Book(demand) = change {
                    write!(f, " demand={demand}")?;
                }
            }
        }
        match outcome {
            Outcome::Started { session } => write!(f, " answer=done gave={session}")?,
            Outcome::Full => f.write_str(" answer=failed why=\"no more sessions may be open\"")?,
            Outcome::Ended => f.write_str(" answer=done")?,
            Outcome::NoSuchSession => f.write_str(" answer=failed why=\"no such session is open\"")?,
            Outcome::Throughput { booked, most } => write!(f, " answer=done booked={booked} most={most}")?,
        }
        write!(f, " open={open}")
    }
}

/// Logs a new hardware queue's private data and what the check made of it, and dumps the data.
/// `private` is the guest's data and the padding after it, as the core passes it on.
pub fn queue(queue: u32, serial: u32, private: &[u8], checked: Result<(), hw_queue::Refused>) {
    if !watching() {
        return;
    }
    log::debug!("{}", QueueLine { queue, serial, bytes_with_padding: private.len(), checked });
    dump(format_args!("queue id={queue:#x}"), private);
}

struct QueueLine {
    queue: u32,
    serial: u32,
    bytes_with_padding: usize,
    checked: Result<(), hw_queue::Refused>,
}

impl fmt::Display for QueueLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { queue, serial, bytes_with_padding, checked } = self;
        let check = match checked {
            Ok(()) => Check::Taken,
            Err(refused) => Check::Refused(refused.0),
        };
        write!(
            f,
            "{PREFIX} queue id={queue:#x} serial={serial} bytes_with_padding={bytes_with_padding} {check}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::{put_dword, put_qword};
    use crate::protocol::command_data;
    use crate::protocol::escape::{COUNT_SIZE, HEADER_SIZE, WRAPPER_KIND, WRAPPER_SIZE, WRAPPER_VERSION};
    use crate::watch::lines;
    use log::Level;

    const PROCESS: u32 = 0x4000_0002;
    /// Where the header keeps its version and its code, and the oldest version the kernel takes.
    const HEADER_VERSION_AT: usize = 0x04;
    const HEADER_VERSION: u32 = 0x0001_0000;
    const CODE_AT: usize = 0x08;

    /// A framed escape with an input and an output of these sizes, each filled with its own mark.
    fn escape_of(code: u32, input_bytes: usize, output_bytes: usize) -> Vec<u8> {
        let output_count = HEADER_SIZE + COUNT_SIZE + input_bytes;
        let mut data = vec![0u8; output_count + COUNT_SIZE + output_bytes];
        put_dword(&mut data, 0, HEADER_SIZE as u32);
        put_dword(&mut data, HEADER_VERSION_AT, HEADER_VERSION);
        put_dword(&mut data, CODE_AT, code);
        put_dword(&mut data, HEADER_SIZE, input_bytes as u32);
        data[HEADER_SIZE + COUNT_SIZE..output_count].fill(0x11);
        put_dword(&mut data, output_count, output_bytes as u32);
        data[output_count + COUNT_SIZE..].fill(0xCC);
        data
    }

    fn wrapped(inner: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; WRAPPER_SIZE];
        put_dword(&mut data, 0, WRAPPER_KIND);
        put_dword(&mut data, 4, WRAPPER_VERSION);
        data.extend_from_slice(inner);
        data
    }

    /// The line an escape gets, read before the answer and written after it, as the backend does.
    fn line_of(mut data: Vec<u8>) -> String {
        let asked = EscapeAsked::read(&data);
        let answered = escape::answer(&mut data);
        EscapeLine { process: PROCESS, asked, answered: &answered }.to_string()
    }

    #[test]
    fn an_escape_answered_as_done_has_its_line() {
        assert_eq!(
            line_of(escape_of(0x0300_6203, 0x20, 0x20)),
            "amd escape process=0x40000002 length=200 wrapped=0 code=0x3006203 service=3 subfunction=0x62 \
             client=3 input_bytes=32 output_bytes=32 answer=done \
             payload=2000000011111111111111111111111111111111111111111111111111111111..(+40)"
        );
    }

    #[test]
    fn a_wrapped_escape_nothing_answers_is_failed_and_says_it_was_wrapped() {
        let line = line_of(wrapped(&escape_of(0x0300_7E03, 0x100, 0x100)));
        assert!(
            line.starts_with(
                "amd escape process=0x40000002 length=724 wrapped=1 code=0x3007e03 service=3 \
                 subfunction=0x7e client=3 input_bytes=256 output_bytes=256 answer=failed payload=00010000"
            ),
            "{line}"
        );
    }

    #[test]
    fn a_renamed_code_shows_both_what_was_sent_and_what_is_dispatched() {
        let line = line_of(escape_of(0x0100_2603, 4, 4));
        assert!(line.contains(" code=0x1002603 service=3 subfunction=0x26 client=3 "), "{line}");
    }

    #[test]
    fn a_needed_escape_of_the_wrong_shape_says_so_and_marks_the_count_that_does_not_fit() {
        let mut short = escape_of(0x0300_5D03, 0x20, 0x60);
        short.pop();
        let line = line_of(short);
        assert!(
            line.contains(" input_bytes=32 output_bytes=96(past-end) answer=needed-but-misshaped payload="),
            "{line}"
        );
    }

    #[test]
    fn an_escape_that_cannot_be_read_has_dashes_for_what_nothing_read() {
        assert_eq!(
            line_of(vec![0xEE; 6]),
            "amd escape process=0x40000002 length=6 wrapped=0 code=- service=- subfunction=- client=- \
             input_bytes=- output_bytes=- answer=unreadable why=\"shorter than its header\" payload="
        );
        let mut old = escape_of(0x0200_0103, 0, 0);
        put_dword(&mut old, HEADER_VERSION_AT, HEADER_VERSION - 1);
        assert_eq!(
            line_of(old),
            "amd escape process=0x40000002 length=136 wrapped=0 code=0x2000103 service=- subfunction=- \
             client=- input_bytes=0 output_bytes=0 answer=unreadable why=\"its header is too old\" \
             payload=0000000000000000"
        );
    }

    #[test]
    fn the_line_shows_the_payload_as_sent_even_where_the_answer_wrote_over_it() {
        let mut data = escape_of(0x0300_7E03, 0, 0x20);
        let asked = EscapeAsked::read(&data);
        // No answer made today writes this near the payload's start. One that did would look so.
        data[HEADER_SIZE..].fill(0);
        let line = EscapeLine { process: PROCESS, asked, answered: &escape::answer(&mut data) }.to_string();
        assert!(
            line.ends_with("payload=0000000020000000cccccccccccccccccccccccccccccccccccccccccccccccc..(+8)"),
            "{line}"
        );
    }

    #[test]
    fn an_escape_is_dumped_as_it_came_and_then_gets_its_line() {
        let mut data = escape_of(0x0300_6203, 0x20, 0x20);
        let sent = data.clone();
        let lines = lines::logged(|| {
            let asked = escape_asked(PROCESS, &data);
            let answered = escape::answer(&mut data);
            escape_answered(PROCESS, asked, &answered);
        });
        assert_ne!(data, sent, "the answer changed the buffer");
        let debug = lines::at(Level::Debug, &lines);
        assert_eq!(debug.len(), lines.len());
        let hex: String = sent.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(debug[0], format!("amd dump escape process=0x40000002 length=200 bytes={hex}"));
        assert!(debug[1].starts_with("amd escape process=0x40000002 length=200 "), "{}", debug[1]);
        assert_eq!(debug.len(), 2);
    }

    /// One of a video session's escapes as the guest's user-mode driver sends it: its header, then
    /// the payload, which has no counts.
    fn video_escape((code, payload): (escape::EscapeCode, Vec<u8>)) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_SIZE];
        put_dword(&mut data, 0, HEADER_SIZE as u32);
        put_dword(&mut data, HEADER_VERSION_AT, HEADER_VERSION);
        put_dword(&mut data, CODE_AT, code.service << 24 | code.subfunction << 8 | code.client);
        data.extend_from_slice(&payload);
        data
    }

    /// The escapes of one video session, in the order a guest's driver sends them.
    fn a_video_session(session: u32) -> [Vec<u8>; 5] {
        use crate::protocol::video_escape::tests::{end, start, throughput};
        [
            video_escape(throughput(0, Change::Ask)),
            video_escape(start()),
            wrapped(&video_escape(throughput(session, Change::Book(209_635)))),
            video_escape(throughput(session, Change::Release)),
            video_escape(end(session)),
        ]
    }

    #[test]
    fn a_video_sessions_escapes_fail_as_they_did_when_no_video_is_offered() {
        for sent in a_video_session(1) {
            let mut data = sent.clone();
            let lines = lines::logged(|| crate::answer_escape(PROCESS, &mut data, None));
            let inner = if escape::framing(&sent).wrapped { WRAPPER_SIZE } else { 0 };
            assert_eq!(
                data[inner + HEADER_SIZE..],
                sent[inner + HEADER_SIZE..],
                "the payload was written to"
            );
            let debug = lines::at(Level::Debug, &lines);
            assert_eq!(debug.len(), 2, "{lines:?}");
            assert!(debug[1].contains(" service=3 ") && debug[1].contains(" answer=failed "), "{}", debug[1]);
        }
    }

    #[test]
    fn a_video_sessions_escapes_are_answered_from_the_books_when_video_is_offered() {
        use crate::protocol::video_escape::tests::{answered, session_given};
        const MOST: u32 = 2_937_600;
        let mut books = crate::video::Sessions::new(MOST);
        let [mut ask, mut start, mut book, mut release, mut end] = a_video_session(1);
        let mut lines = Vec::new();
        for data in [&mut ask, &mut start, &mut book, &mut release, &mut end] {
            lines.extend(lines::logged(|| crate::answer_escape(PROCESS, data, Some(&mut books))));
        }
        assert_eq!(answered(&ask[HEADER_SIZE..]), (0, MOST));
        assert_eq!(session_given(&start[HEADER_SIZE..]), 1);
        assert_eq!(answered(&book[WRAPPER_SIZE + HEADER_SIZE..]), (209_635, MOST), "inside its wrapper");
        assert_eq!(answered(&release[HEADER_SIZE..]), (0, MOST));
        // Each has its dump, its session line and its escape line, and nothing above debug.
        let debug = lines::at(Level::Debug, &lines);
        assert_eq!((debug.len(), lines.len()), (15, 15), "{lines:?}");
        let escapes = debug.iter().skip(2).step_by(3);
        assert!(escapes.clone().all(|line| line.starts_with("amd escape ")), "{lines:?}");
        assert!(escapes.clone().all(|line| line.contains(" service=3 ")), "{lines:?}");
        assert!(escapes.clone().all(|line| line.contains(" answer=done ")), "{lines:?}");
        let sessions: Vec<&str> = debug.iter().skip(1).step_by(3).copied().collect();
        assert_eq!(
            sessions,
            [
                "amd session process=0x40000002 asked=ask session=0 engine=41 answer=done booked=0 \
                 most=2937600 open=0",
                "amd session process=0x40000002 asked=start answer=done gave=1 open=1",
                "amd session process=0x40000002 asked=book session=1 engine=41 demand=209635 answer=done \
                 booked=209635 most=2937600 open=1",
                "amd session process=0x40000002 asked=release session=1 engine=41 answer=done booked=0 \
                 most=2937600 open=1",
                "amd session process=0x40000002 asked=end session=1 answer=done open=0",
            ]
        );
    }

    #[test]
    fn a_video_escape_the_books_cannot_do_is_failed_and_its_session_line_says_why() {
        let mut books = crate::video::Sessions::new(1);
        // A guest that was never given an ID sends zero for it.
        let [_, _, mut book, _, mut end] = a_video_session(0);
        let before = (book.clone(), end.clone());
        let lines = lines::logged(|| {
            crate::answer_escape(PROCESS, &mut book, Some(&mut books));
            crate::answer_escape(PROCESS, &mut end, Some(&mut books));
        });
        let debug = lines::at(Level::Debug, &lines);
        assert_eq!((debug.len(), lines.len()), (6, 6), "{lines:?}");
        assert!(debug[2].contains(" answer=failed ") && debug[5].contains(" answer=failed "), "{lines:?}");
        assert_eq!(
            [debug[1], debug[4]],
            [
                "amd session process=0x40000002 asked=book session=0 engine=41 demand=209635 answer=failed \
                 why=\"no such session is open\" open=0",
                "amd session process=0x40000002 asked=end session=0 answer=failed why=\"no such session is \
                 open\" open=0",
            ]
        );
        assert_eq!(book[WRAPPER_SIZE + HEADER_SIZE..], before.0[WRAPPER_SIZE + HEADER_SIZE..]);
        assert_eq!(end[HEADER_SIZE..], before.1[HEADER_SIZE..]);
    }

    /// What the backend's own handler leaves in the buffer and the log, for each way an escape can
    /// be answered: `(escape, warnings, info lines)`.
    fn every_kind_of_escape() -> Vec<(Vec<u8>, usize, usize)> {
        let mut misshaped = escape_of(0x0300_5D03, 0x20, 0x60);
        misshaped.pop();
        vec![
            (escape_of(0x0300_5D03, 0x20, 0x60), 0, 0),
            (escape_of(0x0300_6203, 0x20, 0x20), 0, 0),
            (escape_of(0x0300_6C03, 0x100, 0x20), 0, 0),
            (wrapped(&escape_of(0x0300_6203, 0x20, 0x20)), 0, 0),
            (wrapped(&escape_of(0x0300_7E03, 0x100, 0x100)), 0, 0),
            (escape_of(0x0200_0103, 8, 8), 0, 0),
            (misshaped, 1, 0),
            (vec![0xEE; 6], 0, 1),
            (Vec::new(), 0, 1),
        ]
    }

    #[test]
    fn the_answer_is_the_same_bytes_whether_or_not_the_escape_is_logged() {
        for (sent, warnings, infos) in every_kind_of_escape() {
            let mut alone = sent.clone();
            let _ = escape::answer(&mut alone);

            let mut quiet = sent.clone();
            crate::answer_escape(PROCESS, &mut quiet, None);
            assert_eq!(quiet, alone, "with debug lines off");

            let mut watched = sent.clone();
            let lines = lines::logged(|| crate::answer_escape(PROCESS, &mut watched, None));
            assert_eq!(watched, alone, "with debug lines on");

            // The dump is first and holds what was sent, and the line is last.
            let hex: String = sent.iter().map(|byte| format!("{byte:02x}")).collect();
            let length = sent.len();
            let debug = lines::at(Level::Debug, &lines);
            assert_eq!(debug.len(), 2, "{lines:?}");
            assert_eq!(lines[0].1, format!("amd dump escape process=0x40000002 length={length} bytes={hex}"));
            assert!(lines.last().unwrap().1.starts_with("amd escape process=0x40000002 "), "{lines:?}");
            assert_eq!(lines::at(Level::Warn, &lines).len(), warnings, "{lines:?}");
            assert_eq!(lines::at(Level::Info, &lines).len(), infos, "{lines:?}");
            assert_eq!(lines.len(), 2 + warnings + infos);
        }
    }

    #[test]
    fn the_lines_the_log_had_before_are_worded_as_they_were() {
        let mut misshaped = escape_of(0x0300_5D03, 0x20, 0x60);
        misshaped.pop();
        let lines = lines::logged(|| crate::answer_escape(7, &mut misshaped, None));
        assert_eq!(
            lines::at(Level::Warn, &lines),
            ["escape from 7: service 3 subfunction 0x5d, not shaped as the guest's driver sends it, \
              answered as failed; the guest can't go on without it"]
        );
        let lines = lines::logged(|| crate::answer_escape(7, &mut [0xEE; 6], None));
        assert_eq!(
            lines::at(Level::Info, &lines),
            ["an escape from 7 is shorter than its header (6 bytes, starting 0xeeeeeeee, 0xeeee)"]
        );
    }

    fn query_of<'a>(kind: u32, data: &'a [u8], program: &'a str) -> VendorQuery<'a> {
        VendorQuery { kind, size: 16, data, process_is_32_bit: false, program }
    }

    #[test]
    fn a_query_says_who_asked_what_and_who_answers() {
        let given =
            QueryLine { query: &query_of(1, &[0x0C, 0, 0, 0], "dwm.exe"), answer: Some(Status::SUCCESS) };
        assert_eq!(
            given.to_string(),
            "amd query kind=1 size=16 wow64=0 program=\"dwm.exe\" data_bytes=4 answer=given status=0x0 \
             data=0c000000"
        );
        let refused = QueryLine {
            query: &VendorQuery { process_is_32_bit: true, ..query_of(24, &[], "") },
            answer: Some(Status::INVALID_PARAMETER),
        };
        assert_eq!(
            refused.to_string(),
            "amd query kind=24 size=16 wow64=1 program=\"\" data_bytes=0 answer=given status=0xc000000d data="
        );
        let left = QueryLine { query: &query_of(15, &[0xAB; 40], "a game.exe"), answer: None };
        assert_eq!(
            left.to_string(),
            format!(
                "amd query kind=15 size=16 wow64=0 program=\"a game.exe\" data_bytes=40 \
                 answer=left-to-the-core data={}..(+8)",
                "ab".repeat(QUERY_BYTES_SHOWN)
            )
        );
    }

    /// Queries of each kind this backend answers, one it answers as too short, and one it leaves.
    fn queries() -> Vec<VendorQuery<'static>> {
        use crate::protocol::{
            adapter_header, adapter_info, adapter_registry, api_drivers, gpu_mmu_caps, wddm_caps,
        };
        let sized = |kind: u32, size: usize| VendorQuery { size, ..query_of(kind, &[0; 16], "dwm.exe") };
        vec![
            sized(guesthouse_backend::guest_text::QUERY_USER_DRIVER_NAME, 528),
            sized(api_drivers::QUERY_OPENGL, 528),
            sized(adapter_registry::QUERY_TYPE, 64),
            sized(wddm_caps::QUERY_WDDM_2_0_CAPS, 4),
            sized(wddm_caps::QUERY_WDDM_2_0_CAPS, 2),
            sized(gpu_mmu_caps::QUERY_GPU_MMU_CAPS, 12),
            sized(adapter_info::QUERY_TYPE, adapter_header::HEADER_SIZE),
            sized(adapter_info::QUERY_TYPE, 1),
            sized(u32::MAX, 4),
        ]
    }

    #[test]
    fn a_query_is_answered_the_same_whether_or_not_it_is_logged() {
        let info = crate::DeviceInfo::default();
        let mut answers = 0;
        for query in queries() {
            let mut alone = vec![0xA5; 4];
            let status = crate::own_answer(&info, &query, &mut alone);
            answers += usize::from(status.is_some());

            let mut quiet = vec![0xA5; 4];
            assert_eq!(crate::answer_query(&info, &query, &mut quiet), status);
            assert_eq!(quiet, alone, "with debug lines off");

            let mut watched = vec![0xA5; 4];
            let lines =
                lines::logged(|| assert_eq!(crate::answer_query(&info, &query, &mut watched), status));
            assert_eq!(watched, alone, "with debug lines on");

            // The modules that answer have lines of their own, which come first.
            let kind = query.kind;
            let [.., line, dump] = lines::at(Level::Debug, &lines)[..] else { panic!("{lines:?}") };
            assert!(line.starts_with(&format!("amd query kind={kind} size=")), "{lines:?}");
            assert_eq!(dump, format!("amd dump query kind={kind} length=16 bytes={}", "00".repeat(16)));
            assert_eq!(line.contains("answer=left-to-the-core"), status.is_none(), "{lines:?}");
            let ours = lines.iter().filter(|(_, text)| text.starts_with("amd ")).count();
            assert_eq!(ours, 2, "{lines:?}");
        }
        assert_eq!(answers, queries().len() - 1, "only the last is left to the core");
    }

    fn work_with(private: &[u8]) -> Work<'_> {
        Work {
            source: WorkSource::HardwareQueue,
            queue: 0x4000_0010,
            space: guesthouse_backend::SpaceId(3),
            node: 1,
            client_hint: 0x6,
            context_queues: 1,
            private,
            commands: None,
            fence: 0,
            fence_gpu_address: 0,
            fence_value: 12,
            may_run: true,
            queue_failed: false,
        }
    }

    /// A submission's private data: client 2, with a DMA buffer record and a tracking record.
    fn submission_data() -> Vec<u8> {
        const DMA_BUFFER: (u32, usize) = (0, 0x40);
        const DMA_BUFFER_BYTES_AT: usize = 0x08;
        const DMA_BUFFER_ADDRESS_AT: usize = 0x18;
        const TRACKING: (u32, usize) = (8, 0x20);
        const HEADER: usize = 8;
        let mut data = vec![0u8; HEADER + DMA_BUFFER.1 + TRACKING.1];
        put_dword(&mut data, 0, 2);
        put_dword(&mut data, 4, 2);
        for (at, (opcode, length)) in [(HEADER, DMA_BUFFER), (HEADER + DMA_BUFFER.1, TRACKING)] {
            put_dword(&mut data, at, opcode);
            put_dword(&mut data, at + 4, length as u32);
        }
        put_dword(&mut data, HEADER + DMA_BUFFER_BYTES_AT, 0x40);
        put_qword(&mut data, HEADER + DMA_BUFFER_ADDRESS_AT, 0x10_0000);
        data
    }

    #[test]
    fn a_submission_says_what_the_check_read_and_which_buffers_it_names() {
        let data = submission_data();
        let submission = command_data::check(&data).unwrap();
        let buffers: Vec<Ib> = command_data::command_buffers(&data)
            .into_iter()
            .map(|buffer| Ib { address: buffer.address, bytes: buffer.bytes })
            .collect();
        let work = work_with(&data);
        let line = SubmitLine { work: &work, engine_family: 2, submission: &submission, buffers: &buffers };
        assert_eq!(
            line.to_string(),
            "amd submit source=hardware-queue queue=0x40000010 node=1 hint=0x6 fence_value=12 length=104 \
             check=taken client=2 opcodes=[0 8] engine_family=2 space=3 buffers=[0x100000:0x40] may_run=1 \
             queue_failed=0"
        );
        let two = [buffers[0], Ib { address: 0x20_0000, bytes: 0x80 }];
        let line = SubmitLine { work: &work, engine_family: 2, submission: &submission, buffers: &two };
        assert!(line.to_string().contains(" buffers=[0x100000:0x40 0x200000:0x80] "), "{line}");
    }

    #[test]
    fn the_empty_marker_and_a_context_submission_that_may_not_run_read_plainly() {
        let data = [0xAA, 0, 0, 0, 0, 0, 0, 0];
        let submission = command_data::check(&data).unwrap();
        let work =
            Work { source: WorkSource::Context, may_run: false, queue_failed: true, ..work_with(&data) };
        let line = SubmitLine { work: &work, engine_family: 0, submission: &submission, buffers: &[] };
        assert_eq!(
            line.to_string(),
            "amd submit source=context queue=0x40000010 node=1 hint=0x6 fence_value=12 length=8 check=taken \
             client=170 opcodes=[] engine_family=0 space=3 buffers=[] may_run=0 queue_failed=1"
        );
    }

    #[test]
    fn a_refused_submission_shows_why_and_how_it_starts() {
        let mut data = submission_data();
        put_dword(&mut data, 8, 7);
        let why = command_data::check(&data).unwrap_err().0;
        let work = work_with(&data);
        assert_eq!(
            RefusedSubmitLine { work: &work, why }.to_string(),
            "amd submit source=hardware-queue queue=0x40000010 node=1 hint=0x6 fence_value=12 length=104 \
             check=refused why=\"the opcode is unknown\" first=[0x2 0x2 0x7 0x40 0x40 0x0 0x0 0x0 0x100000 \
             0x0 0x0 0x0 0x0 0x0 0x0 0x0]"
        );
        let short = work_with(&[1, 2, 3]);
        let line = RefusedSubmitLine { work: &short, why: "the header is cut short" }.to_string();
        assert!(line.ends_with(" length=3 check=refused why=\"the header is cut short\" first=[0x30201]"));
    }

    #[test]
    fn a_submission_has_one_line_and_none_with_debug_lines_off() {
        let data = submission_data();
        let submission = command_data::check(&data).unwrap();
        let work = work_with(&data);
        let both = || {
            super::submission(&work, 0, &submission, &[]);
            submission_refused(&work, "the opcode is unknown");
        };
        both();
        let lines = lines::logged(both);
        let debug = lines::at(Level::Debug, &lines);
        assert_eq!(debug.len(), 2, "{lines:?}");
        assert!(debug[0].contains(" check=taken "), "{lines:?}");
        assert!(debug[1].contains(" check=refused "), "{lines:?}");
        assert!(lines::logged(|| {}).is_empty(), "a line from before was kept");
    }

    /// Two command buffers as the daemon lays them out for the video ring.
    fn copied_video_work() -> Copied {
        let mut copied = Copied::default();
        copied.place(8, 256).unwrap().copy_from_slice(&[0x10, 0x3000_0001]);
        copied.place(4, 256).unwrap().copy_from_slice(&[0xAABB_CCDD]);
        copied
    }

    #[test]
    fn a_video_run_says_how_it_ended_and_how_long_it_took() {
        let data = submission_data();
        let (work, copied) = (work_with(&data), copied_video_work());
        let line = |ran: &io::Result<Ran>| {
            VideoLine { work: &work, buffers: copied.runs(), took: Duration::from_micros(1900), ran }
                .to_string()
        };
        assert_eq!(
            line(&Ok(Ran::Retired)),
            "amd video queue=0x40000010 fence_value=12 space=3 buffers=[0x100080000:0x8 0x100080100:0x4] \
             took_us=1900 ran=retired"
        );
        assert!(line(&Ok(Ran::Refused)).ends_with(" ran=refused"));
        assert!(line(&Ok(Ran::Hung)).ends_with(" ran=hung"));
        let fault = crate::fault::Fault { address: 0x3000, status: 0x40_0c30 };
        assert!(line(&Ok(Ran::Faulted(fault))).ends_with(" ran=faulted address=0x3000 status=0x400c30"));
        let error = Err(io::Error::other("its commands are too long"));
        assert!(line(&error).ends_with(" ran=not-run why=\"its commands are too long\""));
    }

    #[test]
    fn a_video_run_has_a_dump_for_each_buffer_and_a_line_and_nothing_with_debug_lines_off() {
        let data = submission_data();
        let (work, copied) = (work_with(&data), copied_video_work());
        let both = || {
            video_commands(&work, &copied);
            video_ran(&work, &copied, Duration::ZERO, &Ok(Ran::Retired));
        };
        both();
        let lines = lines::logged(both);
        assert_eq!(
            lines::at(Level::Debug, &lines),
            [
                "amd dump video queue=0x40000010 fence_value=12 buffer=0 length=8 bytes=1000000001000030",
                "amd dump video queue=0x40000010 fence_value=12 buffer=1 length=4 bytes=ddccbbaa",
                "amd video queue=0x40000010 fence_value=12 space=3 buffers=[0x100080000:0x8 0x100080100:0x4] \
                 took_us=0 ran=retired",
            ]
        );
        assert!(lines::logged(|| {}).is_empty(), "a line from before was kept");
    }

    /// A card with all three engine families, so the guest's nodes are graphics, compute and DMA.
    fn full_card() -> crate::DeviceInfo {
        let engines = crate::EngineSet { graphics: true, compute: true, dma: true, video: false };
        crate::DeviceInfo { engines, ..Default::default() }
    }

    /// What the parts say of a submission with nothing logged: the engine and the buffers, unless
    /// the check refuses it or the card has no such node.
    fn read_alone(info: &crate::DeviceInfo, work: &Work<'_>) -> Option<(crate::Node, Vec<Ib>)> {
        let buffers = command_data::command_buffers(work.private)
            .into_iter()
            .map(|buffer| Ib { address: buffer.address, bytes: buffer.bytes })
            .collect();
        let engine = crate::nodes::nodes(&info.engines).get(work.node as usize).copied();
        command_data::check(work.private).ok().and(engine).map(|engine| (engine, buffers))
    }

    /// A submission to read, the warning it should bring, and what its debug line should hold.
    struct Read<'a> {
        private: &'a [u8],
        node: u32,
        warning: Option<&'a str>,
        line_holds: Option<&'a str>,
    }

    #[test]
    fn a_submission_is_read_the_same_whether_or_not_it_is_logged() {
        const NO_SUCH_NODE: u32 = 3;
        let info = full_card();
        let taken = submission_data();
        let empty_marker = [0xAA, 0, 0, 0, 0, 0, 0, 0];
        let mut refused = submission_data();
        put_dword(&mut refused, 8, 7);
        let cases = [
            Read {
                private: &taken,
                node: 1,
                warning: None,
                line_holds: Some(" check=taken client=2 opcodes=[0 8] engine_family=1 "),
            },
            Read {
                private: &empty_marker,
                node: 0,
                warning: None,
                line_holds: Some(" check=taken client=170 opcodes=[] engine_family=0 "),
            },
            Read {
                private: &refused,
                node: 1,
                warning: Some("refused a submission on queue 0x40000010: the opcode is unknown"),
                line_holds: Some(" check=refused why=\"the opcode is unknown\" first=[0x2 0x2 0x7 "),
            },
            Read {
                private: &taken,
                node: NO_SUCH_NODE,
                warning: Some("work for node 3, which this card hasn't got"),
                line_holds: None,
            },
        ];
        for Read { private, node, warning, line_holds } in cases {
            let work = Work { node, ..work_with(private) };
            let alone = read_alone(&info, &work);
            assert_eq!(alone.is_some(), warning.is_none());
            assert_eq!(crate::submission_read(&info, &work), alone, "with debug lines off");
            let lines = lines::logged(|| assert_eq!(crate::submission_read(&info, &work), alone));

            // The warning is worded as it was, and comes before the line.
            let warnings: Vec<&str> = warning.into_iter().collect();
            assert_eq!(lines::at(Level::Warn, &lines), warnings, "{lines:?}");
            let debug = lines::at(Level::Debug, &lines);
            assert_eq!(debug.len(), usize::from(line_holds.is_some()), "{lines:?}");
            if let Some(holds) = line_holds {
                assert!(debug[0].starts_with("amd submit source=hardware-queue "), "{lines:?}");
                assert!(debug[0].contains(&format!(" node={node} ")), "{lines:?}");
                assert!(debug[0].contains(holds), "{lines:?}");
                assert_eq!(lines.last().unwrap().1, debug[0], "{lines:?}");
            }
            assert_eq!(lines.len(), warnings.len() + debug.len(), "{lines:?}");
        }
        let (engine, buffers) = read_alone(&info, &work_with(&taken)).unwrap();
        assert_eq!(engine.ip_type, crate::info::HW_IP_COMPUTE);
        assert_eq!(buffers, [Ib { address: 0x10_0000, bytes: 0x40 }]);
    }

    #[test]
    fn a_registry_lookup_left_to_the_core_has_no_line_and_another_query_left_to_it_has() {
        let info = crate::DeviceInfo::default();
        let lookup = query_of(AdapterQuery::REGISTRY, &[0x5A; 16], "dwm.exe");
        let mut out = vec![0xA5; 4];
        let lines = lines::logged(|| assert_eq!(crate::answer_query(&info, &lookup, &mut out), None));
        assert_eq!(out, [0xA5; 4]);
        assert!(!lines.iter().any(|(_, text)| text.starts_with("amd ")), "{lines:?}");

        let other = VendorQuery { kind: u32::MAX, ..lookup };
        let lines = lines::logged(|| assert_eq!(crate::answer_query(&info, &other, &mut out), None));
        assert_eq!(out, [0xA5; 4]);
        let [line, dump] = lines::at(Level::Debug, &lines)[..] else { panic!("{lines:?}") };
        assert!(line.starts_with("amd query kind=4294967295 size=16 "), "{lines:?}");
        assert!(line.contains(" answer=left-to-the-core "), "{lines:?}");
        assert_eq!(dump, format!("amd dump query kind=4294967295 length=16 bytes={}", "5a".repeat(16)));

        // Being left to the core is what keeps a lookup out: one this backend answered has its lines.
        let lines = lines::logged(|| query(&lookup, Some(Status::SUCCESS)));
        let [line, _dump] = lines::at(Level::Debug, &lines)[..] else { panic!("{lines:?}") };
        assert!(line.starts_with("amd query kind=48 size=16 "), "{lines:?}");
    }

    #[test]
    fn a_registry_value_the_backend_gives_is_named_and_one_it_has_not_is_left_to_the_core() {
        use guesthouse_messages::registry::TYPE_STRING;
        let vulkan = RegistryName::Value("vulkandrivername".into());
        let given = crate::registry_answer(&vulkan);
        let lines = lines::logged(|| assert_eq!(crate::registry_answer(&vulkan), given));
        let bytes = given.expect("the backend has this value").bytes.len();
        let line = format!("amd registry name=\"vulkandrivername\" type={TYPE_STRING} bytes={bytes}");
        assert_eq!(lines, [(Level::Debug, line)]);

        let store = RegistryName::DriverStorePath;
        let given = crate::registry_answer(&store);
        let lines = lines::logged(|| assert_eq!(crate::registry_answer(&store), given));
        let bytes = given.expect("the backend has this path").bytes.len();
        assert_eq!(
            lines,
            [(Level::Debug, format!("amd registry name=\"<driver store path>\" type=- bytes={bytes}"))]
        );

        let unknown = RegistryName::Value("opencldrivername".into());
        let lines = lines::logged(|| assert_eq!(crate::registry_answer(&unknown), None));
        assert!(lines.is_empty(), "{lines:?}");
    }

    /// A context's header, with its size, its flags, its session flags and its client id.
    fn context_data(flags: u32, session_flags: u32, client_id: u32) -> Vec<u8> {
        const SIZE: usize = 0x40;
        const FLAGS_AT: usize = 0x04;
        const SESSION_FLAGS_AT: usize = 0x14;
        const CLIENT_ID_AT: usize = 0x34;
        let mut data = vec![0u8; SIZE];
        put_dword(&mut data, 0, SIZE as u32);
        put_dword(&mut data, FLAGS_AT, flags);
        put_dword(&mut data, SESSION_FLAGS_AT, session_flags);
        put_dword(&mut data, CLIENT_ID_AT, client_id);
        data
    }

    const HAS_CLIENT_ID: u32 = 1 << 18;

    fn context_line(private: &[u8]) -> String {
        ContextLine { process: PROCESS, node: 2, private, checked: &context::check(private) }.to_string()
    }

    #[test]
    fn a_context_says_what_the_check_read_from_its_header() {
        assert_eq!(
            context_line(&context_data(HAS_CLIENT_ID | 1 << 1, 1 << 4, 3)),
            "amd context process=0x40000002 node=2 length=64 check=taken flags=0x40002 session_flags=0x10 \
             client_id=3 extension=0"
        );
        assert_eq!(
            context_line(&context_data(0, 0, 0)),
            "amd context process=0x40000002 node=2 length=64 check=taken flags=0x0 session_flags=0x0 \
             client_id=- extension=0"
        );
    }

    #[test]
    fn a_refused_context_still_shows_its_flag_dwords() {
        assert_eq!(
            context_line(&context_data(0, 1, 0)),
            "amd context process=0x40000002 node=2 length=64 check=refused why=\"a display encoding session \
             is asked for\" flags=0x0 session_flags=0x1 client_id=- extension=-"
        );
    }

    #[test]
    fn a_context_too_short_for_a_header_is_taken_unread_and_names_nothing() {
        assert_eq!(
            context_line(&[0xFF; 12]),
            "amd context process=0x40000002 node=2 length=12 check=unread flags=- session_flags=- \
             client_id=- extension=-"
        );
    }

    /// A queue's private data with its description pointer set, and padding after it.
    fn queue_data() -> Vec<u8> {
        const DESCRIPTION_POINTER_AT: usize = 0x30;
        let mut data = vec![0u8; 0x68];
        put_qword(&mut data, DESCRIPTION_POINTER_AT, 0x7FF0_0000_1000);
        data
    }

    #[test]
    fn a_queue_says_how_its_check_came_out() {
        let taken = QueueLine { queue: 0x4000_0020, serial: 1, bytes_with_padding: 0x68, checked: Ok(()) };
        assert_eq!(taken.to_string(), "amd queue id=0x40000020 serial=1 bytes_with_padding=104 check=taken");
        let checked = hw_queue::check(&[0; 0x68]);
        let refused = QueueLine { queue: 0x4000_0020, serial: 2, bytes_with_padding: 0x68, checked };
        assert_eq!(
            refused.to_string(),
            "amd queue id=0x40000020 serial=2 bytes_with_padding=104 check=refused why=\"the queue has no \
             description\""
        );
    }

    #[test]
    fn contexts_and_queues_are_judged_the_same_whether_or_not_they_are_logged() {
        let contexts = [context_data(HAS_CLIENT_ID, 0, 3), context_data(0, 1, 0), vec![0xFF; 12]];
        for private in &contexts {
            let alone = context::check(private).is_ok();
            assert_eq!(crate::context_taken(PROCESS, 2, private), alone, "with debug lines off");
            let lines = lines::logged(|| assert_eq!(crate::context_taken(PROCESS, 2, private), alone));
            let hex: String = private.iter().map(|byte| format!("{byte:02x}")).collect();
            let length = private.len();
            let [line, dump] = lines::at(Level::Debug, &lines)[..] else { panic!("{lines:?}") };
            assert!(line.starts_with("amd context process=0x40000002 node=2 "), "{lines:?}");
            assert_eq!(
                dump,
                format!("amd dump context process=0x40000002 node=2 length={length} bytes={hex}")
            );
            // The refusal's own line is as it was.
            let info = lines::at(Level::Info, &lines);
            let refusal = "refused a context on node 2: a display encoding session is asked for";
            assert_eq!(info, if alone { vec![] } else { vec![refusal] });
        }

        for private in [queue_data(), vec![0; 0x68], vec![0; 4]] {
            let alone = hw_queue::check(&private).map_err(|refused| refused.0.to_string());
            let judged = || crate::queue_checked(0x4000_0020, 1, &private).map_err(|error| error.to_string());
            assert_eq!(judged(), alone, "with debug lines off");
            let lines = lines::logged(|| assert_eq!(judged(), alone));
            let hex: String = private.iter().map(|byte| format!("{byte:02x}")).collect();
            let length = private.len();
            let [line, dump] = lines::at(Level::Debug, &lines)[..] else { panic!("{lines:?}") };
            assert!(line.starts_with("amd queue id=0x40000020 serial=1 "), "{lines:?}");
            assert_eq!(dump, format!("amd dump queue id=0x40000020 length={length} bytes={hex}"));
            assert_eq!(lines.len(), 2);
        }
    }

    #[test]
    fn nothing_is_read_from_an_escape_with_debug_lines_off() {
        let data = escape_of(0x0300_6203, 0x20, 0x20);
        assert_eq!(escape_asked(PROCESS, &data), None);
        let lines = lines::logged(|| {
            // What the backend holds when the lines came on between the two calls.
            escape_answered(PROCESS, None, &Err(escape::Refused("unused")));
        });
        assert!(lines.is_empty());
    }

    /// Bytes that look random and are the same on every run, so the test below needs no crate for
    /// them.
    struct Scattered(u64);

    impl Scattered {
        /// An xorshift step, which scatters well enough for test bytes.
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, limit: usize) -> usize {
            (self.next() % limit as u64) as usize
        }

        fn bytes(&mut self, length: usize) -> Vec<u8> {
            (0..length).map(|_| self.next() as u8).collect()
        }

        /// What a guest could send in place of `shape`: now and then bytes of no shape at all, a
        /// few of them past what a dump shows, and otherwise the shape with a count set to the
        /// most a dword says, bytes changed, or its end cut off anywhere.
        fn spoiled(&mut self, shape: &Shape) -> Vec<u8> {
            const MOST_SHAPELESS: usize = 0x300;
            if self.below(4) == 0 {
                let most = if self.below(16) == 0 { 2 * crate::watch::DUMP_BYTES } else { MOST_SHAPELESS };
                let length = self.below(most);
                return self.bytes(length);
            }
            let mut data = shape.whole.clone();
            if !shape.counts.is_empty() && self.below(3) == 0 {
                put_dword(&mut data, shape.counts[self.below(shape.counts.len())], u32::MAX);
            }
            for _ in 0..self.below(4) {
                let at = self.below(data.len());
                data[at] = self.next() as u8;
            }
            if self.below(3) == 0 {
                data.truncate(self.below(data.len() + 1));
            }
            data
        }
    }

    /// A buffer as the guest's driver sends it, and where it keeps the counts its reader goes by.
    struct Shape {
        whole: Vec<u8>,
        counts: Vec<usize>,
    }

    fn escape_shape(code: u32, input_bytes: usize, output_bytes: usize, in_wrapper: bool) -> Shape {
        let bare = escape_of(code, input_bytes, output_bytes);
        let front = if in_wrapper { WRAPPER_SIZE } else { 0 };
        Shape {
            whole: if in_wrapper { wrapped(&bare) } else { bare },
            counts: vec![front + HEADER_SIZE, front + HEADER_SIZE + COUNT_SIZE + input_bytes],
        }
    }

    // The books answer the same whether or not their lines are kept, so two sets of them stay in
    // step however a guest spoils what it sends.
    #[test]
    fn a_video_sessions_escapes_of_any_shape_are_answered_the_same_with_the_lines_on_and_off() {
        const ROUNDS: usize = 4000;
        const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
        let shapes: Vec<Shape> = (0..3)
            .flat_map(a_video_session)
            .map(|whole| {
                let front = if escape::framing(&whole).wrapped { WRAPPER_SIZE } else { 0 };
                Shape { whole, counts: vec![front + HEADER_SIZE] }
            })
            .collect();
        let mut quiet_books = crate::video::Sessions::new(7);
        let mut watched_books = crate::video::Sessions::new(7);
        let mut random = Scattered(SEED);
        let (mut done, mut failed) = (0, 0);
        for round in 0..ROUNDS {
            // Most are sent as a driver sends them, so that sessions are open when a spoiled one comes.
            let shape = &shapes[random.below(shapes.len())];
            let sent = if round % 3 == 0 { random.spoiled(shape) } else { shape.whole.clone() };
            let mut quiet = sent.clone();
            crate::answer_escape(PROCESS, &mut quiet, Some(&mut quiet_books));
            let mut watched = sent.clone();
            let lines =
                lines::logged(|| crate::answer_escape(PROCESS, &mut watched, Some(&mut watched_books)));
            assert_eq!(watched, quiet, "escape {sent:02x?}");
            let debug = lines::at(Level::Debug, &lines);
            assert!((2..=3).contains(&debug.len()), "escape {sent:02x?} {lines:?}");
            let answered_done = debug.iter().any(|line| line.contains(" answer=done "));
            done += usize::from(answered_done);
            failed += usize::from(!answered_done);
            if round % 500 == 499 {
                quiet_books.process_gone(PROCESS);
                watched_books.process_gone(PROCESS);
            }
        }
        assert!(done > ROUNDS / 8 && failed > ROUNDS / 8, "{done} done, {failed} failed");
    }

    // The readers the lines brought run only with debug lines on, and the tests above send them what
    // a driver sends or a few things near it. This one sends them anything.
    #[test]
    fn bytes_of_any_shape_are_logged_without_a_panic_and_answered_the_same() {
        const ROUNDS: usize = 4000;
        const SEED: u64 = 0x2545_F491_4F6C_DD1D;
        /// Where a submission keeps its record count and its first record's length, and a context
        /// its size.
        const RECORD_COUNT_AT: usize = 0x04;
        const FIRST_RECORD_LENGTH_AT: usize = 0x0C;
        const CONTEXT_SIZE_AT: usize = 0x00;
        let escapes = [
            escape_shape(0x0300_5D03, 0x20, 0x60, false),
            escape_shape(0x0300_6203, 0x20, 0x20, false),
            escape_shape(0x0300_6203, 0x20, 0x20, true),
            escape_shape(0x0300_7E03, 0x100, 0x100, true),
            escape_shape(0x0200_0103, 8, 8, false),
        ];
        let submission =
            Shape { whole: submission_data(), counts: vec![RECORD_COUNT_AT, FIRST_RECORD_LENGTH_AT] };
        let context = Shape { whole: context_data(HAS_CLIENT_ID, 1 << 4, 3), counts: vec![CONTEXT_SIZE_AT] };
        let queue = Shape { whole: queue_data(), counts: Vec::new() };
        let info = full_card();
        let asked = queries();
        let mut random = Scattered(SEED);
        // What the bytes sent turned out to hold, so the test can say it met each.
        let (mut empty, mut odd, mut in_wrapper, mut greatest_count, mut read) = (0, 0, 0, 0, 0);

        for round in 0..ROUNDS {
            let sent = random.spoiled(&escapes[round % escapes.len()]);
            let framing = escape::framing(&sent);
            empty += usize::from(sent.is_empty());
            odd += sent.len() % 2;
            in_wrapper += usize::from(framing.wrapped);
            greatest_count += usize::from(framing.input == Count::PastEnd(u32::MAX));
            greatest_count += usize::from(framing.output == Count::PastEnd(u32::MAX));
            let mut quiet = sent.clone();
            crate::answer_escape(PROCESS, &mut quiet, None);
            let mut watched = sent.clone();
            let lines = lines::logged(|| crate::answer_escape(PROCESS, &mut watched, None));
            assert_eq!(watched, quiet, "escape {sent:02x?}");
            assert_eq!(lines::at(Level::Debug, &lines).len(), 2, "escape {sent:02x?}");

            let mut query = asked[round % asked.len()];
            let data = random.spoiled(&context);
            query.data = &data;
            let mut quiet = vec![0xA5; 4];
            let status = crate::answer_query(&info, &query, &mut quiet);
            let mut watched = vec![0xA5; 4];
            lines::logged(|| assert_eq!(crate::answer_query(&info, &query, &mut watched), status));
            assert_eq!(watched, quiet, "query {} {data:02x?}", query.kind);

            let private = random.spoiled(&submission);
            let work = Work { node: random.below(4) as u32, ..work_with(&private) };
            let quiet = crate::submission_read(&info, &work);
            read += usize::from(quiet.is_some());
            lines::logged(|| assert_eq!(crate::submission_read(&info, &work), quiet, "{private:02x?}"));
            let lines = lines::logged(|| submission_refused(&work, "whatever the check said"));
            assert_eq!(lines::at(Level::Debug, &lines).len(), 1, "submission {private:02x?}");

            let private = random.spoiled(&context);
            let node = random.below(4) as u32;
            let quiet = crate::context_taken(PROCESS, node, &private);
            let lines = lines::logged(|| assert_eq!(crate::context_taken(PROCESS, node, &private), quiet));
            assert_eq!(lines::at(Level::Debug, &lines).len(), 2, "context {private:02x?}");

            let private = random.spoiled(&queue);
            let judged = || crate::queue_checked(0x4000_0020, 1, &private).map_err(|error| error.to_string());
            let quiet = judged();
            let lines = lines::logged(|| assert_eq!(judged(), quiet));
            assert_eq!(lines::at(Level::Debug, &lines).len(), 2, "queue {private:02x?}");
        }
        let met = [empty, odd, in_wrapper, greatest_count, read, ROUNDS - read];
        assert!(met.iter().all(|&times| times > 0), "{met:?}");
    }
}
