//! Everything Guesthouse's desktop app does that needs no display.
//!
//! Each machine is one JSON file. Anything slow runs as a task: a process of its own that outlives
//! the app and writes each step to a small file, so the app can close at any moment and read where
//! things are when it opens again. [`context::Context`] holds what the code needs from the PC, and
//! tests swap those parts for stand-ins. How a VM starts is behind [`vm_host::VmHost`].

pub mod account;
pub mod actions;
pub mod agent;
pub mod bridge;
pub mod card_choice;
pub mod clock;
pub mod context;
pub mod daemon;
pub mod defaults;
pub mod disk_image;
pub mod display;
pub mod driver_modes;
pub mod failure;
pub mod firmware;
pub mod forwarded_input;
pub mod gpu_identity;
pub mod gpu_sample;
pub mod gpu_support;
pub mod guest_memory;
pub mod headless;
pub mod held_memory;
pub mod host;
pub mod host_drives;
pub mod host_sensors;
pub mod input;
pub mod iso;
pub mod jailing;
pub mod library;
pub mod limits;
pub mod linux;
pub mod log;
pub mod machine;
pub mod monitor;
pub mod network;
pub mod network_settings;
pub mod overlay;
pub mod paths;
pub mod process_stat;
pub mod processes;
pub mod random;
pub mod screen_gpu;
pub mod settings;
pub mod setup;
pub mod share;
pub mod slots;
pub mod snapshots;
pub mod start_checks;
pub mod stopped;
pub mod tasks;
#[doc(hidden)]
pub mod testing;
pub mod text;
pub mod updates;
pub mod usb;
pub mod vm_args;
pub mod vm_host;
pub mod zones;

pub use failure::{Failure, Result};
