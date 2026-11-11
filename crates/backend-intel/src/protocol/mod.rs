//! The guest-protocol side: the private data Intel's Windows driver sends through WDDM, and what
//! its kernel driver would answer.
//!
//! Each module reads what the guest sends, checks it the way the kernel driver does, and builds
//! what the guest is owed, over plain byte slices, so all of it is tested without a card or a
//! guest. The layouts are those of one release of Intel's Windows driver. Where Intel's open
//! sources name a structure or field, that name is used; the rest are named for what they do, or
//! by where they are when nothing is known of them yet.

pub mod allocation;
pub mod bytes;
pub mod command_header;
pub mod context;
pub mod escape;
pub mod features;
pub mod gaps;
pub mod guest_driver;
pub mod hw_queue;
pub mod partition;
pub mod record;
