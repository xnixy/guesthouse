//! A Windows host's half of Guesthouse: Windows' Host Compute System, which runs a VM with only the
//! Virtual Machine Platform feature on, so Windows 11 Home can run one too. Hyper-V and its tools
//! are never needed.
//!
//! `schema` is the machine document and the other documents HCS and its network take, as types, and
//! it builds on every system so its tests run anywhere, as does the rule for which adapters are
//! physical in `adapters`. The rest is Windows only:
//!
//! - `system`: computecore.dll, which creates, starts, changes and stops a machine.
//! - `network`: computenetwork.dll, the network a machine's card is plugged into.
//! - `adapters`: the host's network adapters, and which of them a machine can be bridged onto.
//! - `services`: whether a Windows service is installed and running.
//! - `disk`: virtdisk.dll, which makes, attaches and detaches a VHDX or an ISO, and the volumes on them.
//! - `gpu`: the host's cards that can be shared with a guest, and their drivers' folders.
//! - `drivers`: the copy of a card's driver a guest needs, put on its disk while the machine is off.
//! - `guest_volume`: a guest's volume while its disk is attached to the host, the only way anything
//!   on it is read, written or removed, which follows no links.
//! - `hvsock`: Hyper-V sockets, which reach a guest with no network. Its IDs build everywhere.
//! - `worker`: the account a machine's worker process runs as. It builds everywhere.
//! - `host`: whether this process is elevated, which HCS needs, and Ctrl+C.
//! - `dns_sd`: announcing a service through Windows' own mDNS responder. What to do after each of
//!   its answers builds everywhere.
//!
//! The JSON shapes, the order a machine gets its GPU in, and where a guest looks for its host's
//! driver were first worked out in App Sandbox, as `THIRD-PARTY.md` says.

// Anything on a guest's disk goes through `guest_volume`. Tests make their own folders.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod adapters;
pub mod dns_sd;
pub mod hvsock;
pub mod schema;

#[cfg(windows)]
pub mod disk;
#[cfg(windows)]
pub mod driver_files;
#[cfg(windows)]
pub mod drivers;
#[cfg(windows)]
mod error;
#[cfg(windows)]
pub mod gpu;
#[cfg(windows)]
pub mod guest_volume;
#[cfg(windows)]
pub mod host;
#[cfg(windows)]
pub mod network;
#[cfg(windows)]
pub mod serial;
#[cfg(windows)]
pub mod services;
#[cfg(windows)]
pub mod system;
#[cfg(windows)]
mod text;
pub mod worker;

#[cfg(windows)]
pub use error::{Error, Result};
