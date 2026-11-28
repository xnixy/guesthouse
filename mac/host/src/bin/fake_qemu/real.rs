// SPDX-License-Identifier: GPL-2.0-or-later
//! A stand-in for QEMU, for trying the agent with no guest. It takes the arguments the agent
//! gives QEMU, opens the `-qmp unix:...` sockets and speaks enough QMP: the greeting,
//! `qmp_capabilities`, `system_powerdown` (it exits half a second later) and `quit` (at once).
//! Its odd behaviours are chosen by words in the disk image's file name:
//!
//! - `failstart`: prints a line to standard error and exits 1 before opening anything
//! - `stubborn`: ignores `system_powerdown`; `quit` still ends it
//! - `deaf`: ignores both, so only SIGTERM ends it
//! - `selfexit`: exits 0 after two seconds, as Windows shutting itself down would
//! - `crash`: exits 3 after two seconds
//!
//! With `-device pvg-fwd,...,socket=PATH` it connects to that socket, as QEMU's forwarding
//! device does to the graphics process, and holds the connection until it exits; with no
//! process there it fails as QEMU does. `-device help` lists `pvg-fwd`.
//!
//! It writes the arguments it was given, one a line, to `fake-qemu.args` in its folder.
#![allow(clippy::disallowed_methods)] // a stand-in for trying the agent, reading its own sockets

#[path = "../fake_parent/mod.rs"]
mod fake_parent;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::exit;
use std::thread;
use std::time::Duration;

const GREETING: &str = r#"{"QMP": {"version": {"qemu": {"micro": 0, "minor": 2, "major": 10}, "package": "fake"}, "capabilities": []}}"#;
const OK: &str = "{\"return\": {}}\n";

#[derive(Clone, Copy)]
struct Quirks {
    stubborn: bool,
    deaf: bool,
}

pub fn main() {
    fake_parent::end_with_parent();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["-device", "help"] {
        println!("Misc devices:\nname \"pvg-fwd\", bus PCI, desc \"forwards to the graphics process\"");
        return;
    }
    let _ = std::fs::write("fake-qemu.args", args.join("\n") + "\n");
    let disk = args
        .windows(2)
        .find(|pair| pair[0] == "-drive" && pair[1].contains("id=hd"))
        .map(|pair| pair[1].clone())
        .unwrap_or_default();
    let has = |word: &str| disk.contains(word);

    if has("failstart") {
        eprintln!("fake-qemu: could not open the disk image: Operation not permitted");
        eprintln!("a second line the agent must not use");
        exit(1);
    }
    let quirks = Quirks { stubborn: has("stubborn") || has("deaf"), deaf: has("deaf") };
    // The graphics process, if the machine is on the guarded path. The connection is held for
    // as long as this process lives, which is how the process knows when QEMU is gone.
    let forwarded: Vec<UnixStream> = args
        .windows(2)
        .filter(|pair| pair[0] == "-device" && pair[1].starts_with("pvg-fwd"))
        .map(|pair| {
            let path = pair[1].split(',').find_map(|part| part.strip_prefix("socket=")).unwrap_or("");
            UnixStream::connect(path).unwrap_or_else(|error| {
                eprintln!("fake-qemu: pvg-fwd cannot reach the graphics process at {path}: {error}");
                exit(1);
            })
        })
        .collect();
    std::mem::forget(forwarded);
    for sockets in args.windows(2).filter(|pair| pair[0] == "-qmp") {
        let path = sockets[1].trim_start_matches("unix:").split(',').next().unwrap_or("").to_string();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("a QMP socket");
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || answer(stream, quirks));
            }
        });
    }
    if has("selfexit") {
        thread::sleep(Duration::from_secs(2));
        exit(0);
    }
    if has("crash") {
        thread::sleep(Duration::from_secs(2));
        exit(3);
    }
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// One QMP client, like QEMU: a greeting, then an answer to every command.
fn answer(stream: UnixStream, quirks: Quirks) {
    let Ok(mut output) = stream.try_clone() else {
        return;
    };
    let _ = writeln!(output, "{GREETING}");
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let _ = output.write_all(OK.as_bytes());
        if line.contains("system_powerdown") && !quirks.stubborn {
            thread::sleep(Duration::from_millis(500));
            exit(0);
        }
        if line.contains("\"quit\"") && !quirks.deaf {
            exit(0);
        }
    }
}
