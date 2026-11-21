//! Closing an encode session the guest leaves open in the card's firmware, and resting the video
//! engine before a new one: two switches under [`crate::video`]'s, each off unless the daemon is
//! started with it, and neither any use without that one. Without them nothing here is used.
//!
//! They are a stopgap, kept apart so that it is one module to drop. AMD's driver in the guest sends
//! no work at an encoder's end, where Mesa's sends one last command buffer that closes the session
//! in the firmware. A guest program that opened a second encoder a tenth of a second after its
//! first, with its context buffer at the same GPU address, has had the firmware write at address
//! zero, and amdgpu reset the video ring for everyone on the host. Neither switch is something
//! AMD's kernel driver does, and why the guest's firmware state goes wrong isn't understood. They
//! are what has kept a guest from that reset so far.
//!
//! `GUESTHOUSE_AMD_VIDEO_CLOSE=1` has the daemon close such a session itself. It keeps what each
//! queue's encode commands say of their session, once their run has reached the engine, and runs
//! the closing commands, shaped as Mesa's are, when the session is over: at the escape that ends a
//! video session, for every session whose context buffer the guest has freed by then, as the escape
//! comes after that and the daemon can't tell from it which session of the firmware's it is; and
//! when the queue goes. A session whose commands never named its context buffer is closed only
//! when its queue goes, as the end of another's session would otherwise close it while it runs.
//! The firmware reads the session's buffer to close it, so a session whose buffer is no longer
//! mapped is left alone.
//!
//! `GUESTHOUSE_AMD_VIDEO_REST` is a time in milliseconds the video engine is left idle before a
//! new session's first work, counted from the engine's last work, when no other queue has a session
//! open. amdgpu powers the engine down after a second without work, so a session begun after a
//! longer rest meets firmware that has started afresh. The rest is a sleep on the daemon's message
//! thread: nothing else of the guest's is served meanwhile.
//!
//! The close alone was not enough in the runs so far, and the close with a rest of 1500 ms was.

use crate::protocol::escape;
use crate::video_ib::Copied;
use guesthouse_backend::SpaceId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The daemon's switch for closing the sessions a guest leaves open.
pub const CLOSE_VARIABLE: &str = "GUESTHOUSE_AMD_VIDEO_CLOSE";
/// The daemon's switch for resting the engine before a new session, with the rest in milliseconds.
pub const REST_VARIABLE: &str = "GUESTHOUSE_AMD_VIDEO_REST";

/// How much of a session's buffer must still be mapped for the session to be closed: what Mesa
/// makes one of.
pub const SESSION_BYTES: u64 = 128 * 1024;

// The firmware's encode commands, as far as a session is read from them and closed with them. A
// command buffer is packets, each its size in bytes, its kind, and its words.
const PACKET_HEADER_WORDS: usize = 2;
/// The packet that says which of the firmware's engines the buffer is for, and its word for encode.
const ENGINE_INFO: u32 = 0x3000_0001;
const ENGINE_ENCODE: u32 = 2;
/// The session's packet: a version, its buffer's address in two halves, and one word more.
const SESSION_INFO: u32 = 0x1;
const SESSION_INFO_WORDS: usize = 4;
/// The task's packet: the size of the task, its number, and whether it wants feedback.
const TASK_INFO: u32 = 0x2;
/// The packet that names the context buffer, which holds the session's reference pictures.
const CONTEXT_BUFFER: u32 = 0x11;
const OP_INITIALIZE: u32 = 0x0100_0001;
const OP_CLOSE_SESSION: u32 = 0x0100_0002;
/// The closing commands: engine info, session info, task info and the operation.
pub const CLOSE_WORDS: usize = 17;

/// The escape a guest's driver sends from user mode when a video session is over: service 3,
/// subfunction 0x4E.
const SESSION_OVER_ESCAPE: u32 = 0x0300_4E03;

/// Whether an escape, bare or in its wrapper, is the one that says a video session is over. The
/// payload isn't looked at: the escape names no session of the firmware's either way.
pub fn says_a_session_is_over(data: &[u8]) -> bool {
    escape::framing(data).code_sent == Some(SESSION_OVER_ESCAPE)
}

/// The rest the switch's value asks for. Zero and an empty value are no rest, and so is a value
/// that is no number of milliseconds, which is said in a warning.
pub fn rest_from(value: Option<&str>) -> Option<Duration> {
    let text = value?.trim();
    let millis = text.parse::<u64>().ok();
    if millis.is_none() && !text.is_empty() {
        log::warn!("{REST_VARIABLE} is {text}, which isn't a number of milliseconds");
    }
    millis.filter(|&millis| millis > 0).map(Duration::from_millis)
}

/// What an encode command buffer says of its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// The session info's words as the guest sent them.
    pub info: [u32; SESSION_INFO_WORDS],
    /// The number of the task these commands are.
    pub task: u32,
    /// Whether the commands close the session themselves.
    pub closed: bool,
    /// Whether the commands start the session.
    pub starts: bool,
    /// Where the session's context buffer is, if these commands name it.
    pub context: Option<u64>,
}

impl Session {
    /// Where the session's buffer is.
    pub fn address(&self) -> u64 {
        (u64::from(self.info[1]) << 32) | u64::from(self.info[2])
    }
}

/// Reads the session out of one command buffer, if it is encode work that names one.
pub fn session_in(commands: &[u32]) -> Option<Session> {
    let (mut encode, mut info, mut task, mut closed) = (false, None, 0, false);
    let (mut starts, mut context) = (false, None);
    let mut at = 0;
    while at + PACKET_HEADER_WORDS <= commands.len() {
        let words = commands[at] as usize / size_of::<u32>();
        if words < PACKET_HEADER_WORDS || at + words > commands.len() {
            break;
        }
        let body = &commands[at + PACKET_HEADER_WORDS..at + words];
        match commands[at + 1] {
            ENGINE_INFO => encode = body.first() == Some(&ENGINE_ENCODE),
            SESSION_INFO if body.len() >= SESSION_INFO_WORDS => {
                info = Some([body[0], body[1], body[2], body[3]]);
            }
            TASK_INFO if body.len() >= 2 => task = body[1],
            OP_CLOSE_SESSION => closed = true,
            OP_INITIALIZE => starts = true,
            CONTEXT_BUFFER if body.len() >= 2 => {
                context = Some((u64::from(body[0]) << 32) | u64::from(body[1]));
            }
            _ => {}
        }
        at += words;
    }
    info.filter(|_| encode).map(|info| Session { info, task, closed, starts, context })
}

/// The commands that close a session, shaped as Mesa's are: the task's size counts the task info
/// and the operation, and the task is the one after the session's last.
pub fn close_commands(session: &Session) -> [u32; CLOSE_WORDS] {
    let [version, high, low, last] = session.info;
    let bytes = |words: usize| (words * size_of::<u32>()) as u32;
    [
        bytes(4),
        ENGINE_INFO,
        ENGINE_ENCODE,
        bytes(CLOSE_WORDS),
        bytes(6),
        SESSION_INFO,
        version,
        high,
        low,
        last,
        bytes(5),
        TASK_INFO,
        bytes(5 + 2),
        session.task.wrapping_add(1),
        0,
        bytes(2),
        OP_CLOSE_SESSION,
    ]
}

/// What one submission's commands say of their queue's session, read before the run and kept once
/// the run has reached the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    queue: u32,
    space: SpaceId,
    session: Session,
}

/// What the daemon keeps of the firmware's encode sessions, for the two switches.
#[derive(Debug)]
pub struct Watch {
    closes: bool,
    rest: Option<Duration>,
    /// The session each queue's commands have named and not closed, with its space.
    open: HashMap<u32, (SpaceId, Session)>,
    /// When the video engine last had work.
    last_run: Option<Instant>,
}

impl Watch {
    /// `None` when neither switch is on, as nothing is kept then.
    pub fn new(closes: bool, rest: Option<Duration>) -> Option<Self> {
        (closes || rest.is_some()).then(|| Self { closes, rest, open: HashMap::new(), last_run: None })
    }

    /// Whether the daemon closes the sessions a guest leaves open.
    pub fn closes(&self) -> bool {
        self.closes
    }

    /// Reads what a queue's commands, about to be run, say of its session, and how long the engine
    /// is to rest before them: only ahead of commands that start a session, with no other queue's
    /// session open, as that one's work would end the rest anyway. Nothing is kept yet, as the
    /// firmware knows of none of it until the run has reached it: see [`Self::reached`].
    pub fn before(
        &self,
        queue: u32,
        space: SpaceId,
        copied: &Copied,
        now: Instant,
    ) -> (Option<Seen>, Duration) {
        let Some(session) = copied.buffers().filter_map(session_in).last() else {
            return (None, Duration::ZERO);
        };
        let starts = session.starts && !session.closed;
        let rest = if starts { self.rest_before(queue, now) } else { Duration::ZERO };
        (Some(Seen { queue, space, session }), rest)
    }

    /// What is left of the rest, counted from the engine's last work.
    fn rest_before(&self, queue: u32, now: Instant) -> Duration {
        let (Some(rest), Some(last)) = (self.rest, self.last_run) else {
            return Duration::ZERO;
        };
        if self.open.keys().any(|&open| open != queue) {
            log::debug!("amd video: no rest before the session of queue {queue:#x}, as others are open");
            return Duration::ZERO;
        }
        rest.saturating_sub(now.saturating_duration_since(last))
    }

    /// Notes that a run has reached the video engine, which has had work now, and keeps what the
    /// run's commands said of their session, if they named one. A run that never got there leaves
    /// the books as they were: the firmware has no session from it to close.
    pub fn reached(&mut self, seen: Option<Seen>, now: Instant) {
        self.last_run = Some(now);
        let Some(Seen { queue, space, mut session }) = seen else { return };
        if session.closed {
            self.open.remove(&queue);
            return;
        }
        // Work after the first needn't name the context buffer again.
        let before = self.open.get(&queue).and_then(|(_, before)| before.context);
        session.context = session.context.or(before);
        self.open.insert(queue, (space, session));
    }

    /// Takes a queue's session off the books, to close it or because its queue has gone.
    pub fn take(&mut self, queue: u32) -> Option<(SpaceId, Session)> {
        self.open.remove(&queue)
    }

    /// The queues whose session has ended: those whose context buffer `mapped` no longer finds. A
    /// guest's driver frees that buffer before it says a session is over. A session whose commands
    /// never named its context buffer can't be told from one that is still running, so it is never
    /// among them, and waits for its queue to go.
    pub fn ended(&self, mapped: impl Fn(SpaceId, u64) -> bool) -> Vec<u32> {
        let ended = self
            .open
            .iter()
            .filter(|(_, (space, session))| session.context.is_some_and(|context| !mapped(*space, context)));
        ended.map(|(&queue, _)| queue).collect()
    }

    /// Forgets the sessions of a space that has closed, whose buffers went with it.
    pub fn space_gone(&mut self, space: SpaceId) {
        self.open.retain(|_, (of, _)| *of != space);
    }

    /// Forgets every session: the guest has restarted, and its old queues and spaces will never
    /// be said to have gone. When the engine last had work is the card's to know, and stays.
    pub fn all_gone(&mut self) {
        self.open.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE: SpaceId = SpaceId(3);
    const QUEUE: u32 = 0x4000_0010;
    const OTHER_QUEUE: u32 = 0x4000_0020;
    const CONTEXT: u64 = 0x4_a1e0_0000;
    const REST: Duration = Duration::from_millis(1500);

    /// The start of encode work as a guest sends it: engine info, session info, task info, the
    /// operation that starts a session, and the context buffer.
    fn work() -> Vec<u32> {
        vec![
            0x10,
            ENGINE_INFO,
            ENGINE_ENCODE,
            0x34,
            0x18,
            SESSION_INFO,
            0x1_0018,
            4,
            0xa20b_0000,
            1,
            0x14,
            TASK_INFO,
            0x1c,
            7,
            1,
            0x8,
            OP_INITIALIZE,
            0x14,
            CONTEXT_BUFFER,
            4,
            0xa1e0_0000,
            2,
        ]
    }

    /// A later frame's work: the same session, the next task, and no start or context buffer.
    fn later_work() -> Vec<u32> {
        let mut later = work()[..15].to_vec();
        later[13] = 8;
        later
    }

    /// A submission as the backend runs it, when its run reaches the engine as soon as any rest
    /// is over. Returns the rest.
    fn run(watch: &mut Watch, queue: u32, space: SpaceId, copied: &Copied, now: Instant) -> Duration {
        let (seen, rest) = watch.before(queue, space, copied, now);
        watch.reached(seen, now + rest);
        rest
    }

    fn copied(buffers: &[&[u32]]) -> Copied {
        let mut copied = Copied::default();
        for buffer in buffers {
            copied.place(size_of_val(*buffer) as u32, 256).unwrap().copy_from_slice(buffer);
        }
        copied
    }

    #[test]
    fn encode_work_names_its_session_and_task() {
        let session = session_in(&work()).unwrap();
        assert_eq!(session.address(), 0x4_a20b_0000);
        assert_eq!(session.task, 7);
        assert!(!session.closed);
        assert!(session.starts);
        assert_eq!(session.context, Some(CONTEXT));
    }

    #[test]
    fn decode_work_and_cut_off_work_name_no_session() {
        let mut decode = work();
        decode[2] = 3;
        assert_eq!(session_in(&decode), None);
        assert_eq!(session_in(&work()[..7]), None);
        assert_eq!(session_in(&[0, 0, 0]), None);
        assert_eq!(session_in(&[]), None);
        // A packet that says it is longer than what is left ends the reading.
        let mut long = work();
        long[4] = 0x1000;
        assert_eq!(session_in(&long), None);
    }

    #[test]
    fn the_closing_commands_are_read_back_as_closing_the_same_session() {
        let session = session_in(&work()).unwrap();
        let close = close_commands(&session);
        assert_eq!(close[3], 0x44);
        assert_eq!(close[12], 0x1c);
        let read = session_in(&close).unwrap();
        assert_eq!(read.address(), session.address());
        assert_eq!(read.task, 8);
        assert!(read.closed);
        assert!(!read.starts);
        assert_eq!(read.context, None);
    }

    #[test]
    fn the_escape_that_ends_a_session_is_known_bare_and_in_its_wrapper() {
        use crate::protocol::bytes::put_dword;
        const CODE_AT: usize = 0x08;
        let mut bare = vec![0u8; escape::HEADER_SIZE + 0x40];
        put_dword(&mut bare, CODE_AT, SESSION_OVER_ESCAPE);
        assert!(says_a_session_is_over(&bare));
        let mut wrapped = vec![0u8; escape::WRAPPER_SIZE];
        put_dword(&mut wrapped, 0, escape::WRAPPER_KIND);
        put_dword(&mut wrapped, 4, escape::WRAPPER_VERSION);
        wrapped.extend_from_slice(&bare);
        assert!(says_a_session_is_over(&wrapped));
        // The one that starts a session, the same subfunction from another client, and no escape.
        for other in [0x0300_4D03, 0x0300_4E05, 0] {
            put_dword(&mut bare, CODE_AT, other);
            assert!(!says_a_session_is_over(&bare), "{other:#x}");
        }
        assert!(!says_a_session_is_over(&bare[..escape::HEADER_SIZE - 1]));
    }

    #[test]
    fn the_rest_is_a_number_of_milliseconds_and_anything_else_is_none() {
        assert_eq!(rest_from(None), None);
        assert_eq!(rest_from(Some("1500")), Some(REST));
        assert_eq!(rest_from(Some(" 250 ")), Some(Duration::from_millis(250)));
        assert_eq!(rest_from(Some("0")), None);
        let lines = crate::watch::lines::logged(|| {
            assert_eq!(rest_from(Some("1.5s")), None);
            assert_eq!(rest_from(Some("")), None);
        });
        assert_eq!(
            crate::watch::lines::at(log::Level::Warn, &lines),
            ["GUESTHOUSE_AMD_VIDEO_REST is 1.5s, which isn't a number of milliseconds"]
        );
    }

    #[test]
    fn nothing_is_kept_with_neither_switch_on() {
        assert!(Watch::new(false, None).is_none());
        assert!(Watch::new(true, None).is_some_and(|watch| watch.closes()));
        assert!(Watch::new(false, Some(REST)).is_some_and(|watch| !watch.closes()));
    }

    #[test]
    fn a_queues_session_is_kept_with_its_context_buffer_until_the_guest_frees_it() {
        let mut watch = Watch::new(true, None).unwrap();
        let now = Instant::now();
        run(&mut watch, QUEUE, SPACE, &copied(&[&work()]), now);
        // The later work names no context buffer, and the session keeps the one it had.
        run(&mut watch, QUEUE, SPACE, &copied(&[&later_work()]), now);
        assert!(watch.ended(|space, address| (space, address) == (SPACE, CONTEXT)).is_empty());
        assert_eq!(watch.ended(|_, _| false), [QUEUE]);
        let (space, session) = watch.take(QUEUE).unwrap();
        assert_eq!((space, session.task, session.context), (SPACE, 8, Some(CONTEXT)));
        assert_eq!(watch.take(QUEUE), None);
        assert!(watch.ended(|_, _| false).is_empty());
    }

    #[test]
    fn a_session_the_guest_closes_itself_and_one_whose_space_has_gone_are_forgotten() {
        let mut watch = Watch::new(true, None).unwrap();
        let now = Instant::now();
        run(&mut watch, QUEUE, SPACE, &copied(&[&work()]), now);
        let close = close_commands(&session_in(&work()).unwrap());
        run(&mut watch, QUEUE, SPACE, &copied(&[&close]), now);
        assert_eq!(watch.take(QUEUE), None);

        run(&mut watch, QUEUE, SPACE, &copied(&[&work()]), now);
        run(&mut watch, OTHER_QUEUE, SpaceId(4), &copied(&[&work()]), now);
        watch.space_gone(SPACE);
        assert_eq!(watch.ended(|_, _| false), [OTHER_QUEUE]);
    }

    #[test]
    fn a_session_whose_run_never_reached_the_engine_is_not_kept_and_is_no_work_to_rest_after() {
        let mut watch = Watch::new(true, Some(REST)).unwrap();
        let start = Instant::now();
        watch.reached(None, start);
        // amdgpu refused the run, say, or the commands couldn't be sent: nothing is noted of it.
        let later = start + Duration::from_millis(500);
        let (seen, rest) = watch.before(QUEUE, SPACE, &copied(&[&work()]), later);
        assert!(seen.is_some());
        assert_eq!(rest, Duration::from_millis(1000));
        assert_eq!(watch.take(QUEUE), None, "the firmware has no session to close");
        assert!(watch.ended(|_, _| false).is_empty());
        // The rest is still counted from the work that did reach the engine.
        let retry = later + Duration::from_millis(300);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &copied(&[&work()]), retry), Duration::from_millis(700));
        assert!(watch.take(QUEUE).is_some());
    }

    #[test]
    fn a_session_with_no_known_context_buffer_is_not_ended_by_anothers_end_only_by_its_queues() {
        let mut watch = Watch::new(true, None).unwrap();
        let now = Instant::now();
        // Its commands never named the context buffer, so nothing says whether the guest freed it.
        run(&mut watch, QUEUE, SPACE, &copied(&[&later_work()]), now);
        run(&mut watch, OTHER_QUEUE, SPACE, &copied(&[&work()]), now);
        assert_eq!(watch.ended(|_, _| false), [OTHER_QUEUE], "whatever is still mapped");
        assert!(watch.ended(|_, _| true).is_empty());
        let (_, session) = watch.take(QUEUE).unwrap();
        assert_eq!(session.context, None);
    }

    #[test]
    fn a_guest_that_restarts_leaves_no_session_behind_to_close_or_to_stop_a_rest() {
        let mut watch = Watch::new(true, Some(REST)).unwrap();
        let now = Instant::now();
        run(&mut watch, QUEUE, SPACE, &copied(&[&work()]), now);
        watch.all_gone();
        assert!(watch.ended(|_, _| false).is_empty());
        assert_eq!(watch.take(QUEUE), None);
        // A new guest's first session rests: no session from before counts as another one open.
        let after = now + Duration::from_millis(100);
        let rest = run(&mut watch, OTHER_QUEUE, SpaceId(9), &copied(&[&work()]), after);
        assert_eq!(rest, Duration::from_millis(1400));
    }

    #[test]
    fn work_that_names_no_encode_session_changes_nothing() {
        let mut watch = Watch::new(true, Some(REST)).unwrap();
        let now = Instant::now();
        let mut decode = work();
        decode[2] = 3;
        watch.reached(None, now);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &copied(&[&decode]), now), Duration::ZERO);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &copied(&[&[0, 0][..]]), now), Duration::ZERO);
        assert_eq!(watch.take(QUEUE), None);
    }

    #[test]
    fn a_new_session_rests_what_is_left_since_the_engines_last_work() {
        let mut watch = Watch::new(false, Some(REST)).unwrap();
        let start = Instant::now();
        let first = copied(&[&work()]);
        assert_eq!(
            run(&mut watch, QUEUE, SPACE, &first, start),
            Duration::ZERO,
            "the engine has had no work"
        );
        watch.reached(None, start);
        // The same session's later work isn't held up.
        let later = start + Duration::from_millis(16);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &copied(&[&later_work()]), later), Duration::ZERO);
        watch.reached(None, later);
        // The next session on the queue comes 440 ms after that, and waits out the rest.
        let next = later + Duration::from_millis(440);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &first, next), Duration::from_millis(1060));
        watch.reached(None, next + Duration::from_millis(1060));
        // One that comes after the rest is over isn't held up.
        let late = next + Duration::from_secs(5);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &first, late), Duration::ZERO);
    }

    #[test]
    fn there_is_no_rest_while_another_queues_session_is_open_or_with_no_rest_asked_for() {
        let mut watch = Watch::new(true, Some(REST)).unwrap();
        let now = Instant::now();
        let first = copied(&[&work()]);
        run(&mut watch, OTHER_QUEUE, SPACE, &first, now);
        watch.reached(None, now);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &first, now), Duration::ZERO);
        // With the other one taken off, a new session on the queue rests.
        watch.take(OTHER_QUEUE);
        assert_eq!(run(&mut watch, QUEUE, SPACE, &first, now), REST);

        let mut closes_only = Watch::new(true, None).unwrap();
        closes_only.reached(None, now);
        assert_eq!(run(&mut closes_only, QUEUE, SPACE, &first, now), Duration::ZERO);
    }

    #[test]
    fn the_last_buffer_of_a_submission_that_names_a_session_is_the_one_kept() {
        let mut watch = Watch::new(true, None).unwrap();
        let mut other = work();
        other[13] = 21;
        run(&mut watch, QUEUE, SPACE, &copied(&[&work(), &other, &[0, 0][..]]), Instant::now());
        assert_eq!(watch.take(QUEUE).map(|(_, session)| session.task), Some(21));
    }
}
