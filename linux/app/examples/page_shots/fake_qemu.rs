//! This program under QEMU's name: monitors that answer as a running VM's do, and a display. No VM runs.

#![allow(clippy::disallowed_methods)] // This is the fake QEMU of the page shots.

use crate::fake_display;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process;
use std::thread;
use std::time::Duration;

const LIFE: Duration = Duration::from_secs(60);
const DISPLAY_SOCKET: &str = "display.sock";

fn serve_monitor(mut stream: UnixStream) {
    let _ = stream.write_all(b"{\"QMP\": {}}\n");
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines().map_while(Result::ok) {
        let request: Value = serde_json::from_str(&line).unwrap_or_default();
        let answer = match request["execute"].as_str() {
            Some("query-status") => json!({"return": {"status": "running"}}),
            Some("query-mice") => json!({"return": [], "id": "mice"}),
            Some("system_powerdown") => process::exit(0),
            _ => json!({"return": {}}),
        };
        let _ = writeln!(stream, "{answer}");
    }
}

fn listen_on(path: &Path) {
    if path.ends_with(DISPLAY_SOCKET) {
        fake_display::serve(path);
        return;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            thread::spawn(move || serve_monitor(stream));
        }
    });
}

/// Takes the arguments the app gave QEMU.
pub fn run(arguments: &[String]) -> ! {
    for pair in arguments.windows(2).filter(|pair| pair[0] == "-qmp") {
        listen_on(Path::new(pair[1].trim_start_matches("unix:").split(',').next().unwrap()));
    }
    thread::sleep(LIFE);
    process::exit(0)
}
