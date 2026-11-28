// SPDX-License-Identifier: GPL-2.0-or-later
//! One process per VM on a macOS host: it owns the guest's graphics device.
//!
//! As on a Linux host, QEMU is the machine and moves bytes, and the device's other half is a
//! process of Guesthouse's. Here the device is macOS's own paravirtualized graphics device, from
//! `guesthouse-pvg-sys`, and the bytes are register reads and writes: QEMU's `pvg-fwd` device shows
//! the guest a PCI function, forwards what the guest does to its registers over a socket, and
//! raises the interrupts that come back. The guest's RAM is shared memory that both map.
//!
//! ```sh
//! guesthouse-pvg-host --socket vm.sock --control vm-control.sock
//! qemu-system-aarch64 -M virt,memory-backend=ram -object memory-backend-shm,id=ram,size=4G \
//!     -device pvg-fwd,socket=vm.sock ...
//! ```
//!
//! The process starts first and waits for QEMU. It serves one guest and ends when QEMU goes. The
//! control socket takes one line a connection: `frame PATH` writes the display's picture to a PPM
//! file, `modes 1920x1080@60,1280x800@60` gives the display a new mode list, `status` says what
//! has happened so far.
//!
//! `--frames NAME` publishes each picture the guest shows under the bootstrap name NAME, for up to
//! four processes on the Mac to read at once with no copy, as QEMU's `frame-export` does: the
//! Guesthouse app's screen (`guesthouse.<machine id>`) and remoteman's `rmhost`. It opens no
//! window; with `--window` too, both show the guest. Without `--window` the process has none, as
//! always.
//!
//! `guesthouse-pvg-host --option-rom` prints where the framework keeps the option ROM for the
//! device's PCI function, for QEMU's `romfile=`, and ends. `guesthouse-pvg-host --screen-scale`
//! prints the backing scale of the screen the window opens on (1, or 2 on a Retina screen): the
//! window's modes are its points times that, so a guest's desktop at that scale shows its things
//! at the size the Mac shows its own.
//!
//! - `wire`: the messages on QEMU's socket.
//! - `guest_ram`: the guest's memory, mapped from the files QEMU hands over.
//! - `host`: the device, and the loop that serves it.
//! - `picture`: a picture as a file, and a mode list as text.
//! - `window`: a window on the Mac that shows the guest's picture, with `--window`. The guest is
//!   then served from a second thread, as the window wants the first. The display's mode list
//!   follows the window's size, from its first, and the control socket takes `window WxH` to
//!   resize it, `fullscreen` to take it to full screen and back, and `close` for its close button.
//! - `input` and `keys`: with `--input SOCKET`, the window takes the keyboard and the pointer for
//!   the guest and sends them through QEMU's monitor socket SOCKET, and its close button asks the
//!   guest to power down. `--input-test` keeps the window from taking the keyboard and takes made-up
//!   events from the control socket's `input` instead, for runs with no person: the process is
//!   then never made the Mac's active app. `--power SOCKET` gives a window without input QEMU's
//!   monitor socket for its close button alone: in every mode the close presses the guest's power
//!   button, and the window stays until QEMU has gone.
//! - `export`: the guest's pictures for other processes, with `--frames`, from a thread of
//!   their own.
//!
//! Two environment words for the window: `PVG_WINDOW_SCREEN` names the screen it opens on,
//! centered: `builtin` (the Mac's own screen, else the main one), `main` (the one with the menu
//! bar, as when the word is unset) or an index of `NSScreen.screens`; `--screen-scale` says that
//! screen's scale. `PVG_WINDOW_FOCUS=no` keeps a window with `--input` from coming to the front
//! with the keyboard when it opens, as `--input-test` and a window without input always do: for a
//! run with no person, whose window is to leave the user's app in front.
//!
//! The program is a thin `main` over this library, and the library is there for a daemon that
//! takes the program's place: [`options`] reads the program's arguments, [`run`] serves one guest
//! as the program does (and [`serve`] with the window's ways of taking input), with the program's
//! lines on standard error and its control socket; [`USAGE`] and [`DEFAULT_MODES`] are its texts.

// Tests read their own sockets with plain `read_line`.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

#[cfg(target_os = "macos")]
mod export;
#[cfg(target_os = "macos")]
mod guest_ram;
#[cfg(target_os = "macos")]
mod host;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod keys;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod picture;
#[cfg(target_os = "macos")]
mod watch;
#[cfg(target_os = "macos")]
mod window;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod wire;

#[cfg(target_os = "macos")]
pub use host::Options;
pub use picture::{ModeText, read_modes};
#[cfg(target_os = "macos")]
pub use window::Taking;

/// The display's modes when the arguments name none.
#[cfg(target_os = "macos")]
pub const DEFAULT_MODES: &str = "1920x1080@60,1280x800@60,1024x768@60,640x480@60";
/// What the program says to arguments it can't take, after `pvg-host: `: what the Mac app
/// expects, byte for byte, as the app and the daemon's checks read it. The window's own flags,
/// `--input`, `--input-test`, `--power` and `--screen-scale`, are in this crate's notes at the top
/// of this file.
#[cfg(target_os = "macos")]
pub const USAGE: &str = "usage: guesthouse-pvg-host --socket PATH [--control PATH] [--modes WxH@R,...] [--name NAME] [--window] [--frames NAME]\n       guesthouse-pvg-host --option-rom";
/// A window's content at its start, in points.
#[cfg(target_os = "macos")]
const WINDOW_POINTS: (u32, u32) = (960, 540);

/// Reads the program's arguments, its name left out: the options to serve with, and the window's
/// way of taking input if it has one. A mistake is what the program says before it ends.
#[cfg(target_os = "macos")]
pub fn options(arguments: impl IntoIterator<Item = String>) -> Result<(Options, Option<Taking>), String> {
    let mut arguments = arguments.into_iter();
    let (mut socket, mut control, mut name) = (None, None, "Guesthouse".to_owned());
    let (mut window, mut input, mut test, mut power) = (false, None, false, None);
    let mut frames = None;
    let mut modes = picture::read_modes(DEFAULT_MODES)?;
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or_else(|| format!("{argument} wants a value\n{USAGE}"));
        match argument.as_str() {
            "--socket" => socket = Some(value()?.into()),
            "--control" => control = Some(value()?.into()),
            "--name" => name = value()?,
            "--modes" => modes = picture::read_modes(&value()?)?,
            "--window" => window = true,
            "--input" => input = Some(value()?.into()),
            "--input-test" => test = true,
            "--power" => power = Some(value()?.into()),
            "--frames" => frames = Some(value()?),
            _ => return Err(USAGE.to_owned()),
        }
    }
    if input.is_some() && power.is_some() {
        return Err(format!(
            "--power is for a window without --input, whose socket powers down too\n{USAGE}"
        ));
    }
    // Input and the close button are the window's, so they bring one.
    let taking = match (window || input.is_some() || power.is_some(), &input, test) {
        (false, _, _) => None,
        (true, None, false) => Some(Taking::Nothing),
        (true, Some(_), false) => Some(Taking::Focus),
        (true, Some(_), true) => Some(Taking::Test),
        (true, None, true) => return Err(format!("--input-test wants --input\n{USAGE}")),
    };
    let monitor = input.or(power);
    Ok((Options { socket: socket.ok_or(USAGE)?, control, name, modes, monitor, frames }, taking))
}

/// Serves one guest until QEMU goes, with no window or with one that takes no input, as the
/// program does with `--window`. The process's lines go to standard error, as the program's.
#[cfg(target_os = "macos")]
pub fn run(options: Options, with_window: bool) -> Result<(), String> {
    serve(options, with_window.then_some(Taking::Nothing))
}

/// Serves the guest, with a window or without. The window takes this thread, and the guest is
/// served from another until QEMU goes, which closes the window: also when the window's close
/// button asked the guest to power down. On a Mac a window wants the process's first thread.
#[cfg(target_os = "macos")]
pub fn serve(options: Options, taking: Option<Taking>) -> Result<(), String> {
    let Some(taking) = taking else {
        return host::run(&options, None).map_err(|error| error.to_string());
    };
    let monitor = options.monitor.is_some();
    let (window, handle) = window::open(&options.name, WINDOW_POINTS.0, WINDOW_POINTS.1, taking, monitor)?;
    let handle = std::sync::Arc::new(handle);
    let serving = std::thread::spawn(move || {
        let outcome = host::run(&options, Some(&handle)).map_err(|error| error.to_string());
        handle.stop();
        outcome
    });
    window.run();
    serving.join().unwrap_or_else(|_| Err("the thread that serves the guest ended badly".to_owned()))
}

/// The backing scale of the screen a window opens on: 1, or 2 on a Retina screen.
#[cfg(target_os = "macos")]
pub fn screen_scale() -> u32 {
    window::screen_scale().round().max(1.0) as u32
}
