// SPDX-License-Identifier: GPL-2.0-or-later
//! The window's keys and pointer for the guest, through a QEMU monitor socket of their own.
//!
//! As Guesthouse's Linux hosts do (`app-core`'s `input.rs`, which is for Linux only), every key,
//! move, button and turn of the wheel is a QMP `input-send-event` to QEMU, whose virtio keyboard
//! and tablet hand them to the guest. QEMU serves one client on the socket. Nothing is sent before
//! QEMU has answered, and nothing here waits: a send that would wait is kept for the next one, so
//! a QEMU that hangs can't hold up the window, from whose thread all of it is sent.
//!
//! Keys go by QEMU's names for them, so that keys outside a PC keyboard's main block need no table
//! of scan codes. What the guest has down is kept here, so that a window that loses the keyboard
//! can let go of all of it, and nothing the guest doesn't have down is let go.

use crate::keys;
use guesthouse_basics::capped_line::read_capped_line;
use std::collections::BTreeSet;
use std::io::{self, BufReader, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// The tablet's axes go from 0 to this.
pub const RANGE: u32 = 32767;
/// QEMU's names for the buttons, by the window's numbers for them.
const BUTTONS: [&str; 5] = ["left", "right", "middle", "side", "extra"];
/// More turns of the wheel than this in one event are taken as this many.
const MOST_STEPS: i32 = 8;
/// What waits to be sent is dropped past this, as nobody is reading it.
const MOST_UNSENT: usize = 1 << 20;
const GREETING: &[u8] = b"{\"execute\":\"qmp_capabilities\"}\n";
/// The first of QEMU's errors are worth reading.
const ERRORS_SHOWN: u64 = 8;
/// How long the socket is looked for after QEMU has come, and how often.
const CONNECT_TRIES: u32 = 100;
const CONNECT_PAUSE: Duration = Duration::from_millis(100);

/// Where a point of the window's content falls on the tablet's axes. The point is in points from
/// the content's top left, of a content `width` by `height`; the picture of `picture` pixels is
/// scaled into it to fit, keeping its shape, with bars beside it, as the window shows it. A point
/// on a bar is taken to the picture's nearest edge.
pub fn absolute(x: f64, y: f64, width: f64, height: f64, picture: (u32, u32)) -> Option<(u32, u32)> {
    let (picture_width, picture_height) = (f64::from(picture.0), f64::from(picture.1));
    if picture.0 == 0 || picture.1 == 0 || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (width / picture_width).min(height / picture_height);
    let left = (width - picture_width * scale) / 2.0;
    let top = (height - picture_height * scale) / 2.0;
    // A pixel's place on an axis, as a guest's compositor reads it back: value * pixels / (RANGE + 1).
    let axis = |at: f64, start: f64, pixels: f64| {
        let pixel = ((at - start) / scale).clamp(0.0, pixels);
        ((pixel * f64::from(RANGE + 1) / pixels) as u32).min(RANGE)
    };
    Some((axis(x, left, picture_width), axis(y, top, picture_height)))
}

/// What the reading thread found out.
#[derive(Default)]
struct Heard {
    answered: AtomicBool,
    gone: AtomicBool,
    errors: AtomicU64,
}

/// The monitor socket.
pub struct Qmp {
    stream: UnixStream,
    unsent: Vec<u8>,
    heard: Arc<Heard>,
}

impl Qmp {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let on = 1i32;
        // SAFETY: an option of a socket this process owns, from a live int of the size given. A
        // write to a QEMU that has gone is then an error and no signal.
        unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const on).cast(),
                size_of::<i32>() as libc::socklen_t,
            )
        };
        let heard = Arc::new(Heard::default());
        let reader = stream.try_clone()?;
        let on_thread = Arc::clone(&heard);
        thread::spawn(move || drain(reader, &on_thread));
        let mut qmp = Self { stream, unsent: GREETING.to_vec(), heard };
        qmp.flush();
        Ok(qmp)
    }

    /// Whether QEMU has answered, and how many of its answers were errors.
    pub fn answered(&self) -> (bool, u64) {
        (self.heard.answered.load(Ordering::Relaxed), self.heard.errors.load(Ordering::Relaxed))
    }

    fn execute(&mut self, line: &str) {
        if self.heard.answered.load(Ordering::Relaxed) && self.unsent.len() < MOST_UNSENT {
            self.unsent.extend_from_slice(line.as_bytes());
            self.unsent.push(b'\n');
        }
        self.flush();
    }

    fn send_events(&mut self, events: &str) {
        self.execute(&format!(
            "{{\"execute\":\"input-send-event\",\"arguments\":{{\"events\":[{events}]}}}}"
        ));
    }

    /// Sends what is waiting, as far as it goes without waiting.
    fn flush(&mut self) {
        if self.unsent.is_empty() || self.heard.gone.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: a send from a live buffer of the length given, on a socket this process owns,
        // that does not wait; the reading thread only reads.
        let sent = unsafe {
            libc::send(
                self.stream.as_raw_fd(),
                self.unsent.as_ptr().cast(),
                self.unsent.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if sent >= 0 {
            self.unsent.drain(..sent as usize);
        } else if io::Error::last_os_error().kind() != ErrorKind::WouldBlock {
            self.unsent.clear();
        }
    }
}

/// The longest line of QEMU's monitor that is read.
const MOST_LINE: usize = 1 << 20;

/// Reads QEMU's answers until it goes. Only whether it answered matters, and its errors.
fn drain(stream: UnixStream, heard: &Heard) {
    let mut reader = BufReader::new(stream);
    while let Ok(Some(line)) = read_capped_line(&mut reader, MOST_LINE) {
        heard.answered.store(true, Ordering::Relaxed);
        if line.contains("\"error\"") {
            let seen = heard.errors.fetch_add(1, Ordering::Relaxed);
            if seen < ERRORS_SHOWN {
                eprintln!("pvg-host: QEMU's monitor says: {}", line.trim());
            }
        }
    }
    heard.gone.store(true, Ordering::Relaxed);
}

/// What was sent, counted, for whoever asks how input is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sent {
    pub keys: u64,
    pub moves: u64,
    pub buttons: u64,
    pub steps: u64,
}

/// The keys and buttons the guest has down, and the socket they go through once it is there.
#[derive(Default)]
pub struct Input {
    qmp: Option<Qmp>,
    iso: bool,
    keys: BTreeSet<u16>,
    buttons: [bool; BUTTONS.len()],
    at: Option<(u32, u32)>,
    sent: Sent,
}

impl Input {
    pub fn new(iso: bool) -> Self {
        Self { iso, ..Self::default() }
    }

    pub fn connected(&mut self, qmp: Qmp) {
        self.qmp = Some(qmp);
    }

    /// Whether the socket is there, whether QEMU answered on it, and how many errors it said.
    pub fn state(&self) -> (bool, bool, u64) {
        self.qmp.as_ref().map_or((false, false, 0), |qmp| {
            let (answered, errors) = qmp.answered();
            (true, answered, errors)
        })
    }

    pub fn sent(&self) -> Sent {
        self.sent
    }

    fn send_events(&mut self, events: &str) {
        if let Some(qmp) = &mut self.qmp {
            qmp.send_events(events);
        }
    }

    fn send_key(&mut self, key: keys::Key, down: bool) {
        self.sent.keys += 1;
        self.send_events(&format!(
            "{{\"type\":\"key\",\"data\":{{\"key\":{{\"type\":\"qcode\",\"data\":\"{}\"}},\"down\":{down}}}}}",
            key.qcode
        ));
    }

    /// A key of the Mac's went down or up. One the guest already has as it is changes nothing.
    pub fn key(&mut self, code: u16, down: bool) {
        let Some(key) = keys::key(code, self.iso) else {
            return;
        };
        // Caps Lock comes as a press and a release each time, and the guest turns it over.
        let changed = if down { self.keys.insert(key.linux) } else { self.keys.remove(&key.linux) };
        if changed {
            self.send_key(key, down);
        }
    }

    pub fn point(&mut self, x: u32, y: u32) {
        if self.at == Some((x, y)) {
            return;
        }
        self.at = Some((x, y));
        self.sent.moves += 1;
        self.send_events(&format!(
            "{{\"type\":\"abs\",\"data\":{{\"axis\":\"x\",\"value\":{x}}}}},{{\"type\":\"abs\",\"data\":{{\"axis\":\"y\",\"value\":{y}}}}}"
        ));
    }

    fn send_button(&mut self, name: &str, down: bool) {
        self.send_events(&format!("{{\"type\":\"btn\",\"data\":{{\"button\":\"{name}\",\"down\":{down}}}}}"));
    }

    /// A button went down or up: 0 left, 1 right, 2 middle, 3 and 4 the side buttons.
    pub fn button(&mut self, button: u32, down: bool) {
        let Some(index) = usize::try_from(button).ok().filter(|&index| index < BUTTONS.len()) else {
            return;
        };
        if self.buttons[index] != down {
            self.buttons[index] = down;
            self.sent.buttons += 1;
            self.send_button(BUTTONS[index], down);
        }
    }

    /// The wheel turned by whole steps, to the right and up. Each step is a press and a release
    /// of one of the wheel's four buttons.
    pub fn wheel(&mut self, right: i32, up: i32) {
        for (steps, forward, back) in [(up, "wheel-up", "wheel-down"), (right, "wheel-right", "wheel-left")] {
            let name = if steps > 0 { forward } else { back };
            for _ in 0..steps.unsigned_abs().min(MOST_STEPS.unsigned_abs()) {
                self.sent.steps += 1;
                self.send_button(name, true);
                self.send_button(name, false);
            }
        }
    }

    /// Lets go of every key and button the guest has down.
    pub fn release_all(&mut self) {
        for linux in std::mem::take(&mut self.keys) {
            if let Some(&(_, _, qcode)) = keys::KEYS.iter().find(|&&(_, code, _)| code == linux) {
                self.send_key(keys::Key { linux, qcode }, false);
            }
        }
        for index in 0..BUTTONS.len() {
            self.button(index as u32, false);
        }
    }

    /// Asks the guest to shut down, as its power button would.
    pub fn power_down(&mut self) -> bool {
        match &mut self.qmp {
            Some(qmp) => {
                qmp.execute("{\"execute\":\"system_powerdown\"}");
                true
            }
            None => false,
        }
    }
}

/// Looks for QEMU's monitor socket for a while, from a thread of its own, and hands what it finds
/// to `found`. QEMU makes the socket before its devices, so it is there once QEMU has connected
/// to this process.
pub fn connect_later(path: std::path::PathBuf, found: impl FnOnce(Qmp) + Send + 'static) {
    thread::spawn(move || {
        for _ in 0..CONNECT_TRIES {
            match Qmp::connect(&path) {
                Ok(qmp) => {
                    eprintln!("pvg-host: input goes to QEMU through {}", path.display());
                    found(qmp);
                    return;
                }
                Err(_) => thread::sleep(CONNECT_PAUSE),
            }
        }
        eprintln!("pvg-host: no QEMU monitor at {}, so the window's input goes nowhere", path.display());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Instant;

    /// A folder of a test's own, gone when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("pvg-host-input-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Answers as QEMU's monitor does, and passes on each line it is sent.
    fn fake_qemu(path: &Path) -> mpsc::Receiver<String> {
        let listener = UnixListener::bind(path).unwrap();
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"{\"QMP\": {\"version\": {}}}\n").unwrap();
            for line in BufReader::new(stream.try_clone().unwrap()).lines() {
                let line = line.unwrap();
                let reply = if line.contains("nonsense") { "{\"error\": {}}" } else { "{\"return\": {}}" };
                writeln!(stream, "{reply}").unwrap();
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        lines
    }

    fn wait_for(condition: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < end, "it never happened");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn next(lines: &mpsc::Receiver<String>) -> String {
        lines.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    fn connected(scratch: &Scratch) -> (Input, mpsc::Receiver<String>) {
        let path = scratch.0.join("input.sock");
        let lines = fake_qemu(&path);
        let mut input = Input::new(false);
        input.connected(Qmp::connect(&path).unwrap());
        assert_eq!(next(&lines), "{\"execute\":\"qmp_capabilities\"}");
        wait_for(|| input.state().1);
        (input, lines)
    }

    #[test]
    fn a_point_is_scaled_into_the_picture_with_its_bars() {
        // A picture of the window's shape fills it.
        assert_eq!(absolute(0.0, 0.0, 960.0, 540.0, (1920, 1080)), Some((0, 0)));
        assert_eq!(absolute(480.0, 270.0, 960.0, 540.0, (1920, 1080)), Some((16384, 16384)));
        assert_eq!(absolute(960.0, 540.0, 960.0, 540.0, (1920, 1080)), Some((RANGE, RANGE)));
        // A pixel's place comes back as that pixel: value * 1920 / 32768.
        let (x, _) = absolute(100.25, 0.0, 960.0, 540.0, (1920, 1080)).unwrap();
        assert_eq!(u64::from(x) * 1920 / 32768, 200);
        // A 4:3 picture in a wide window has bars left and right, and a point on one is the edge.
        assert_eq!(absolute(120.0, 0.0, 960.0, 540.0, (1024, 768)), Some((0, 0)));
        assert_eq!(absolute(10.0, 270.0, 960.0, 540.0, (1024, 768)), Some((0, 16384)));
        assert_eq!(absolute(480.0, 270.0, 960.0, 540.0, (1024, 768)), Some((16384, 16384)));
        assert_eq!(absolute(900.0, 600.0, 960.0, 540.0, (1024, 768)), Some((RANGE, RANGE)));
        assert_eq!(absolute(1.0, 1.0, 960.0, 540.0, (0, 0)), None);
    }

    #[test]
    fn keys_go_by_their_qemu_names_once_each_way() {
        let scratch = Scratch::new("keys");
        let (mut input, lines) = connected(&scratch);
        input.key(0x00, true);
        input.key(0x00, true);
        input.key(0x00, false);
        input.key(0x00, false);
        input.key(0x3f, true);
        input.key(0x24, true);
        assert_eq!(
            next(&lines),
            "{\"execute\":\"input-send-event\",\"arguments\":{\"events\":[{\"type\":\"key\",\"data\":{\"key\":{\"type\":\"qcode\",\"data\":\"a\"},\"down\":true}}]}}"
        );
        assert!(next(&lines).contains("\"data\":\"a\"},\"down\":false"));
        assert!(next(&lines).contains("\"data\":\"ret\"},\"down\":true"));
        assert_eq!(input.sent().keys, 3);
    }

    #[test]
    fn caps_lock_is_a_press_and_a_release_each_time() {
        let scratch = Scratch::new("caps");
        let (mut input, lines) = connected(&scratch);
        for _ in 0..2 {
            input.key(keys::CAPS_LOCK, true);
            input.key(keys::CAPS_LOCK, false);
        }
        for down in [true, false, true, false] {
            assert!(next(&lines).contains(&format!("\"caps_lock\"}},\"down\":{down}")));
        }
    }

    #[test]
    fn moves_buttons_and_the_wheel() {
        let scratch = Scratch::new("pointer");
        let (mut input, lines) = connected(&scratch);
        input.point(10, 20);
        input.point(10, 20);
        input.button(0, true);
        input.button(0, true);
        input.wheel(0, -2);
        input.wheel(1, 0);
        input.button(9, true);
        assert_eq!(
            next(&lines),
            "{\"execute\":\"input-send-event\",\"arguments\":{\"events\":[{\"type\":\"abs\",\"data\":{\"axis\":\"x\",\"value\":10}},{\"type\":\"abs\",\"data\":{\"axis\":\"y\",\"value\":20}}]}}"
        );
        assert!(next(&lines).contains("{\"button\":\"left\",\"down\":true}"));
        for _ in 0..2 {
            assert!(next(&lines).contains("{\"button\":\"wheel-down\",\"down\":true}"));
            assert!(next(&lines).contains("{\"button\":\"wheel-down\",\"down\":false}"));
        }
        assert!(next(&lines).contains("{\"button\":\"wheel-right\",\"down\":true}"));
        assert!(next(&lines).contains("{\"button\":\"wheel-right\",\"down\":false}"));
        assert_eq!(input.sent(), Sent { keys: 0, moves: 1, buttons: 1, steps: 3 });
    }

    #[test]
    fn losing_the_keyboard_lets_go_of_what_is_down_and_only_that() {
        let scratch = Scratch::new("release");
        let (mut input, lines) = connected(&scratch);
        input.key(0x38, true);
        input.key(0x00, true);
        input.button(1, true);
        input.release_all();
        input.release_all();
        input.power_down();
        let seen: Vec<String> = (0..7).map(|_| next(&lines)).collect();
        // By their Linux codes: a is 30, the left Shift 42.
        assert!(seen[3].contains("\"a\"},\"down\":false"));
        assert!(seen[4].contains("\"shift\"},\"down\":false"));
        assert!(seen[5].contains("{\"button\":\"right\",\"down\":false}"));
        assert_eq!(seen[6], "{\"execute\":\"system_powerdown\"}");
        assert_eq!(input.sent().keys, 4);
    }

    #[test]
    fn nothing_is_sent_until_qemu_answers() {
        let scratch = Scratch::new("waiting");
        let path = scratch.0.join("input.sock");
        // A QEMU that has not taken the connection yet.
        let _listener = UnixListener::bind(&path).unwrap();
        let mut input = Input::new(false);
        input.connected(Qmp::connect(&path).unwrap());
        input.key(0x00, true);
        let qmp = input.qmp.as_ref().unwrap();
        assert!(qmp.unsent.is_empty(), "the greeting went, and a key has nowhere to go yet");
        assert_eq!(input.state(), (true, false, 0));
    }

    #[test]
    fn no_socket_means_nothing_is_sent_and_nothing_breaks() {
        let mut input = Input::new(true);
        input.key(0x0a, true);
        input.point(1, 1);
        assert!(!input.power_down());
        assert_eq!(input.state(), (false, false, 0));
    }
}
