//! Hardware video for a Windows guest: an experiment, off unless the daemon is started with
//! `GUESTHOUSE_AMD_VIDEO=1`. Without it nothing here is used, and the guest's driver decodes and
//! encodes on the CPU as before.
//!
//! With it the guest is told three things more, and its driver then offers its hardware decoders
//! and its encoder:
//!
//! - a node after the card's others with AMD's number for its first video codec engine, which has
//!   amdgpu's video ring behind it: see [`crate::nodes`];
//! - two dwords in the adapter block: see [`crate::protocol::adapter_info`];
//! - answers to the three escapes of a video session, from the books kept here, as AMD's kernel
//!   driver answers them from its own: see [`crate::protocol::video_escape`].
//!
//! The guest's driver writes its video commands in the form the card's firmware takes, and the
//! daemon runs them as they are, from a copy: see [`crate::video_ib`]. Nothing here reads them.
//!
//! It is a switch because of what a mistake costs. Video work the firmware doesn't take can hang
//! the video ring, amdgpu then resets it, and that reaches every program on the host that is using
//! the card's video engine, not only this guest. So it is for a person to turn on, on a host where
//! that is acceptable. Each run is also waited for on the daemon's message thread, a few
//! milliseconds a frame, which holds up the guest's other GPU work for that long.
//!
//! The books are a list of the sessions that are open, each with what it has booked on an engine.
//! The guest's driver is told the sum booked on an engine and the most the chip takes, and decides
//! by itself what to make of them: nothing here refuses a session for want of room. The sum is of
//! this guest's sessions alone. What the host's own programs or another machine are decoding isn't
//! in it. A process that ends without ending its sessions has them ended here, so what it booked
//! doesn't stay in the sum.
//!
//! The most a chip takes is fixed by its family, and it is known for two. On any other card the
//! switch is refused with a warning, as a made-up number would have the guest's driver plan by it.
//! It is refused too on a card with more than one video instance, where the copy isn't all that
//! amdgpu reads: see [`most_throughput`].

use crate::info::DeviceInfo;
use crate::protocol::escape::EscapeCode;
use crate::protocol::video_escape::{self, Asked, Change};
use crate::traffic;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;

/// The daemon's switch.
pub const VARIABLE: &str = "GUESTHOUSE_AMD_VIDEO";
/// The one value that turns a video switch on.
const ON: &str = "1";
/// The values that leave it off without a word, as leaving the variable out does.
const OFF: [&str; 2] = ["0", ""];

/// Whether the switch `name` is on, by its value in the daemon's environment. Only [`ON`] turns
/// one on: a switch that can cost the host a reset of its video engine mustn't come on because a
/// variable is there with a value that reads as no. A value that is neither is said in a warning.
pub fn switch_is_on(name: &str, value: Option<&OsStr>) -> bool {
    let Some(value) = value else { return false };
    if value != ON && !OFF.iter().any(|off| value == *off) {
        log::warn!("{name} is {}, which isn't {ON}, so it is left off", value.display());
    }
    value == ON
}

/// amdgpu's family numbers, AMDGPU_FAMILY_GC_11_0_1 and AMDGPU_FAMILY_GC_11_5_0, for the chips
/// whose most throughput is known: Phoenix and Strix, as AMD names them.
const FAMILY_PHOENIX: u32 = 148;
const FAMILY_STRIX: u32 = 150;
/// The most video work those chips take at once, in macroblocks a second: 1920x1088 at 360 frames
/// a second. AMD's driver gives the same figure for both.
const MOST_THROUGHPUT: u32 = 2_937_600;

/// The first session's ID. Zero is never one.
const FIRST_SESSION: u32 = 1;
/// How many sessions a guest may have open. Far more than its programs open, and few enough that a
/// program that starts sessions without end can't have the daemon keep books for them all.
pub const MOST_SESSIONS: usize = 4096;

/// Why a card's video engine isn't offered, in words that follow "but".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotOffered {
    /// amdgpu has no ring for it, as on a card whose video engine is off or has none.
    NoRing,
    /// amdgpu says the card has this many video instances, not one, or 0 if it wouldn't say.
    Instances(u32),
    /// The most this family of chips takes isn't known.
    UnknownFamily(u32),
}

impl fmt::Display for NotOffered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRing => f.write_str("amdgpu has no video ring ready on this card"),
            Self::Instances(0) => f.write_str(
                "amdgpu didn't say how many video instances this card has, and video is offered only on a card \
                 with one",
            ),
            Self::Instances(count) => write!(
                f,
                "amdgpu says this card has {count} video instances, and video is offered only on a card with one"
            ),
            Self::UnknownFamily(family) => write!(
                f,
                "how much video work this card's chip takes isn't known (amdgpu's family {family}; it is known \
                 for {FAMILY_PHOENIX} and {FAMILY_STRIX}, Phoenix and Strix)"
            ),
        }
    }
}

/// The most video work the card's chip takes at once, in macroblocks a second, which the guest's
/// driver has to be told. A card with no answer is one whose video engine isn't offered.
///
/// A card with more than one video instance gets none, whatever its family. On a chip with a
/// second instance, amdgpu reads more than the command buffer for work it puts on that instance: it
/// follows the decode message buffer the commands name by GPU address and maps that with the CPU
/// too, and that buffer is the guest's, which the copy in [`crate::video_ib`] doesn't cover. With
/// one instance that never comes into it. That is from reading amdgpu's VCN 4.0 code. It is asked
/// of the card rather than taken from its family, as each of amdgpu's family numbers covers several
/// chips. amdgpu counts an instance it has turned off as well, so a chip with one of two turned off
/// gets none either.
pub fn most_throughput(info: &DeviceInfo) -> Result<u32, NotOffered> {
    if !info.video_ring {
        return Err(NotOffered::NoRing);
    }
    if info.video_instances != 1 {
        return Err(NotOffered::Instances(info.video_instances));
    }
    match info.family {
        FAMILY_PHOENIX | FAMILY_STRIX => Ok(MOST_THROUGHPUT),
        family => Err(NotOffered::UnknownFamily(family)),
    }
}

/// One open session: whose it is, and the engine and demand it has booked, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Session {
    process: u32,
    booked: Option<(u32, u32)>,
}

/// How one of the three escapes came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A session was started under this ID.
    Started { session: u32 },
    /// As many sessions as a guest may have are open, so none was started.
    Full,
    /// The session was ended, and what it had booked taken off.
    Ended,
    /// The escape named a session that isn't open, so nothing was changed. What AMD's driver
    /// answers then isn't known. Here it fails, as nothing was done for it.
    NoSuchSession,
    /// The engine's sum after any change, and the most the chip takes.
    Throughput { booked: u32, most: u32 },
}

impl Outcome {
    /// Whether the escape is answered as done.
    pub fn done(&self) -> bool {
        matches!(self, Self::Started { .. } | Self::Ended | Self::Throughput { .. })
    }
}

/// The books of a guest's video sessions. See the module documentation.
#[derive(Debug)]
pub struct Sessions {
    most: u32,
    /// The ID the next session is given, if no open session has it.
    next: u32,
    open: HashMap<u32, Session>,
    /// Whether it has been said that no more sessions may be open. A guest can ask again without
    /// end, so it is said once.
    said_full: bool,
}

impl Sessions {
    /// Empty books for a chip that takes `most` macroblocks a second.
    pub fn new(most: u32) -> Self {
        Self { most, next: FIRST_SESSION, open: HashMap::new(), said_full: false }
    }

    /// Answers one of the three escapes from `process` in its payload, and says whether it did.
    /// False, with nothing written, for any other escape, for one of the three that isn't shaped as
    /// the guest's driver sends it, and for one that can't be done.
    pub fn answer(&mut self, process: u32, code: EscapeCode, payload: &mut [u8]) -> bool {
        let Some(asked) = video_escape::read(code, payload) else {
            return false;
        };
        let outcome = self.settle(process, asked);
        match outcome {
            Outcome::Started { session } => video_escape::give_session(payload, session),
            Outcome::Throughput { booked, most } => video_escape::give_throughput(payload, booked, most),
            Outcome::Full if !self.said_full => {
                self.said_full = true;
                log::warn!(
                    "a guest program asked for a video session with {MOST_SESSIONS} open, which is as many \
                     as a guest may have, so it gets none. This is said once"
                );
            }
            Outcome::Full | Outcome::Ended | Outcome::NoSuchSession => {}
        }
        traffic::session(process, asked, outcome, self.open.len());
        outcome.done()
    }

    /// Changes the books by what was asked, and says what the answer is.
    fn settle(&mut self, process: u32, asked: Asked) -> Outcome {
        match asked {
            Asked::Start if self.open.len() >= MOST_SESSIONS => Outcome::Full,
            Asked::Start => {
                let session = self.unused_id();
                self.open.insert(session, Session { process, booked: None });
                Outcome::Started { session }
            }
            Asked::End { session } => match self.open.remove(&session) {
                Some(_) => Outcome::Ended,
                None => Outcome::NoSuchSession,
            },
            Asked::Throughput { session, engine, change } => {
                let booked = match change {
                    Change::Book(demand) => Some((engine, demand)),
                    Change::Release => None,
                    // Asked before a session is started too, so it names none.
                    Change::Ask => return self.throughput_of(engine),
                };
                let Some(open) = self.open.get_mut(&session) else {
                    return Outcome::NoSuchSession;
                };
                // A session books for itself once. Booking again takes the place of what it had.
                open.booked = booked;
                self.throughput_of(engine)
            }
        }
    }

    fn throughput_of(&self, engine: u32) -> Outcome {
        Outcome::Throughput { booked: self.booked_on(engine), most: self.most }
    }

    /// What the open sessions have booked on an engine, all processes together. It stops at the
    /// most a dword holds, which only a guest sending made-up demands reaches.
    fn booked_on(&self, engine: u32) -> u32 {
        let on_engine = self.open.values().filter_map(|session| session.booked);
        let sum: u64 = on_engine.filter(|&(on, _)| on == engine).map(|(_, demand)| u64::from(demand)).sum();
        u32::try_from(sum).unwrap_or(u32::MAX)
    }

    /// The next ID in the count that no open session has, and never zero. The count starts over
    /// after the last a dword holds.
    fn unused_id(&mut self) -> u32 {
        loop {
            let id = self.next;
            self.next = id.checked_add(1).unwrap_or(FIRST_SESSION);
            if !self.open.contains_key(&id) {
                return id;
            }
        }
    }

    /// A process has ended, however it ended, so its sessions have, and what they booked is taken
    /// off. Its driver sends nothing for them when the process is killed.
    pub fn process_gone(&mut self, process: u32) {
        let before = self.open.len();
        self.open.retain(|_, session| session.process != process);
        let ended = before - self.open.len();
        if ended > 0 {
            log::debug!("ended {ended} video sessions process {process:#x} left open");
        }
    }

    /// The guest has restarted, so none of its sessions are open. The count of IDs goes on.
    pub fn all_gone(&mut self) {
        self.open.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::video_escape::tests::{
        CODEC_ENGINE, answered, end, session_given, start, throughput,
    };

    const PROCESS: u32 = 0x4000_0002;
    const OTHER_PROCESS: u32 = 0x4000_0006;
    const MOST: u32 = MOST_THROUGHPUT;

    #[test]
    fn only_one_turns_a_switch_on_and_any_other_word_is_said() {
        let is_on = |value: Option<&str>| switch_is_on(VARIABLE, value.map(OsStr::new));
        assert!(is_on(Some("1")));
        let lines = crate::watch::lines::logged(|| {
            assert!(!is_on(None) && !is_on(Some("0")) && !is_on(Some("")));
        });
        assert!(lines.is_empty(), "{lines:?}");
        let lines = crate::watch::lines::logged(|| {
            assert!(!is_on(Some("yes")) && !is_on(Some("01")) && !is_on(Some(" 1")));
        });
        let warnings = crate::watch::lines::at(log::Level::Warn, &lines);
        assert_eq!(warnings.len(), 3, "{lines:?}");
        assert_eq!(warnings[0], "GUESTHOUSE_AMD_VIDEO is yes, which isn't 1, so it is left off");
    }

    fn card(family: u32, video_ring: bool) -> DeviceInfo {
        DeviceInfo { family, video_ring, video_instances: 1, ..DeviceInfo::default() }
    }

    #[test]
    fn the_most_a_chip_takes_is_given_for_phoenix_and_strix_only() {
        assert_eq!(most_throughput(&card(148, true)), Ok(2_937_600));
        assert_eq!(most_throughput(&card(150, true)), Ok(2_937_600));
        // 1080p, which is 1088 lines of macroblocks, 360 times a second.
        assert_eq!(MOST_THROUGHPUT, (1920 / 16) * (1088 / 16) * 360);
        for family in [0, 143, 145, 146, 149, 151, 152] {
            assert_eq!(most_throughput(&card(family, true)), Err(NotOffered::UnknownFamily(family)));
        }
    }

    #[test]
    fn a_card_with_no_video_ring_is_not_offered_whatever_its_family() {
        assert_eq!(most_throughput(&card(148, false)), Err(NotOffered::NoRing));
        assert_eq!(most_throughput(&card(149, false)), Err(NotOffered::NoRing));
    }

    #[test]
    fn a_card_with_other_than_one_video_instance_is_not_offered_whatever_its_family() {
        for family in [148, 150, 149] {
            for count in [0, 2, 4] {
                let info = DeviceInfo { video_instances: count, ..card(family, true) };
                assert_eq!(most_throughput(&info), Err(NotOffered::Instances(count)));
            }
        }
        // No ring is said first, as it is the plainer reason.
        assert_eq!(
            most_throughput(&DeviceInfo { video_instances: 2, ..card(148, false) }),
            Err(NotOffered::NoRing)
        );
    }

    #[test]
    fn why_a_card_is_not_offered_reads_on_from_but() {
        assert_eq!(NotOffered::NoRing.to_string(), "amdgpu has no video ring ready on this card");
        assert_eq!(
            NotOffered::Instances(2).to_string(),
            "amdgpu says this card has 2 video instances, and video is offered only on a card with one"
        );
        assert_eq!(
            NotOffered::Instances(0).to_string(),
            "amdgpu didn't say how many video instances this card has, and video is offered only on a card with one"
        );
        assert_eq!(
            NotOffered::UnknownFamily(149).to_string(),
            "how much video work this card's chip takes isn't known (amdgpu's family 149; it is known for 148 \
             and 150, Phoenix and Strix)"
        );
    }

    /// Starts a session through its escape, and returns the ID the guest reads back.
    fn started(sessions: &mut Sessions, process: u32) -> u32 {
        let (code, mut payload) = start();
        assert!(sessions.answer(process, code, &mut payload));
        session_given(&payload)
    }

    /// Sends a throughput escape, and returns the sum and the most the guest reads back, or `None`
    /// for one answered as failed, with its answer's block as it came.
    fn asked(sessions: &mut Sessions, session: u32, change: Change) -> Option<(u32, u32)> {
        let (code, mut payload) = throughput(session, change);
        let before = payload.clone();
        if sessions.answer(PROCESS, code, &mut payload) {
            return Some(answered(&payload));
        }
        assert_eq!(payload, before, "a failed escape was written to");
        None
    }

    fn ended(sessions: &mut Sessions, session: u32) -> bool {
        let (code, mut payload) = end(session);
        let before = payload.clone();
        let done = sessions.answer(PROCESS, code, &mut payload);
        assert_eq!(payload, before, "the end of a session writes nothing back");
        done
    }

    #[test]
    fn sessions_are_numbered_from_one_and_an_ended_one_is_forgotten() {
        let mut sessions = Sessions::new(MOST);
        assert_eq!(started(&mut sessions, PROCESS), 1);
        assert_eq!(started(&mut sessions, PROCESS), 2);
        assert!(ended(&mut sessions, 1));
        assert!(!ended(&mut sessions, 1), "it was forgotten");
        // The count goes on: an ID isn't given again while the count hasn't come round.
        assert_eq!(started(&mut sessions, OTHER_PROCESS), 3);
    }

    #[test]
    fn the_count_comes_round_past_zero_and_past_the_sessions_still_open() {
        let mut sessions = Sessions::new(MOST);
        assert_eq!(started(&mut sessions, PROCESS), 1);
        sessions.next = u32::MAX;
        assert_eq!(started(&mut sessions, PROCESS), u32::MAX);
        assert_eq!(started(&mut sessions, PROCESS), 2, "not zero, and not the one still open");
    }

    #[test]
    fn asking_changes_nothing_and_needs_no_session() {
        let mut sessions = Sessions::new(MOST);
        assert_eq!(asked(&mut sessions, 0, Change::Ask), Some((0, MOST)));
        let session = started(&mut sessions, PROCESS);
        assert_eq!(asked(&mut sessions, session, Change::Book(209_635)), Some((209_635, MOST)));
        assert_eq!(asked(&mut sessions, 0, Change::Ask), Some((209_635, MOST)));
        assert_eq!(asked(&mut sessions, 77, Change::Ask), Some((209_635, MOST)));
    }

    #[test]
    fn the_sum_is_of_every_open_session_and_a_release_takes_one_sessions_demand_off() {
        let mut sessions = Sessions::new(MOST);
        let first = started(&mut sessions, PROCESS);
        let second = started(&mut sessions, OTHER_PROCESS);
        assert_eq!(asked(&mut sessions, first, Change::Book(209_635)), Some((209_635, MOST)));
        assert_eq!(asked(&mut sessions, second, Change::Book(832_377)), Some((1_042_012, MOST)));
        assert_eq!(asked(&mut sessions, first, Change::Release), Some((832_377, MOST)));
        assert_eq!(
            asked(&mut sessions, first, Change::Release),
            Some((832_377, MOST)),
            "nothing left to take"
        );
        assert_eq!(asked(&mut sessions, second, Change::Release), Some((0, MOST)));
    }

    #[test]
    fn a_session_that_books_again_has_its_new_demand_in_the_sum_once() {
        let mut sessions = Sessions::new(MOST);
        let session = started(&mut sessions, PROCESS);
        assert_eq!(asked(&mut sessions, session, Change::Book(100)), Some((100, MOST)));
        assert_eq!(asked(&mut sessions, session, Change::Book(250)), Some((250, MOST)));
    }

    #[test]
    fn nothing_is_refused_for_want_of_room_and_the_sum_stops_at_a_dwords_most() {
        let mut sessions = Sessions::new(MOST);
        let first = started(&mut sessions, PROCESS);
        let second = started(&mut sessions, PROCESS);
        assert_eq!(asked(&mut sessions, first, Change::Book(MOST)), Some((MOST, MOST)));
        assert_eq!(asked(&mut sessions, second, Change::Book(1)), Some((MOST + 1, MOST)));
        assert_eq!(asked(&mut sessions, first, Change::Book(u32::MAX)), Some((u32::MAX, MOST)));
    }

    #[test]
    fn each_engine_has_a_sum_of_its_own() {
        const ENGINE_AT: usize = 0x0C;
        let mut sessions = Sessions::new(MOST);
        let session = started(&mut sessions, PROCESS);
        assert_eq!(asked(&mut sessions, session, Change::Book(500)), Some((500, MOST)));
        let (code, mut payload) = throughput(0, Change::Ask);
        payload[ENGINE_AT..ENGINE_AT + 4].copy_from_slice(&(CODEC_ENGINE + 7).to_le_bytes());
        assert!(sessions.answer(PROCESS, code, &mut payload));
        assert_eq!(answered(&payload), (0, MOST));
    }

    #[test]
    fn booking_or_releasing_for_a_session_that_is_not_open_fails_and_changes_nothing() {
        let mut sessions = Sessions::new(MOST);
        let session = started(&mut sessions, PROCESS);
        assert_eq!(asked(&mut sessions, session, Change::Book(100)), Some((100, MOST)));
        // Zero is what a guest sends that was never given an ID.
        for unknown in [0, session + 1] {
            assert_eq!(asked(&mut sessions, unknown, Change::Book(50)), None);
            assert_eq!(asked(&mut sessions, unknown, Change::Release), None);
        }
        assert_eq!(asked(&mut sessions, 0, Change::Ask), Some((100, MOST)));
    }

    #[test]
    fn ending_a_session_takes_off_what_it_still_had_booked() {
        let mut sessions = Sessions::new(MOST);
        let session = started(&mut sessions, PROCESS);
        asked(&mut sessions, session, Change::Book(100)).unwrap();
        assert!(ended(&mut sessions, session));
        assert_eq!(asked(&mut sessions, 0, Change::Ask), Some((0, MOST)));
        assert_eq!(asked(&mut sessions, session, Change::Book(100)), None, "it is over");
    }

    #[test]
    fn a_process_that_ends_without_a_word_leaves_nothing_in_the_sum() {
        let mut sessions = Sessions::new(MOST);
        let killed = [started(&mut sessions, PROCESS), started(&mut sessions, PROCESS)];
        let kept = started(&mut sessions, OTHER_PROCESS);
        asked(&mut sessions, killed[0], Change::Book(100)).unwrap();
        asked(&mut sessions, killed[1], Change::Book(200)).unwrap();
        assert_eq!(asked(&mut sessions, kept, Change::Book(400)), Some((700, MOST)));

        sessions.process_gone(PROCESS);
        assert_eq!(asked(&mut sessions, 0, Change::Ask), Some((400, MOST)));
        assert!(!ended(&mut sessions, killed[0]) && !ended(&mut sessions, killed[1]));
        assert!(ended(&mut sessions, kept), "another process's session is left open");
        sessions.process_gone(PROCESS);
    }

    #[test]
    fn a_guest_that_restarts_has_no_sessions_and_its_new_ones_get_new_ids() {
        let mut sessions = Sessions::new(MOST);
        let session = started(&mut sessions, PROCESS);
        asked(&mut sessions, session, Change::Book(100)).unwrap();
        sessions.all_gone();
        assert_eq!(asked(&mut sessions, 0, Change::Ask), Some((0, MOST)));
        assert!(!ended(&mut sessions, session));
        assert_eq!(started(&mut sessions, PROCESS), session + 1);
    }

    #[test]
    fn a_guest_gets_no_more_sessions_than_it_may_have_open() {
        let mut sessions = Sessions::new(MOST);
        let all: Vec<u32> = (0..MOST_SESSIONS).map(|_| started(&mut sessions, PROCESS)).collect();
        let (code, mut payload) = start();
        let before = payload.clone();
        let lines = crate::watch::lines::logged(|| {
            for _ in 0..3 {
                assert!(!sessions.answer(PROCESS, code, &mut payload));
            }
        });
        assert_eq!(payload, before);
        // A guest can ask as often as it likes, and the log hears of it once.
        assert_eq!(crate::watch::lines::at(log::Level::Warn, &lines).len(), 1, "{lines:?}");
        assert!(ended(&mut sessions, all[0]));
        assert_eq!(started(&mut sessions, PROCESS), MOST_SESSIONS as u32 + 1);
    }

    #[test]
    fn another_escape_and_a_misshaped_one_are_left_for_someone_else() {
        let mut sessions = Sessions::new(MOST);
        let (code, mut payload) = start();
        let other = EscapeCode { subfunction: 0x86, ..code };
        assert!(!sessions.answer(PROCESS, other, &mut payload));
        payload.pop();
        assert!(!sessions.answer(PROCESS, code, &mut payload));
        assert_eq!(started(&mut sessions, PROCESS), 1, "neither started a session");
    }
}
