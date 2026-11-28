//! Guest machines on a Windows host: making a machine's disks from a Windows or Ubuntu ISO, and
//! running it through HCS with the host's card by GPU-PV. The work is the same whoever asks for it,
//! `guesthouse-hcs-boot` on the command line or the Guesthouse service for the app, so it says what
//! it's doing through an [`events::Sink`] and is told what to do by [`settings`].
//!
//! What a machine is and what goes on its disks builds on every system, so its tests run anywhere.
//! [`windows`], the work itself, is Windows only.

// Anything on a guest's disk goes through `guest_volume`. Tests make their own folders.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod announce;
pub mod events;
pub mod guest;
pub mod guest_ctl;
pub mod hyper_v;
pub mod machine;
pub mod network;
pub mod printable;
pub mod record;
pub mod screen;
pub mod settings;
pub mod snapshots;
pub mod ssh;
pub mod wim_info;
#[cfg(windows)]
pub mod windows;
