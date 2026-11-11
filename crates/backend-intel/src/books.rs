//! Memory and address spaces as this stage keeps them: names and books, with nothing on the card.
//!
//! Nothing runs on the GPU yet, so nothing ever writes video memory, and the core moves an
//! allocation into the guest's own pages the first time the guest's CPU locks it. So a block of
//! video memory is only a name here. The books still refuse what a driver would, as the trait asks:
//! a mapping over one already there, or outside the space's ranges.

use guesthouse_backend::{BackendError, GpuRange, MapRequest, MemoryId, SpaceId, UnmapRequest};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Default)]
struct Space {
    ranges: Vec<GpuRange>,
    /// Address to length, and the memory mapped there.
    mappings: BTreeMap<u64, (u64, MemoryId)>,
}

impl Space {
    fn overlaps(&self, address: u64, length: u64) -> bool {
        let before = self.mappings.range(..address.saturating_add(length)).next_back();
        before.is_some_and(|(&start, &(size, _))| start.saturating_add(size) > address)
    }
}

/// See the module documentation.
#[derive(Debug, Default)]
pub struct Books {
    next_id: u64,
    memory: HashSet<MemoryId>,
    spaces: HashMap<SpaceId, Space>,
}

impl Books {
    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn new_memory(&mut self) -> MemoryId {
        let memory = MemoryId(self.next());
        self.memory.insert(memory);
        memory
    }

    pub fn release(&mut self, memory: MemoryId) {
        self.memory.remove(&memory);
        // A mapping of freed memory would be a dangling one on a real card, so it goes too.
        for space in self.spaces.values_mut() {
            space.mappings.retain(|_, (_, mapped)| *mapped != memory);
        }
    }

    pub fn create_space(&mut self, ranges: &[GpuRange]) -> SpaceId {
        let space = SpaceId(self.next());
        self.spaces.insert(space, Space { ranges: ranges.to_vec(), mappings: BTreeMap::new() });
        space
    }

    pub fn destroy_space(&mut self, space: SpaceId) {
        self.spaces.remove(&space);
    }

    pub fn map(&mut self, request: &MapRequest) -> Result<(), BackendError> {
        if !self.memory.contains(&request.memory) {
            return Err(BackendError::new("mapping memory that does not exist"));
        }
        let Some(space) = self.spaces.get_mut(&request.space) else {
            return Err(BackendError::new("this address space is closed"));
        };
        if !space.ranges.iter().any(|range| range.contains(request.address)) {
            return Err(BackendError::new("no range holds this address"));
        }
        if space.overlaps(request.address, request.length) {
            return Err(BackendError::new("these addresses are taken"));
        }
        space.mappings.insert(request.address, (request.length, request.memory));
        Ok(())
    }

    pub fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError> {
        if !self.memory.contains(&unmap.memory) {
            return Err(BackendError::new("unmapping memory that does not exist"));
        }
        if let Some(space) = self.spaces.get_mut(&unmap.space) {
            space.mappings.remove(&unmap.address);
        }
        Ok(())
    }

    pub fn live_memory(&self) -> usize {
        self.memory.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::MapMemoryType;

    fn map(space: SpaceId, memory: MemoryId, address: u64, length: u64) -> MapRequest {
        MapRequest {
            space,
            memory,
            offset: 0,
            length,
            address,
            small_pages: false,
            gpu_cached: false,
            readable: true,
            writable: true,
            executable: false,
            memory_type: MapMemoryType::Default,
        }
    }

    #[test]
    fn taken_addresses_and_addresses_outside_the_ranges_are_refused() {
        let mut books = Books::default();
        let space = books.create_space(&[GpuRange { start: 0x1000, end: 0x10000 }]);
        let memory = books.new_memory();
        books.map(&map(space, memory, 0x2000, 0x1000)).unwrap();
        assert!(books.map(&map(space, memory, 0x2800, 0x1000)).is_err());
        assert!(books.map(&map(space, memory, 0x10000, 0x10)).is_err());
        assert!(books.map(&map(space, memory, 0x3000, 0x1000)).is_ok());
        let unmap = UnmapRequest { space, memory, address: 0x2000, length: 0x1000 };
        books.unmap(&unmap).unwrap();
        assert!(books.map(&map(space, memory, 0x2000, 0x1000)).is_ok(), "free again once unmapped");
    }

    #[test]
    fn freeing_memory_frees_the_addresses_it_was_mapped_at() {
        let mut books = Books::default();
        let space = books.create_space(&[GpuRange { start: 0, end: 0x10000 }]);
        let (first, second) = (books.new_memory(), books.new_memory());
        books.map(&map(space, first, 0x1000, 0x1000)).unwrap();
        books.release(first);
        assert!(books.map(&map(space, second, 0x1000, 0x1000)).is_ok());
    }

    #[test]
    fn memory_that_has_gone_can_not_be_mapped() {
        let mut books = Books::default();
        let space = books.create_space(&[GpuRange { start: 0, end: 0x10000 }]);
        let memory = books.new_memory();
        books.release(memory);
        assert_eq!(books.live_memory(), 0);
        assert!(books.map(&map(space, memory, 0x1000, 0x1000)).is_err());
        books.destroy_space(space);
        let other = books.new_memory();
        assert!(books.map(&map(space, other, 0x1000, 0x1000)).is_err(), "a closed space");
    }
}
