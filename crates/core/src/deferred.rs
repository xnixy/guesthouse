//! Work that must wait until the GPU has finished what it already has.
//!
//! A process can free memory straight after submitting work that uses it. The guest's memory manager
//! would wait on real hardware, so the host must too: unmapping it now would make the GPU fault.

use crate::fences::Fence;
use crate::tables::{Mapping, ReservedRange};
use guesthouse_backend::{MemoryId, PagesView, SpaceId};

/// Something to do later.
#[derive(Debug)]
pub enum DeferredAction {
    ReturnFence(Fence),
    FreeAllocation(FreedAllocation),
    CloseSpace(SpaceId),
    GiveBackReserved(ReservedRange),
}

/// What is left of a destroyed allocation.
#[derive(Debug)]
pub struct FreedAllocation {
    pub memory: MemoryId,
    pub mappings: Vec<Mapping>,
    pub view: Option<PagesView>,
    /// Its place in the shared window, and its size.
    pub shared: Option<(u64, u64)>,
}

impl FreedAllocation {
    /// As [`crate::tables::Allocation::in_system_memory`].
    pub fn in_system_memory(&self) -> bool {
        self.view.is_some() || self.shared.is_some()
    }
}

/// An action, and the queue positions it waits for.
#[derive(Debug)]
pub struct Deferred {
    pub waits: Vec<(u32, u64)>,
    pub action: DeferredAction,
}
