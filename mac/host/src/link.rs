// SPDX-License-Identifier: GPL-2.0-or-later
//! The QMP socket that input goes to. A thread of its own connects, does the handshake and
//! writes the commands, so the window never waits on QEMU: a hung guest cannot freeze it.
//!
//! Commands sent while the handshake runs wait for it; commands sent while there is no QEMU
//! are dropped, since old input means nothing later. The thread tries again every second.

use guesthouse_basics::capped_line::read_capped_line;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait between tries to reach QEMU.
const RETRY: Duration = Duration::from_secs(1);
/// How long QEMU has to greet us and answer `qmp_capabilities`.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long one command may take to write before the socket is given up on.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) const CAPABILITIES: &str = "{\"execute\":\"qmp_capabilities\"}\r\n";

enum Message {
    Command(String),
    Close,
}

/// The sending end. Dropping it lets the thread write what is queued and close the socket.
pub struct Link {
    sender: Sender<Message>,
}

impl Link {
    pub fn open(path: PathBuf) -> Link {
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("guesthouse-qmp".into())
            .spawn(move || run(&path, &receiver))
            .expect("a thread for the QMP socket");
        Link { sender }
    }

    pub fn send(&self, command: String) {
        // Fails only once the thread has stopped, when there is nothing left to tell.
        let _ = self.sender.send(Message::Command(command));
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Close);
    }
}

fn run(path: &Path, receiver: &Receiver<Message>) {
    let mut socket: Option<UnixStream> = None;
    let mut next_try = Instant::now();
    // The last failure told, so a missing socket is reported once, not every second.
    let mut last_error = String::new();
    let mut fail = |error: io::Error, next_try: &mut Instant| {
        let text = error.to_string();
        if text != last_error {
            eprintln!("guesthouse: QMP socket {}: {text}", path.display());
            last_error = text;
        }
        *next_try = Instant::now() + RETRY;
    };
    loop {
        if socket.is_none() && Instant::now() >= next_try {
            match connect(path) {
                Ok(stream) => socket = Some(stream),
                Err(error) => {
                    fail(error, &mut next_try);
                    // What queued during the try is input for a guest we could not reach.
                    while let Ok(message) = receiver.try_recv() {
                        if let Message::Close = message {
                            return;
                        }
                    }
                }
            }
        }
        match receiver.recv_timeout(RETRY) {
            Ok(Message::Command(command)) => {
                if let Some(stream) = socket.as_mut()
                    && let Err(error) = stream.write_all(command.as_bytes())
                {
                    hang_up(socket.take());
                    fail(error, &mut next_try);
                }
            }
            Ok(Message::Close) | Err(RecvTimeoutError::Disconnected) => {
                hang_up(socket.take());
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Closes the socket both ways, which also ends the thread that drains it.
fn hang_up(socket: Option<UnixStream>) {
    if let Some(socket) = socket {
        let _ = socket.shutdown(Shutdown::Both);
    }
}

/// Connects and does the handshake: QEMU's greeting, then `qmp_capabilities` and its answer.
/// A second thread then reads and drops everything QEMU says, answers and events alike, so
/// the socket never fills.
fn connect(path: &Path) -> io::Result<UnixStream> {
    let stream = connect_long(path)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let greeting = read_line(&mut reader)?;
    if !greeting.contains("\"QMP\"") {
        return Err(refused("no QMP greeting"));
    }
    (&stream).write_all(CAPABILITIES.as_bytes())?;
    loop {
        let line = read_line(&mut reader)?;
        if line.contains("\"return\"") {
            break;
        }
        if line.contains("\"error\"") {
            return Err(refused("qmp_capabilities refused"));
        }
        // Anything else is an event QEMU sent meanwhile.
    }
    stream.set_read_timeout(None)?;
    thread::Builder::new().name("guesthouse-qmp-drain".into()).spawn(move || {
        let mut sink = [0u8; 4096];
        let mut reader = reader;
        while matches!(reader.read(&mut sink), Ok(n) if n > 0) {}
    })?;
    Ok(stream)
}

/// The longest socket path a `sockaddr_un` holds on macOS, less its ending nul.
const MAX_SOCKET_PATH: usize = 103;

/// Connects to `path` even when it is longer than a socket address can hold, as run folders
/// often are: through a short symbolic link in the temporary folder, removed straight after.
pub(crate) fn connect_long(path: &Path) -> io::Result<UnixStream> {
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return UnixStream::connect(path);
    }
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let link = std::env::temp_dir().join(format!(
        "guesthouse-qmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(path, &link)?;
    let result = UnixStream::connect(&link);
    let _ = std::fs::remove_file(&link);
    result
}

/// The longest line of QEMU's monitor that is read.
const MOST_QMP_LINE: usize = 1 << 20;

pub(crate) fn read_line(reader: &mut impl BufRead) -> io::Result<String> {
    read_capped_line(reader, MOST_QMP_LINE)?.ok_or_else(|| refused("QEMU closed the socket"))
}

pub(crate) fn refused(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionRefused, why)
}
