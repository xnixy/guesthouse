//! The Intel backend: the host's open xe driver, and the private data Intel's Windows driver passes
//! to its kernel driver.
//!
//! This is a first stage. It reads the card through xe's read-only queries and sysfs, and answers
//! the guest's driver well enough for it to load, but nothing runs on the card yet.

pub mod card;
pub mod generation;
pub mod hwconfig;
pub mod ioctl;
pub mod protocol;
pub mod sysfs;
pub mod xe_query;
