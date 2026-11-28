// SPDX-License-Identifier: GPL-2.0-or-later
//! A stand-in for `guesthouse-pvg-host`, for trying the agent with no graphics. It takes the
//! arguments the agent gives the real one, writes them one a line to `fake-pvg-host.args`, opens
//! the `--socket` that QEMU's `pvg-fwd` connects to and the `--control` socket (which answers
//! `status` with a line ending in the frame counts), and exits when the connection to the
//! socket ends, as the real one does when its QEMU goes. An option it does not know, as
//! `--help`, gets the usage line, which names `--frames`, as the build that can publish does.
//!
//! Its odd behaviours are chosen by words in the `--frames` name, which holds the machine's id:
//!
//! - `hostfail`: prints a line and exits 1 before opening its socket
//! - `hostdie`: exits 3 a second and a half after QEMU connects
//! - `hoststuck`: stays when QEMU goes, so only SIGTERM ends it
//! - `hostnomodes`: answers `modes` with an error, as a device that refuses the list
#![allow(clippy::disallowed_methods)] // a stand-in for trying the agent, reading its own sockets

#[path = "../fake_parent/mod.rs"]
mod fake_parent;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::process::exit;
use std::thread;
use std::time::Duration;

const USAGE: &str = "pvg-host: usage: guesthouse-pvg-host --socket PATH [--control PATH] [--modes WxH@R,...] [--name NAME] [--window] [--frames NAME]";

pub fn main() {
    fake_parent::end_with_parent();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut socket, mut control, mut frames) = (None, None, String::new());
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let mut value = || words.next().cloned().unwrap_or_default();
        match word.as_str() {
            "--socket" => socket = Some(value()),
            "--control" => control = Some(value()),
            "--frames" => frames = value(),
            "--modes" | "--name" => {
                value();
            }
            "--window" => {}
            _ => {
                eprintln!("{USAGE}");
                exit(1);
            }
        }
    }
    let _ = std::fs::write("fake-pvg-host.args", args.join("\n") + "\n");
    let Some(socket) = socket else {
        eprintln!("{USAGE}");
        exit(1);
    };
    if frames.contains("hostfail") {
        eprintln!("pvg-host: the frame name {frames} is taken");
        exit(1);
    }
    let stuck = frames.contains("hoststuck");
    let dies = frames.contains("hostdie");
    let no_modes = frames.contains("hostnomodes");

    if let Some(control) = control {
        let _ = std::fs::remove_file(&control);
        let listener = UnixListener::bind(&control).expect("a control socket");
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut output = stream.try_clone().expect("a clone");
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let (word, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
                    if word == "status" {
                        let _ = writeln!(output, "ok published 0 skipped 0 dropped 0 failed 0");
                    } else if word == "modes" && no_modes {
                        let _ = writeln!(output, "error the device refused the modes");
                    } else if word == "modes" && !rest.is_empty() {
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("fake-pvg-host.modes")
                            .expect("the modes file");
                        let _ = writeln!(file, "{rest}");
                        let _ = writeln!(output, "ok {} modes", rest.split(',').count());
                    }
                }
            }
        });
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("the device socket");
    for stream in listener.incoming().flatten() {
        thread::spawn(move || {
            let mut stream = stream;
            if dies {
                thread::sleep(Duration::from_millis(1500));
                exit(3);
            }
            // Until QEMU lets go of the connection.
            let mut sink = [0u8; 256];
            while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
            if !stuck {
                exit(0);
            }
        });
    }
}
