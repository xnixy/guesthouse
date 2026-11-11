//! Profiles: what changes between driver branches and between GPU generations.
//!
//! A host branch holds the sizes of the RM's structures on this side. A guest branch holds the
//! layouts of what the guest's driver reads and writes. A generation holds what the architecture
//! changes. They stay apart, so adding one never means a row for each of the others.

mod generation;
mod guest;
mod host;

pub use generation::{
    ADA, AMPERE, BLACKWELL, EngineNodes, Generation, TURING, generation_for, guest_channel_classes,
};
pub use guest::{AllocationLayout, ChannelObject, GUEST_617, GuestBranch, RecordLayout, guest_branch};
pub use host::{
    BRANCHES, GuestControl, HOST_615, HOST_620, HostBranch, HostLayout, host_branch, host_branch_of_version,
};

/// Pairs of (host branch, guest branch) that have been run together.
pub const TESTED_MIXES: &[(&str, &str)] = &[("615", "617"), ("620", "617")];

pub fn mix_tested(host: &str, guest: &str) -> bool {
    TESTED_MIXES.contains(&(host, guest))
}
