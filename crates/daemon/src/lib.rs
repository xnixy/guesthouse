//! The daemon: one process per VM.
//!
//! A slot says which VM this is, and everything the process touches is named from it, so two
//! daemons can never share a file. [`server::Server`] accepts QEMU's two channels, and one thread
//! runs every message through [`router::Router`] to the session. [`viewer::ViewerPort`] serves the
//! frame socket from that same thread. The guest's memory comes from the app as descriptors: see
//! [`handover`]. For a QEMU of someone else's it comes as two files instead: see [`memory_files`].

#[cfg(feature = "amd")]
pub mod amd_setup;
pub mod card_choice;
pub mod display_note;
pub mod environment;
pub mod gpu_note;
pub mod guest_memory;
pub mod handover;
pub mod jail;
pub mod logger;
pub mod memory_files;
pub mod router;
pub mod server;
#[cfg(feature = "nvidia")]
pub mod setup;
pub mod slot;
pub mod startup;
#[cfg(any(feature = "nvidia", feature = "amd"))]
mod store;
pub mod viewer;
