// SPDX-License-Identifier: GPL-2.0-or-later
//! The agent's Unix socket: what the app talks to when the agent was not started by launchd.
//! One request or event per line, as in `protocol`. Each connection hears every state change
//! from the moment it connects, and its answers share the line with them.
//!
//! The same file holds the other end, `Connection`, which the app uses.

use crate::agent::Agent;
use guesthouse_basics::capped_line::read_capped_line;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// A fresh agent that nobody has talked to, and one left with nothing to do, goes after this.
const IDLE_LIMIT: Duration = Duration::from_secs(10);
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// Why `serve` did not run.
#[derive(Debug)]
pub enum ServeError {
    /// Another agent holds the lock.
    AlreadyRunning,
    Io(io::Error),
}

impl From<io::Error> for ServeError {
    fn from(error: io::Error) -> Self {
        ServeError::Io(error)
    }
}

/// Answers on the socket at `path` until the agent has been idle for ten seconds. Takes the
/// lock `path` + ".lock" first, so two agents never share a socket.
pub fn serve(agent: &Agent, path: &Path) -> Result<(), ServeError> {
    let lock = lock_file(&path.with_extension("lock"))?;
    // The lock is the proof that no other agent owns this socket, so a file left there is stale.
    let _ = fs::remove_file(path);
    let listener = bind_owner_only(path)?;
    listener.set_nonblocking(true)?;
    let mut idle_since = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Only this user's own programs may talk to the agent, whatever the file's mode.
                if !peer_is_this_user(&stream) {
                    continue;
                }
                // A socket accepted from a non-blocking listener is non-blocking on macOS too.
                stream.set_nonblocking(false)?;
                let agent = agent.clone();
                thread::spawn(move || talk(&agent, stream));
                idle_since = Instant::now();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(error) => return Err(error.into()),
        }
        if !agent.is_idle() {
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= IDLE_LIMIT {
            let _ = fs::remove_file(path);
            drop(lock);
            return Ok(());
        }
    }
}

/// The mode of the socket, and of the folder it is in: this user only.
const SOCKET_MODE: u32 = 0o600;
const FOLDER_MODE: u32 = 0o700;

/// Binds the socket so that no other user can ever connect: the folder it is in is made owner-only,
/// the socket is created under a umask that gives no one else anything, and its mode is set again
/// after, in case the umask was not honoured.
fn bind_owner_only(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(folder) = path.parent().filter(|f| !f.as_os_str().is_empty()) {
        fs::create_dir_all(folder)?;
        fs::set_permissions(folder, fs::Permissions::from_mode(FOLDER_MODE))?;
    }
    // SAFETY: umask only changes this process's file creation mask; it is put back at once, and the
    // agent has started no other thread that creates files when it binds.
    let before = unsafe { libc::umask(0o077) };
    let bound = UnixListener::bind(path);
    // SAFETY: as above.
    unsafe { libc::umask(before) };
    let listener = bound?;
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))?;
    Ok(listener)
}

/// Whether the program on the other end of `stream` runs as the user this agent runs as.
fn peer_is_this_user(stream: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0 as libc::uid_t, 0 as libc::gid_t);
    // SAFETY: getpeereid reads the descriptor's peer credentials into the two integers.
    let found = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: geteuid has no arguments and cannot fail.
    found && uid == unsafe { libc::geteuid() }
}

fn lock_file(path: &Path) -> Result<File, ServeError> {
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    // SAFETY: flock on a descriptor we own; the lock goes when the file is closed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::WouldBlock {
            ServeError::AlreadyRunning
        } else {
            error.into()
        });
    }
    Ok(file)
}

/// The longest line the agent's socket is read for.
const MOST_LINE: usize = 1 << 20;

/// One connection: a writer thread owns the socket's output, so answers and events never mix
/// in the middle of a line.
fn talk(agent: &Agent, stream: UnixStream) {
    let Ok(output) = stream.try_clone() else {
        return;
    };
    let (sender, receiver) = mpsc::channel::<String>();
    let subscription = agent.subscribe(sender.clone());
    thread::spawn(move || write_lines(output, receiver));
    let mut reader = BufReader::new(stream);
    while let Ok(Some(line)) = read_capped_line(&mut reader, MOST_LINE) {
        if line.trim().is_empty() {
            continue;
        }
        if sender.send(agent.handle(&line)).is_err() {
            break;
        }
    }
    // Dropping the subscription and the sender ends the writer.
    drop(subscription);
}

fn write_lines(mut output: UnixStream, receiver: Receiver<String>) {
    for line in receiver {
        if output.write_all(line.as_bytes()).and_then(|()| output.write_all(b"\n")).is_err() {
            break;
        }
    }
    let _ = output.shutdown(std::net::Shutdown::Both);
}

/// The app's end: lines in, lines out. A thread reads the socket so `receive` can time out.
pub struct Connection {
    output: UnixStream,
    lines: Receiver<io::Result<String>>,
}

impl Connection {
    /// Connects to the agent's socket, which must be there already.
    pub fn open(path: &Path) -> io::Result<Connection> {
        let stream = UnixStream::connect(path)?;
        let input = stream.try_clone()?;
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || read_lines(input, sender));
        Ok(Connection { output: stream, lines })
    }

    /// The process id of the agent at the other end of the socket.
    pub fn peer_pid(&self) -> Option<i32> {
        use std::os::fd::AsRawFd;
        let mut pid: libc::pid_t = 0;
        let mut size = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: the socket is open for as long as `self`, and `pid` has room for its answer.
        let failed = unsafe {
            libc::getsockopt(
                self.output.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut size,
            )
        } != 0;
        (!failed && pid > 0).then_some(pid)
    }

    pub fn send(&self, line: &str) -> io::Result<()> {
        (&self.output).write_all(format!("{line}\n").as_bytes())
    }

    /// The next line, `Ok(None)` after `timeout` with nothing, or an error once the agent has
    /// hung up.
    pub fn receive(&self, timeout: Duration) -> io::Result<Option<String>> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => line.map(Some),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.output.shutdown(std::net::Shutdown::Both);
    }
}

fn read_lines(input: UnixStream, sender: Sender<io::Result<String>>) {
    let mut reader = BufReader::new(input);
    while let Some(line) = read_capped_line(&mut reader, MOST_LINE).transpose() {
        let failed = line.is_err();
        if sender.send(line).is_err() || failed {
            return;
        }
    }
    let _ = sender.send(Err(closed()));
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "the agent closed the connection")
}
