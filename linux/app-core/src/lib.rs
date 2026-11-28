//! Everything Guesthouse's desktop app does that needs no display.
//!
//! Each machine is one JSON file. Anything slow runs as a task: a process of its own that outlives
//! the app and writes each step to a small file, so the app can close at any moment and read where
//! things are when it opens again. [`context::Context`] holds what the code needs from the PC, and
//! tests swap those parts for stand-ins. How a VM starts is behind [`vm_host::VmHost`].

// Linux only: it starts QEMU and the daemon, and reads a Linux host. Other systems get an empty crate.
#![cfg(target_os = "linux")]
// Tests read their own sockets with plain `read_line`.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod account;
pub mod actions;
pub mod agent;
pub mod android_setup;
pub mod app_windows;
pub mod auto_snapshot;
pub mod bridge;
pub mod card_choice;
pub mod card_share;
pub mod carry;
pub mod clock;
pub mod context;
pub mod daemon;
pub mod defaults;
pub mod disk_image;
pub mod display;
pub mod downloads;
pub mod driver_modes;
pub mod duplicate;
pub mod ending;
pub mod export;
pub mod failure;
pub mod firmware;
pub mod forwarded_input;
pub mod gpu_identity;
pub mod gpu_maker;
pub mod gpu_sample;
pub mod gpu_support;
pub mod guest_driver_store;
pub mod guest_files;
pub mod guest_memory;
pub mod guest_shell;
pub mod headless;
pub mod held_memory;
pub mod host;
pub mod host_drives;
pub mod host_sensors;
pub mod idle;
pub mod import;
pub mod input;
pub mod intel_desktop;
pub mod iso;
pub mod jailing;
pub mod kept_logs;
pub mod lean;
pub mod library;
pub mod limits;
pub mod linux;
pub mod log;
pub mod machine;
pub mod macos_part;
pub mod macos_setup;
pub mod monitor;
pub mod network;
pub mod network_settings;
pub mod only_through;
pub mod overlay;
pub mod paths;
pub mod payload_install;
pub mod process_stat;
pub mod processes;
pub mod random;
pub mod recipe;
pub mod remote_access;
pub mod remote_view;
pub mod render_cards;
pub mod report;
pub mod run_files;
pub mod scan_exclusions;
pub mod screen_gpu;
pub mod settings;
pub mod setup;
pub mod share;
pub mod slots;
pub mod snapshot_state;
pub mod snapshots;
pub mod start_checks;
pub mod stopped;
pub mod tasks;
#[doc(hidden)]
pub mod testing;
pub mod text;
pub mod throwaway;
pub mod token_change;
pub mod updates;
pub mod usb;
pub mod vm_args;
pub mod vm_host;
pub mod writable_copy;
pub mod zones;

#[cfg(test)]
mod answers_golden;

pub use failure::{Failure, Result};
