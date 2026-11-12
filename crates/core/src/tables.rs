//! Allocations, queues and the other per-guest records.

use guesthouse_backend::{MemoryId, MemoryKind, PagesView, SpaceId, SurfaceShape};
use std::collections::VecDeque;

/// Where an allocation is mapped in one address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub address: u64,
    /// Where in the allocation the mapping starts.
    pub first: u64,
    pub length: u64,
    pub space: SpaceId,
    /// The driver's own word for how it wants the pages, as the backend was given it. Zero if none.
    pub driver_protection: u64,
}

impl Mapping {
    pub fn contains(&self, address: u64) -> bool {
        (self.address..self.address + self.length).contains(&address)
    }

    pub fn overlaps(&self, address: u64, size: u64) -> bool {
        self.address < address + size && address < self.address + self.length
    }
}

/// An allocation, from creation until the guest destroys it.
#[derive(Debug, Default)]
pub struct Allocation {
    pub size: u64,
    /// The process that made it.
    pub owner: u32,
    /// What backs it. A guest-backed allocation has none until the guest has listed its pages.
    pub memory: Option<MemoryId>,
    /// Where it sits in the shared window, once the guest has locked it.
    pub guest_address: Option<u64>,
    /// The page list so far, for a guest-backed allocation still being described.
    pub pending_pages: Option<Vec<Option<u64>>>,
    /// The guest's own pages as mapped here.
    pub view: Option<PagesView>,
    pub resource: u32,
    pub mappings: Vec<Mapping>,
    /// When it was first mapped, counting mapped allocations.
    pub first_mapped: Option<u64>,
    pub gpu_cached: bool,
    /// The guest made it to be shown, as DWM's buffers and a fullscreen program's are. Kept as the
    /// guest asked, whatever the answer said.
    pub primary: bool,
    pub surface: Option<SurfaceShape>,
    /// How its video memory is laid out, so memory made again for it is the same.
    pub kind: MemoryKind,
    /// The residency priority the guest set, if it has. Nothing here acts on it: the host's
    /// driver decides what stays in video memory.
    pub priority: Option<u32>,
}

impl Allocation {
    /// Whether system memory is behind it: the guest's own pages, or the shared pages a lock moved
    /// it to.
    pub fn in_system_memory(&self) -> bool {
        self.view.is_some() || self.guest_address.is_some()
    }
}

/// A hardware queue, or the queue the host makes for a context that has none.
#[derive(Debug, Clone)]
pub struct Queue {
    /// Its progress fence, by guest physical address.
    pub fence: u64,
    pub fence_gpu_address: u64,
    pub space: SpaceId,
    pub context: u32,
    /// The fence value of the last work the GPU took, if any.
    pub last_fence: Option<u64>,
    /// The GPU stopped answering on this queue, so nothing waits for it any more.
    pub failed: bool,
    /// What the GPU has yet to finish, as (fence value, work), oldest first.
    pub backlog: VecDeque<(u64, u32)>,
    /// All the work in `backlog`.
    pub unfinished: u64,
}

impl Queue {
    pub fn new(fence: u64, fence_gpu_address: u64, space: SpaceId, context: u32) -> Self {
        Self {
            fence,
            fence_gpu_address,
            space,
            context,
            last_fence: None,
            failed: false,
            backlog: VecDeque::new(),
            unfinished: 0,
        }
    }
}

/// A range the guest set aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservedRange {
    pub address: u64,
    pub size: u64,
    pub space: SpaceId,
    /// At an address the guest asked for and not one handed out, so it goes back differently.
    pub placed: bool,
}
