//! A pretend helper for tests: a TCP server on loopback that speaks the guest's side of the contract
//! from what a test tells it. It checks the token the way the real one does, says hello and the layout,
//! answers requests, and can be made to do what a real one does when things go wrong: close without a
//! word, say nothing, speak another version, drop the connection, or be replaced by a second client.

use super::message::{
    Layout, PROTOCOL_VERSION, SIDE_WORD, Size, answer_line, desktop_line, hello_line, layout_line,
};
use guesthouse_basics::capped_line::read_capped_line;
use serde_json::Value;
use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What it does with the next connection that gives the right token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Welcome {
    /// Says hello and the layout.
    Hello,
    /// Closes at once, as QEMU's forward does when nothing listens behind it.
    Close,
    /// Takes the token and says nothing.
    Silent,
}

/// The lines to write back to a request, given the request as the host sent it.
pub type Responder = Box<dyn FnMut(&Value, &Mutex<Inner>) -> Vec<String> + Send>;

pub struct Inner {
    pub session: String,
    pub screen: Size,
    pub scale: u32,
    pub version: u64,
    pub layout: Layout,
    /// A line to send in place of the layout, for a helper that sends one this build cannot read.
    pub layout_line: Option<String>,
    pub welcome: Welcome,
    pub requests: Vec<Value>,
    pub tokens_refused: usize,
    pub connections: usize,
    /// Side connections taken, which never replace the driving one.
    pub side_connections: usize,
    current: Option<TcpStream>,
}

pub struct FakeHelper {
    pub port: u16,
    inner: Arc<Mutex<Inner>>,
    responder: Arc<Mutex<Responder>>,
    stop: Arc<AtomicBool>,
}

/// Answers every request that has an `n` with `ok`, and a `layout` request with the layout too.
fn answer_all(request: &Value, inner: &Mutex<Inner>) -> Vec<String> {
    let n = request["n"].as_u64().unwrap_or(0);
    let mut lines = vec![answer_line(n, true, "")];
    if request["t"] == "layout" {
        lines.insert(0, layout_line(&inner.lock().unwrap().layout));
    }
    lines
}

impl FakeHelper {
    pub fn start(token: &str) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let screen = Size { w: 1920, h: 1080 };
        let layout = Layout { seq: 1, screen, cells: Vec::new() };
        let inner = Arc::new(Mutex::new(Inner {
            session: "session-one".into(),
            screen,
            scale: 100,
            version: PROTOCOL_VERSION,
            layout,
            layout_line: None,
            welcome: Welcome::Hello,
            requests: Vec::new(),
            tokens_refused: 0,
            connections: 0,
            side_connections: 0,
            current: None,
        }));
        let responder: Arc<Mutex<Responder>> = Arc::new(Mutex::new(Box::new(answer_all)));
        let stop = Arc::new(AtomicBool::new(false));
        let helper = Self { port, inner: inner.clone(), responder: responder.clone(), stop: stop.clone() };
        let token = token.to_owned();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let (inner, responder, token) = (inner.clone(), responder.clone(), token.clone());
                thread::spawn(move || serve(stream, &token, &inner, &responder));
            }
        });
        helper
    }

    /// The helper's state, for a closure that has to change it from another thread.
    pub fn inner_for_test(&self) -> Arc<Mutex<Inner>> {
        self.inner.clone()
    }

    pub fn with<T>(&self, change: impl FnOnce(&mut Inner) -> T) -> T {
        change(&mut self.inner.lock().unwrap())
    }

    pub fn respond_with(&self, responder: impl FnMut(&Value, &Mutex<Inner>) -> Vec<String> + Send + 'static) {
        *self.responder.lock().unwrap() = Box::new(responder);
    }

    /// Writes a line to the connection that is open, whatever it is.
    pub fn say(&self, line: &str) {
        let stream = self.inner.lock().unwrap().current.as_ref().and_then(|s| s.try_clone().ok());
        if let Some(mut stream) = stream {
            let _ = stream.write_all(format!("{line}\n").as_bytes());
        }
    }

    /// Takes a layout as the current one and sends it to the open connection.
    pub fn show(&self, layout: Layout) {
        let line = layout_line(&layout);
        self.with(|inner| inner.layout = layout);
        self.say(&line);
    }

    pub fn say_desktop(&self, kind: &str) {
        self.say(&desktop_line(kind));
    }

    /// Closes the open connection from the helper's end.
    pub fn drop_connection(&self) {
        if let Some(stream) = self.inner.lock().unwrap().current.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    pub fn requests(&self) -> Vec<Value> {
        self.inner.lock().unwrap().requests.clone()
    }

    pub fn connections(&self) -> usize {
        self.inner.lock().unwrap().connections
    }

    /// Waits until `enough` holds of what the helper has seen, or two seconds are up.
    pub fn wait_until(&self, enough: impl Fn(&Inner) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if enough(&self.inner.lock().unwrap()) {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

impl Drop for FakeHelper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.drop_connection();
        // A last connection wakes the accept loop so it can end.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

/// The longest line a connection is read for.
const MOST_LINE: usize = 1 << 16;

fn serve(stream: TcpStream, token: &str, inner: &Mutex<Inner>, responder: &Mutex<Responder>) {
    let Ok(reading) = stream.try_clone() else { return };
    let mut reader = BufReader::new(reading);
    let read = read_capped_line(&mut reader, MOST_LINE);
    let first = read.as_ref().ok().cloned().flatten().unwrap_or_default();
    let (given, side) = match first.trim_end().split_once(' ') {
        Some((given, word)) if word == SIDE_WORD => (given, true),
        _ => (first.trim_end(), false),
    };
    if read.is_err() || given != token {
        inner.lock().unwrap().tokens_refused += 1;
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let mut writer = stream.try_clone().unwrap();
    let welcome = {
        let mut inner = inner.lock().unwrap();
        // One driving connection at a time: a second replaces the first. A side one is beside it.
        if side {
            inner.side_connections += 1;
        } else {
            if let Some(old) = inner.current.replace(stream.try_clone().unwrap()) {
                let _ = old.shutdown(Shutdown::Both);
            }
            inner.connections += 1;
        }
        inner.welcome
    };
    match welcome {
        Welcome::Close => {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        Welcome::Hello => {
            let (hello, layout) = {
                let inner = inner.lock().unwrap();
                let hello = hello_line(&inner.session, inner.screen, inner.scale)
                    .replace(&format!("\"v\":{PROTOCOL_VERSION}"), &format!("\"v\":{}", inner.version));
                (hello, inner.layout_line.clone().unwrap_or_else(|| layout_line(&inner.layout)))
            };
            let _ = writer.write_all(format!("{hello}\n{layout}\n").as_bytes());
        }
        Welcome::Silent => {}
    }
    loop {
        let Ok(Some(line)) = read_capped_line(&mut reader, MOST_LINE) else { return };
        let Ok(request) = serde_json::from_str::<Value>(line.trim_end()) else { continue };
        inner.lock().unwrap().requests.push(request.clone());
        let replies = (responder.lock().unwrap())(&request, inner);
        for reply in replies {
            if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
                return;
            }
        }
    }
}
