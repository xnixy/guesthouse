//! What the app and the program say to each other: one request as a line of JSON on a Unix socket in
//! the run folder, and one line back. The monitor and the daemon are reached the same way, and a
//! socket in the run folder is only open to this user. A program that is not running has no answer,
//! which is how the app finds out it is not there.
//!
//! What the lines say is `guesthouse-relay`'s, as the network side they ask is.

use super::SOCKET;
use guesthouse_basics::capped_line::read_capped_line;
pub use guesthouse_relay::{Counters, RelayClient, RelayReply, RelayStatus, Request, ShareState};
use std::io::{self, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A request is a few dozen bytes. A longer line is not one of ours.
const LONGEST_REQUEST: usize = 4096;
/// A status with the most devices the relay allows is a few kilobytes.
const LONGEST_REPLY: usize = 256 * 1024;
/// How long either side waits for the other to say its line.
const LINE_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_MODE: u32 = 0o600;

pub fn socket_path(run_folder: &Path) -> PathBuf {
    run_folder.join(SOCKET)
}

/// Asks the program of the machine in this run folder. An error means it did not answer: it is not
/// running, or not any more.
pub fn ask(run_folder: &Path, request: &Request) -> io::Result<RelayReply> {
    let mut stream = UnixStream::connect(socket_path(run_folder))?;
    stream.set_read_timeout(Some(LINE_TIMEOUT))?;
    stream.set_write_timeout(Some(LINE_TIMEOUT))?;
    let mut line = serde_json::to_string(request).map_err(io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let answer = read_capped_line(&mut BufReader::new(stream), LONGEST_REPLY)?.unwrap_or_default();
    serde_json::from_str(&answer).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Makes the socket, replacing one an earlier run left, for only this user.
pub fn listener(run_folder: &Path) -> io::Result<UnixListener> {
    let path = socket_path(run_folder);
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    Ok(listener)
}

/// Answers requests, one connection at a time, until the listener fails. A request that is not one
/// of ours gets an error and costs nothing else. Run it on a thread of its own.
pub fn serve(listener: &UnixListener, mut answer: impl FnMut(Request) -> RelayReply) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let _ = serve_one(stream, &mut answer);
    }
}

fn serve_one(stream: UnixStream, answer: &mut impl FnMut(Request) -> RelayReply) -> io::Result<()> {
    stream.set_read_timeout(Some(LINE_TIMEOUT))?;
    stream.set_write_timeout(Some(LINE_TIMEOUT))?;
    let line = read_capped_line(&mut BufReader::new(&stream), LONGEST_REQUEST)?.unwrap_or_default();
    let reply = match serde_json::from_str(&line) {
        Ok(request) => answer(request),
        Err(error) => RelayReply::Error(format!("not a request: {error}")),
    };
    let mut out = serde_json::to_string(&reply).map_err(io::Error::other)?;
    out.push('\n');
    (&stream).write_all(out.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;
    use std::io::BufRead;
    use std::thread;

    fn status(state: ShareState) -> RelayStatus {
        RelayStatus {
            machine: "m".into(),
            state,
            address: Some("192.168.1.5".parse().unwrap()),
            port: Some(7800),
            loopback_port: 19065,
            reason: None,
            counters: Counters::default(),
            clients: vec![RelayClient {
                address: "192.168.1.9:5000".into(),
                idle_ms: 20,
                up_packets: 3,
                down_packets: 4,
            }],
        }
    }

    #[test]
    fn a_request_gets_its_answer_over_the_socket() {
        let scratch = Scratch::new();
        let folder = scratch.path("run");
        std::fs::create_dir_all(&folder).unwrap();
        let listener = listener(&folder).unwrap();
        assert_eq!(
            std::fs::metadata(socket_path(&folder)).unwrap().permissions().mode() & 0o777,
            SOCKET_MODE
        );
        thread::spawn(move || {
            serve(&listener, |request| match request {
                Request::Status => RelayReply::Status(status(ShareState::Listening)),
                _ => RelayReply::Error("no".into()),
            })
        });
        assert_eq!(
            ask(&folder, &Request::Status).unwrap(),
            RelayReply::Status(status(ShareState::Listening))
        );
        assert_eq!(ask(&folder, &Request::Close).unwrap(), RelayReply::Error("no".into()));
    }

    #[test]
    fn nonsense_gets_an_error_and_the_next_request_still_works() {
        let scratch = Scratch::new();
        let folder = scratch.path("run");
        std::fs::create_dir_all(&folder).unwrap();
        let listener = listener(&folder).unwrap();
        thread::spawn(move || serve(&listener, |_| RelayReply::Status(status(ShareState::Off))));
        let mut stream = UnixStream::connect(socket_path(&folder)).unwrap();
        stream.write_all(b"{\"op\":\"explode\"}\n").unwrap();
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer).unwrap();
        assert!(matches!(serde_json::from_str::<RelayReply>(&answer).unwrap(), RelayReply::Error(_)));
        assert!(matches!(ask(&folder, &Request::Status).unwrap(), RelayReply::Status(_)));
    }

    #[test]
    fn nobody_listening_is_an_error() {
        let scratch = Scratch::new();
        assert!(ask(scratch.root(), &Request::Status).is_err());
    }

    #[test]
    fn an_old_socket_is_replaced() {
        let scratch = Scratch::new();
        let first = listener(scratch.root()).unwrap();
        drop(first);
        assert!(listener(scratch.root()).is_ok(), "the file the first left behind does not stop the second");
    }
}
