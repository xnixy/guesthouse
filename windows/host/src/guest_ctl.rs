//! remoteman's `ctl` in a running Windows guest, asked over its serial port, as this host has no agent
//! in the guest: [`GuestCtl`] is the handle another thread asks through, and [`read_lines`] is how
//! `boot` reads the port, handing the channel's lines to the handle and every other line on.
//!
//! The channel's lines carry the names of a person's devices and the codes they pair with, so none
//! of them reaches the sink, the machine's events or a report: not an answer, not a request echoed
//! back, not garbage that only begins as one. The format is
//! [`guesthouse_remote_view_core::ctl_wire`]'s.
//!
//! One request is in flight at a time: a second waits for the first, within its own time. The guest
//! runs them one at a time too, on the thread that also listens for the host asking it to shut down.
//!
//! The handle is the shared pairing driver's way into the guest too ([`session::Guest`]), which
//! tells an answer that may be lost from one the guest wrote.

use crate::events::{Event, Sink};
use crate::printable::{LINE_LIMIT, printable};
use guesthouse_remote_view_core::ctl::Words;
use guesthouse_remote_view_core::ctl_wire::{self, Answer, Failure, Id, Progress, Reassembly};
use guesthouse_remote_view_core::session::{self, Unanswered};
use std::io::{self, BufRead, BufReader, Read};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// How long a caller should give the guest to answer: longer than the guest gives `ctl` itself,
/// with room for a few KB at the port's speed.
pub const ANSWER_WAIT: Duration = Duration::from_secs(20);
/// How long the shared driver gives a `status` reading. A reading changes nothing, so one that
/// comes too late is only read again, and a request written before the guest's report listens,
/// which nothing ever answers, holds the channel for this long rather than [`ANSWER_WAIT`].
pub const STATUS_WAIT: Duration = Duration::from_secs(4);

/// The port's writing end, one whole line at a time.
pub trait LineWriter: Send + Sync {
    fn write_line(&self, line: &[u8]) -> io::Result<()>;
}

/// Why a request had no answer. After [`CtlError::Busy`], [`CtlError::Silent`],
/// [`CtlError::Incomplete`] and [`CtlError::PortClosed`] the guest may still have run the command,
/// or may yet, as a line held up in the port can reach it after its caller gave up: read `status`
/// again rather than send the command again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlError {
    /// The words aren't a command the guest takes. Nothing was sent.
    NotAllowed(String),
    /// The guest isn't Windows, and nothing in it answers.
    NotWindows,
    /// The guest hasn't opened its serial port, as it hasn't started yet or is restarting.
    NoPort,
    /// The port closed while the guest was asked, as it does when the guest restarts.
    PortClosed,
    /// The machine has stopped, or stopped while it was asked.
    Stopped,
    /// The port took nothing in the time, or another request held it for all of it.
    Busy,
    /// The guest said nothing back in the time: a guest too old to know the channel, or one whose
    /// report isn't running.
    Silent,
    /// Part of the answer came, and not the rest, in the time.
    Incomplete,
    /// The guest ran nothing, and said why.
    Guest(Failure),
    /// The answer's parts don't make one.
    Garbled(String),
    /// Writing to the port failed.
    Write(String),
}

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtlError::NotAllowed(why) => f.write_str(why),
            CtlError::NotWindows => f.write_str("Only a Windows guest answers remoteman's commands."),
            CtlError::NoPort => f.write_str("The guest's serial port isn't open yet."),
            CtlError::PortClosed => f.write_str("The guest's serial port closed before it answered."),
            CtlError::Stopped => f.write_str("The machine has stopped."),
            CtlError::Busy => f.write_str("The guest's serial port is busy."),
            CtlError::Silent => f.write_str(
                "The guest doesn't answer: Windows hasn't started its report yet, or the guest started \
                     with an older report and answers from its next start.",
            ),
            CtlError::Incomplete => f.write_str("Only part of the guest's answer came."),
            CtlError::Guest(failure) => f.write_str(failure.sentence()),
            CtlError::Garbled(why) => write!(f, "The guest's answer didn't read: {why}"),
            CtlError::Write(why) => write!(f, "Writing to the guest's serial port failed: {why}"),
        }
    }
}

/// The handle to a machine's channel. Clones share it. It lives as long as a boot does: once the
/// machine stops it refuses every request.
#[derive(Clone, Default)]
pub struct GuestCtl(Arc<Shared>);

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    writer: Option<Arc<dyn LineWriter>>,
    /// Whether what is on the port's other end answers: a Windows guest's report.
    answers: bool,
    ended: bool,
    /// A line is being written, and others wait for it.
    writing: bool,
    /// A request is in flight, and others wait for it.
    asking: bool,
    last_id: Id,
    pending: Option<Pending>,
}

struct Pending {
    whole: Reassembly,
    heard: bool,
    result: Option<Result<Answer, CtlError>>,
}

/// Shows only whether the guest's port is open, as what is in flight is the guest's.
impl std::fmt::Debug for GuestCtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("GuestCtl")
            .field("open", &state.writer.is_some())
            .field("ended", &state.ended)
            .finish()
    }
}

impl GuestCtl {
    pub fn new() -> GuestCtl {
        GuestCtl::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits on the state until `deadline`: None once it has passed.
    fn wait_until<'a>(
        &self,
        state: MutexGuard<'a, State>,
        deadline: Instant,
    ) -> Option<MutexGuard<'a, State>> {
        let left = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())?;
        let (state, _) =
            self.0.changed.wait_timeout(state, left).unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(state)
    }

    /// Runs `ctl` with `words` in the guest and gives what it said, within `timeout`.
    pub fn ask(&self, words: &[String], timeout: Duration) -> Result<Answer, CtlError> {
        ctl_wire::allowed(words).map_err(CtlError::NotAllowed)?;
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        let id = loop {
            if state.ended {
                return Err(CtlError::Stopped);
            }
            if state.writer.is_some() && !state.answers {
                return Err(CtlError::NotWindows);
            }
            if state.writer.is_none() {
                return Err(CtlError::NoPort);
            }
            if !state.asking {
                state.asking = true;
                state.last_id = state.last_id.checked_add(1).unwrap_or(1);
                let id = state.last_id;
                state.pending = Some(Pending { whole: Reassembly::new(id), heard: false, result: None });
                break id;
            }
            state = self.wait_until(state, deadline).ok_or(CtlError::Busy)?;
        };
        drop(state);
        let _asking = Asking(self);
        let line = ctl_wire::request_line(id, words).map_err(CtlError::NotAllowed)?;
        self.write(&line, deadline)?;
        let mut state = self.lock();
        loop {
            if let Some(result) = state.pending.as_mut().and_then(|pending| pending.result.take()) {
                return result;
            }
            if state.ended {
                return Err(CtlError::Stopped);
            }
            match self.wait_until(state, deadline) {
                Some(again) => state = again,
                None => {
                    let heard = self.lock().pending.as_ref().is_some_and(|pending| pending.heard);
                    return Err(if heard { CtlError::Incomplete } else { CtlError::Silent });
                }
            }
        }
    }

    /// Writes one line to the guest, within `wait`: for the line asking it to shut down, which any
    /// guest listens for. Gives whether it went.
    pub fn send_line(&self, line: &str, wait: Duration) -> bool {
        self.write(line, Instant::now() + wait).is_ok()
    }

    /// Writes `line` and its ending on a thread of its own, so a port that takes nothing holds up
    /// nobody past `deadline`. Until that write ends, every other waits, and fails at its own.
    fn write(&self, line: &str, deadline: Instant) -> Result<(), CtlError> {
        let mut state = self.lock();
        let writer = loop {
            if state.ended {
                return Err(CtlError::Stopped);
            }
            if !state.writing {
                break state.writer.clone().ok_or(CtlError::NoPort)?;
            }
            state = self.wait_until(state, deadline).ok_or(CtlError::Busy)?;
        };
        state.writing = true;
        drop(state);
        let bytes = format!("{line}\n").into_bytes();
        let (done, written) = mpsc::channel();
        let shared = Arc::clone(&self.0);
        thread::spawn(move || {
            let result = writer.write_line(&bytes);
            shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).writing = false;
            shared.changed.notify_all();
            let _ = done.send(result);
        });
        let left = deadline.saturating_duration_since(Instant::now());
        match written.recv_timeout(left) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(CtlError::Write(error.to_string())),
            Err(_) => Err(CtlError::Busy),
        }
    }

    /// The guest has opened its port, and this is its writing end. `answers` is whether the guest
    /// is one whose report answers: a request is never written to any other, where it could be
    /// read as typing.
    pub fn attach(&self, writer: Arc<dyn LineWriter>, answers: bool) {
        let mut state = self.lock();
        state.writer = Some(writer);
        state.answers = answers;
    }

    /// The guest's port has closed. A request waiting fails now, as the guest that had it is gone.
    pub fn detach(&self) {
        let mut state = self.lock();
        state.writer = None;
        fail(&mut state, CtlError::PortClosed);
        drop(state);
        self.0.changed.notify_all();
    }

    /// The machine has stopped: a request waiting fails now, and every one after.
    pub fn end(&self) {
        let mut state = self.lock();
        state.ended = true;
        state.writer = None;
        fail(&mut state, CtlError::Stopped);
        drop(state);
        self.0.changed.notify_all();
    }

    /// Takes a line the guest said, made printable: true when it is the channel's, which is then
    /// the handle's alone and goes nowhere else.
    pub fn take_line(&self, line: &str) -> bool {
        let Some(read) = ctl_wire::parse_line(line) else { return false };
        let mut state = self.lock();
        if let Some(pending) = state.pending.as_mut().filter(|pending| pending.result.is_none()) {
            let result = match pending.whole.feed(&read) {
                Progress::Ignored => None,
                Progress::Partial => {
                    pending.heard = true;
                    None
                }
                Progress::Done(answer) => Some(Ok(answer)),
                Progress::Failed(failure) => Some(Err(CtlError::Guest(failure))),
                Progress::Garbled(why) => Some(Err(CtlError::Garbled(why))),
            };
            if result.is_some() {
                pending.result = result;
                drop(state);
                self.0.changed.notify_all();
            }
        }
        true
    }
}

/// The shared driver asks through the channel. What the guest printed is the answer, whatever code
/// `ctl` ended with, as the driver reads remoteman's own words for a refusal. A request that was never
/// written, or that the guest refused to run, can't have run, so it's an answer too, in words the
/// driver reads as a refusal. Everything else may or may not have run in the guest, and is
/// [`Unanswered`], so the driver reads the status again before it sends anything more.
impl session::Guest for GuestCtl {
    fn ctl(&self, words: &Words) -> Result<String, Unanswered> {
        let wait =
            if *words == guesthouse_remote_view_core::ctl::status() { STATUS_WAIT } else { ANSWER_WAIT };
        match self.ask(words, wait) {
            Ok(answer) => Ok(answer.text),
            Err(refused @ (CtlError::NotAllowed(_) | CtlError::Guest(Failure::Refused))) => {
                Ok(format!("error: {refused}"))
            }
            Err(error) => Err(Unanswered(error.to_string())),
        }
    }

    /// The driver's lines name devices by their fingerprints and repeat the guest's words, which
    /// stay out of every log on this host, as the channel's lines do.
    fn log(&self, _line: &str) {}
}

fn fail(state: &mut State, error: CtlError) {
    if let Some(pending) = state.pending.as_mut().filter(|pending| pending.result.is_none()) {
        pending.result = Some(Err(error));
    }
}

/// Lets the next request go when this one ends, however it ends.
struct Asking<'a>(&'a GuestCtl);

impl Drop for Asking<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.asking = false;
        state.pending = None;
        drop(state);
        self.0.0.changed.notify_all();
    }
}

/// Reads the guest's lines until the stream ends. Each is made printable; the channel's go to
/// `ctl`, and every other is sent to `sink` as the guest's and handed to `on_line`.
#[allow(clippy::disallowed_methods)] // each read is taken to LINE_LIMIT, and a longer line goes on as the next
pub fn read_lines(source: impl Read, on_line: &mut impl FnMut(&str), sink: &dyn Sink, ctl: &GuestCtl) {
    let mut source = BufReader::new(source);
    let mut line = Vec::new();
    // `take` so a guest that never ends a line can't make this hold more than the limit; the rest of
    // a longer line comes as the next one.
    while source.by_ref().take(LINE_LIMIT as u64 + 1).read_until(b'\n', &mut line).is_ok_and(|read| read > 0)
    {
        let text = printable(&line);
        line.clear();
        if ctl.take_line(&text) {
            continue;
        }
        sink.event(Event::Guest(text.clone()));
        on_line(&text);
    }
}

#[cfg(test)]
mod tests;
