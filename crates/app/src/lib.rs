//! The desktop app: a library of machines, their settings, first-run setup and the viewer's shell.
//!
//! Everything here is GTK. What the app does is in `guesthouse-app-core`, and anything slow goes to a
//! thread through [`common::in_background`] or to a task, so nothing can hold up the window.

pub mod application;
pub mod common;
pub mod driver_row;
pub mod log_dialog;
pub mod new_machine;
pub mod rows;
pub mod settings;
pub mod viewer;
pub mod window;
pub mod wizard;
