// SPDX-License-Identifier: GPL-2.0-or-later
//! Input through the same C functions the screen window calls, into a stand-in for QEMU's
//! QMP socket that greets, answers every line and records what it got.
#![cfg(target_os = "macos")]
#![allow(clippy::disallowed_methods)] // a test's own sockets

use guesthouse_core::*;
use serde_json::{Value, json};
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;

/// Accepts one client, greets it like QEMU, and returns every line it sent until it closed.
fn stand_in(path: PathBuf) -> thread::JoinHandle<Vec<Value>> {
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        writer.write_all(b"{\"QMP\": {\"version\": {}, \"capabilities\": [\"oob\"]}}\r\n").unwrap();
        let mut lines = Vec::new();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            lines.push(serde_json::from_str(&line).unwrap());
            // An event in between, as QEMU sends them, before the answer. The client may
            // have hung up after its last line, so a failed write is fine.
            let _ = writer.write_all(b"{\"event\": \"RESET\", \"data\": {}}\r\n");
            let _ = writer.write_all(b"{\"return\": {}}\r\n");
        }
        let _ = std::fs::remove_file(&path);
        lines
    })
}

fn send(events: Value) -> Value {
    json!({ "execute": "input-send-event", "arguments": { "events": events } })
}

fn key(name: &str, down: bool) -> Value {
    json!({ "type": "key", "data": { "down": down, "key": { "type": "qcode", "data": name } } })
}

fn btn(name: &str, down: bool) -> Value {
    json!({ "type": "btn", "data": { "down": down, "button": name } })
}

fn abs(x: u32, y: u32) -> Value {
    json!([
        { "type": "abs", "data": { "axis": "x", "value": x } },
        { "type": "abs", "data": { "axis": "y", "value": y } }
    ])
}

#[test]
fn scripted_input_reaches_qmp_and_held_keys_are_let_go_on_close() {
    // Longer than a socket address holds, as run folders often are.
    let folder = std::env::temp_dir().join(format!(
        "guesthouse-qmp-test-{}-a-folder-name-long-enough-to-pass-the-socket-limit",
        std::process::id()
    ));
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("rmhost-qmp.sock");
    assert!(path.as_os_str().len() > 104);
    // The stand-in binds from inside the folder, where the name is short.
    std::env::set_current_dir(&folder).unwrap();
    let server = stand_in(PathBuf::from("rmhost-qmp.sock"));
    let c_path = CString::new(path.to_str().unwrap()).unwrap();

    // SAFETY: one handle, used by this thread only, closed once at the end.
    let lines = unsafe {
        let input = guesthouse_input_open(c_path.as_ptr());
        assert!(!input.is_null());
        // A 1080p picture in a 1000 x 1000 view: the picture runs from y 218.75 to 781.25.
        guesthouse_input_pointer(input, 500.0, 500.0, 1000.0, 1000.0, 1920.0, 1080.0);
        guesthouse_input_pointer(input, 500.0, 500.0, 1000.0, 1000.0, 1920.0, 1080.0); // no move
        guesthouse_input_pointer(input, 10.0, 5.0, 1000.0, 1000.0, 1920.0, 1080.0); // in the bar
        guesthouse_input_button(input, 0, true);
        guesthouse_input_button(input, 0, false);
        guesthouse_input_button(input, 7, true); // no such button
        // Shift, A, A up, Shift up.
        guesthouse_input_modifier(input, 0x38, true);
        guesthouse_input_key(input, 0x00, true);
        guesthouse_input_key(input, 0x00, false);
        guesthouse_input_modifier(input, 0x38, false);
        // A mouse wheel notch up, then a trackpad scroll down that makes one step.
        guesthouse_input_scroll(input, 0.0, 1.0, false);
        guesthouse_input_scroll(input, 0.0, -25.0, true);
        guesthouse_input_scroll(input, 0.0, -25.0, true);
        // Command (the Windows key), Fn (no name), F12.
        guesthouse_input_modifier(input, 0x37, true);
        guesthouse_input_modifier(input, 0x3F, true);
        guesthouse_input_key(input, 0x6F, true);
        guesthouse_input_key(input, 0x6F, false);
        guesthouse_input_modifier(input, 0x37, false);
        // Held when the window lets go: Control, C and the right button.
        guesthouse_input_modifier(input, 0x3B, true);
        guesthouse_input_key(input, 0x08, true);
        guesthouse_input_button(input, 1, true);
        guesthouse_input_close(input);
        server.join().unwrap()
    };

    let expected = vec![
        json!({ "execute": "qmp_capabilities" }),
        send(abs(16384, 16384)),
        send(abs(328, 0)),
        send(json!([btn("left", true)])),
        send(json!([btn("left", false)])),
        send(json!([key("shift", true)])),
        send(json!([key("a", true)])),
        send(json!([key("a", false)])),
        send(json!([key("shift", false)])),
        send(json!([btn("wheel-up", true), btn("wheel-up", false)])),
        send(json!([btn("wheel-down", true), btn("wheel-down", false)])),
        send(json!([key("meta_l", true)])),
        send(json!([key("f12", true)])),
        send(json!([key("f12", false)])),
        send(json!([key("meta_l", false)])),
        send(json!([key("ctrl", true)])),
        send(json!([key("c", true)])),
        send(json!([btn("right", true)])),
        // The close.
        send(json!([key("c", false), key("ctrl", false), btn("right", false)])),
    ];
    assert_eq!(lines.len(), expected.len(), "got {lines:#?}");
    for (got, want) in lines.iter().zip(&expected) {
        assert_eq!(got, want);
    }
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn no_socket_means_input_is_dropped_without_waiting() {
    let path = CString::new("/nonexistent/guesthouse-qmp.sock").unwrap();
    // SAFETY: as above.
    unsafe {
        let input = guesthouse_input_open(path.as_ptr());
        assert!(!input.is_null());
        guesthouse_input_key(input, 0x00, true);
        guesthouse_input_close(input);
        assert!(guesthouse_input_open(std::ptr::null()).is_null());
    }
}
