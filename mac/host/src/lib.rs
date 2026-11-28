// SPDX-License-Identifier: GPL-2.0-or-later
//! The Rust core of Guesthouse's Mac app. Swift reaches it through the C functions below,
//! declared in `include/guesthouse_core.h`. Answers cross as JSON text, so the interface stays a
//! handful of functions while what they carry grows.
#![cfg(target_os = "macos")]
// Tests read their own sockets with plain `read_line`.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod agent;
pub mod agent_api;
pub mod agent_build;
pub mod control;
pub mod create;
pub mod driver_version;
pub mod input;
pub mod install;
pub mod keyboard_layout;
pub mod keys;
pub mod lab;
pub mod launch;
pub mod link;
pub mod machines_ffi;
pub mod probe;
pub mod process;
pub mod protocol;
pub mod remote;
pub mod report;
pub mod report_ffi;
pub mod rules;
pub mod screen_size;
pub mod server;
pub mod start_checks;
pub mod state;
pub mod store;
pub mod tools;
pub mod updates;

use input::{Button, Fit, Input};
use link::Link;
use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;

/// The core's version, from Cargo.toml, with the C string's ending nul.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// The core's version as a C string. It is static: the caller must not free it.
#[unsafe(no_mangle)]
pub extern "C" fn guesthouse_core_version() -> *const c_char {
    VERSION.as_ptr().cast()
}

/// The default machines folder, which the caller frees with `guesthouse_string_free`, or null
/// when there is none (no HOME, or a path that is not UTF-8).
#[unsafe(no_mangle)]
pub extern "C" fn guesthouse_machines_folder() -> *mut c_char {
    match store::default_folder().and_then(|path| path.to_str().map(String::from)) {
        // A path from the environment cannot hold a nul, so this never fails.
        Some(path) => CString::new(path).map_or(std::ptr::null_mut(), CString::into_raw),
        None => std::ptr::null_mut(),
    }
}

/// Lists the machines in `folder`, or in the default machines folder when `folder` is null.
/// Returns JSON, either `{"machines": [...], "skipped": [...]}` or `{"error": "..."}`, which
/// the caller frees with `guesthouse_string_free`.
///
/// # Safety
/// `folder` is null or points to a nul-terminated UTF-8 path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_machine_list(folder: *const c_char) -> *mut c_char {
    let folder = if folder.is_null() {
        store::default_folder()
    } else {
        // SAFETY: the caller promises a nul-terminated string.
        let text = unsafe { CStr::from_ptr(folder) };
        text.to_str().ok().map(PathBuf::from)
    };
    let answer = match folder {
        None => error_json("no machines folder: HOME is not set or the path is not UTF-8"),
        Some(folder) => match store::list(&folder) {
            Ok(listing) => serde_json::to_string(&listing).expect("a listing always serialises"),
            Err(error) => error_json(&format!("cannot list {}: {error}", folder.display())),
        },
    };
    into_c_string(answer)
}

/// Frees a string the core returned. Null is allowed and does nothing.
///
/// # Safety
/// `text` is null or a pointer the core returned that has not been freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_string_free(text: *mut c_char) {
    if !text.is_null() {
        // SAFETY: the pointer came from `CString::into_raw` in `into_c_string`.
        drop(unsafe { CString::from_raw(text) });
    }
}

/// A machine's input: what the guest has been told is held, and the QMP socket it goes to.
/// Opaque to C. Each call sends at most one command and never waits on QEMU.
pub struct GuesthouseInput {
    input: Input,
    link: Link,
}

impl GuesthouseInput {
    fn send(&self, events: Vec<serde_json::Value>) {
        if !events.is_empty() {
            self.link.send(input::command(&events));
        }
    }
}

/// Starts sending input to the QMP socket at `path`. Returns null when `path` is null or not
/// UTF-8. The socket need not exist yet: it is tried every second.
///
/// # Safety
/// `path` is null or points to a nul-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_open(path: *const c_char) -> *mut GuesthouseInput {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller promises a nul-terminated string.
    let Ok(path) = unsafe { CStr::from_ptr(path) }.to_str() else {
        return std::ptr::null_mut();
    };
    let handle = GuesthouseInput { input: Input::default(), link: Link::open(PathBuf::from(path)) };
    Box::into_raw(Box::new(handle))
}

/// Runs `body` on the handle, if there is one.
///
/// # Safety
/// `handle` is null or came from `guesthouse_input_open` and is not closed, and no other call
/// uses it at the same time.
unsafe fn with_input(handle: *mut GuesthouseInput, body: impl FnOnce(&mut Input) -> Vec<serde_json::Value>) {
    // SAFETY: as the caller promises.
    if let Some(handle) = unsafe { handle.as_mut() } {
        let events = body(&mut handle.input);
        handle.send(events);
    }
}

/// The pointer is at (`x`, `y`) in a view of `view_width` by `view_height`, from its top left,
/// showing a picture of `picture_width` by `picture_height` fitted inside it.
///
/// # Safety
/// As for every `guesthouse_input_` call: `handle` is null or open, and used by one thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_pointer(
    handle: *mut GuesthouseInput,
    x: f64,
    y: f64,
    view_width: f64,
    view_height: f64,
    picture_width: f64,
    picture_height: f64,
) {
    let fit = Fit { view_width, view_height, picture_width, picture_height };
    // SAFETY: passed on from the caller.
    unsafe { with_input(handle, |input| input.pointer(fit, x, y)) }
}

/// Button `number` (0 left, 1 right, 2 middle) went down or up. Others are ignored.
///
/// # Safety
/// See `guesthouse_input_pointer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_button(handle: *mut GuesthouseInput, number: u32, down: bool) {
    let Some(button) = Button::from_number(number) else {
        return;
    };
    // SAFETY: passed on from the caller.
    unsafe { with_input(handle, |input| input.button(button, down)) }
}

/// A scroll, in points when `precise` and in lines when not.
///
/// # Safety
/// See `guesthouse_input_pointer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_scroll(
    handle: *mut GuesthouseInput,
    dx: f64,
    dy: f64,
    precise: bool,
) {
    // SAFETY: passed on from the caller.
    unsafe { with_input(handle, |input| input.scroll(dx, dy, precise)) }
}

/// A key with macOS key code `code` went down or up.
///
/// # Safety
/// See `guesthouse_input_pointer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_key(handle: *mut GuesthouseInput, code: u16, down: bool) {
    // SAFETY: passed on from the caller.
    unsafe { with_input(handle, |input| input.key(code, down)) }
}

/// The modifier key `code` changed, and its kind of modifier is now on or off.
///
/// # Safety
/// See `guesthouse_input_pointer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_modifier(handle: *mut GuesthouseInput, code: u16, flag_on: bool) {
    // SAFETY: passed on from the caller.
    unsafe { with_input(handle, |input| input.modifier(code, flag_on)) }
}

/// Lets go of every key and button the guest was told is held.
///
/// # Safety
/// See `guesthouse_input_pointer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_release_all(handle: *mut GuesthouseInput) {
    // SAFETY: passed on from the caller.
    unsafe { with_input(handle, Input::release_all) }
}

/// Lets go of everything held, then closes the socket once that is written. Null is allowed.
///
/// # Safety
/// `handle` is null or open; it is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn guesthouse_input_close(handle: *mut GuesthouseInput) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the pointer came from `Box::into_raw` in `guesthouse_input_open`.
    let mut handle = unsafe { Box::from_raw(handle) };
    let events = handle.input.release_all();
    handle.send(events);
}

pub(crate) fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

pub(crate) fn into_c_string(text: String) -> *mut c_char {
    // JSON escapes control characters, so serde_json's output never holds a nul.
    CString::new(text).expect("JSON has no nul").into_raw()
}
