// SPDX-License-Identifier: GPL-2.0-or-later
//! The agent's socket is private to this user: its folder is 0700, the socket 0600, and a program
//! of this user can still talk to it.
#![cfg(target_os = "macos")]
#![allow(clippy::disallowed_methods)] // a test's own sockets

use guesthouse_core::agent::{Agent, Config, InstallConfig};
use guesthouse_core::server;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("ghs-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("Machines")).unwrap();
    // A folder that is open to everyone, as a default umask would leave it.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn the_socket_is_owner_only_in_an_owner_only_folder_and_its_owner_can_use_it() {
    let base = root("mode");
    let folder = base.join("run");
    fs::create_dir_all(&folder).unwrap();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = folder.join("agent.sock");
    let agent = Agent::new(Config {
        machines: base.join("Machines"),
        qemu: None,
        rmhost: None,
        pvg_host: None,
        install: InstallConfig { fake: true, driver_dir: None, firmware_dir: None },
        shutdown_timeout: Duration::from_secs(5),
        start_timeout: Duration::from_secs(10),
    });
    let served = socket.clone();
    thread::spawn(move || {
        let _ = server::serve(&agent, &served);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "the socket never appeared");
        thread::sleep(Duration::from_millis(20));
    }
    let mode = |path: &PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(&folder), 0o700);
    // The same user connects and is answered.
    use std::io::{BufRead, BufReader, Write};
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream.write_all(b"{\"v\":1,\"id\":1,\"op\":\"list\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    assert!(line.contains("\"ok\":true"), "{line}");
    let _ = fs::remove_dir_all(base);
}
