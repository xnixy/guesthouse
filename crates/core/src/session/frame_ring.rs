//! The frame ring in the session: the memory behind it, the copies into it, and what's offered to a
//! sink. The state itself is in `frames/ring.rs`.
//!
//! A copy into the ring reads the guest's surface, so it runs before the guest gets its reply. That
//! is the order the prototype was proven in. It costs the guest one more copy on the GPU, and never
//! a wait on the viewer: with no free slot, the frame is skipped.

use super::copy_bounds::picture_fits;
use super::{Session, services};
use crate::frames::{Delivery, Frame, FrameCounts, FrameRing, FrameSink, Picture, Slots, ViewerFacts};
use crate::frames::{LiveRing, RING_SLOTS, SlotInFlight, SlotMemory};
use crate::gpu_addresses::{ALLOCATION_ALIGN, AddressUse};
use guesthouse_backend::{
    Backend, BackendError, GuestMemory, MapRequest, MemoryId, MemoryKind, SlotCopy, SpaceId, StagingCopy,
    StagingStart, Status, SurfaceShape, UnmapRequest,
};
use guesthouse_messages::indirect::Look;
use guesthouse_messages::wire::align_up;
use std::os::fd::{AsFd, OwnedFd};
use std::time::Instant;

/// Slots are whole units of this, as the rest of video memory is.
const SLOT_GRAIN: u64 = 64 << 10;

/// Where a copy into the ring reads from.
#[derive(Debug, Clone, Copy)]
struct Source {
    surface: SurfaceShape,
    address: u64,
    /// The bytes of the allocation behind it.
    size: u64,
}

impl<B: Backend, M: GuestMemory> Session<B, M> {
    /// Someone has said hello. A ring follows as soon as there's a screen to size it by.
    pub fn viewer_arrived(&mut self) {
        self.frames.viewer = true;
        self.frames.viewer_facts = ViewerFacts::default();
        self.frames.viewer_wants_cursor = false;
        self.frames.refused = None;
    }

    /// The viewer will draw the cursor, which the guest then leaves out of the frames.
    pub fn viewer_wants_cursor(&mut self) {
        self.frames.viewer_wants_cursor = true;
        self.frames.cursor.send_again();
    }

    /// What the viewer that has just arrived can do.
    pub fn viewer_described(&mut self, facts: ViewerFacts) {
        self.frames.viewer_facts = facts;
    }

    /// The viewer has gone. Its buffers go too, and the next one gets its own, so that a slot it might
    /// still have on show is never written again.
    pub fn viewer_left(&mut self) {
        self.frames.viewer = false;
        self.frames.viewer_facts = ViewerFacts::default();
        self.frames.viewer_wants_cursor = false;
        if self.frames.indirect.is_some() {
            self.drop_memory_slots();
            self.keep_ring_for_next_viewer();
        } else {
            self.frames.retire_ring();
        }
        self.frames.behind = false;
        self.close_retired_rings();
        log::info!("the viewer has gone: {:?}", self.frames.counts);
    }

    /// The guest's memory stays, so the same slots go to the next viewer under a new generation, and
    /// a release from this one can't touch them.
    fn keep_ring_for_next_viewer(&mut self) {
        self.frames.generation = self.frames.generation.wrapping_add(1);
        let generation = self.frames.generation;
        if let (Some(indirect), Some(ring)) = (self.frames.indirect.as_mut(), self.frames.ring.as_mut()) {
            ring.generation = generation;
            ring.unsent = None;
            ring.slots.viewer_gone();
            indirect.needs_export = true;
            indirect.export_refused = false;
        }
    }

    /// The viewer is done with a slot. Anything but a slot it has changes nothing: a release from an
    /// older ring is normal, as a viewer can't know a new ring is on its way.
    pub fn slot_released(&mut self, generation: u32, slot: u32) {
        let Some(ring) = self.frames.ring.as_mut().filter(|ring| ring.generation == generation) else {
            log::debug!("a release of slot {slot} from ring {generation}, which has gone");
            return;
        };
        if ring.slots.released(slot).is_err() {
            log::warn!("the viewer released slot {slot}, which it doesn't have");
        }
    }

    /// Offers what's waiting to a sink. The main loop calls this, as a sink may touch a socket.
    pub fn offer_frames(&mut self, sink: &mut dyn FrameSink) {
        if self.deliver_ring(sink) == Delivery::Sent {
            let gone = self.deliver_frame(sink) == Delivery::Gone;
            if gone {
                self.viewer_left();
            }
        }
        if self.frames.viewer_wants_cursor && self.deliver_cursor(sink) == Delivery::Gone {
            self.viewer_left();
        }
    }

    /// The shape first, as a position means nothing without it.
    fn deliver_cursor(&mut self, sink: &mut dyn FrameSink) -> Delivery {
        let cursor = &mut self.frames.cursor;
        if cursor.shape_unsent
            && let Some(shape) = &cursor.shape
        {
            let delivery = sink.cursor_shape(shape);
            if delivery != Delivery::Sent {
                return delivery;
            }
            cursor.shape_unsent = false;
        }
        if cursor.position_unsent
            && let Some(position) = &cursor.position
        {
            let delivery = sink.cursor_moved(position);
            if delivery != Delivery::Sent {
                return delivery;
            }
            cursor.position_unsent = false;
        }
        Delivery::Sent
    }

    /// Offers the ring if it hasn't gone yet. [`Delivery::Sent`] means frames can follow.
    fn deliver_ring(&mut self, sink: &mut dyn FrameSink) -> Delivery {
        if self.frames.indirect.as_ref().is_some_and(|ring| ring.needs_export) {
            return Delivery::NotNow;
        }
        let Some(ring) = self.frames.ring.as_mut() else { return Delivery::NotNow };
        let Some(buffers) = &ring.unsent else { return Delivery::Sent };
        let shared =
            FrameRing { generation: ring.generation, slots: ring.slots.count(), size: ring.slot_size };
        let borrowed: Vec<_> = buffers.iter().map(AsFd::as_fd).collect();
        let delivery = sink.ring_opened(&shared, &borrowed);
        match delivery {
            Delivery::Sent => {
                // The viewer has its own descriptors now. The daemon has no use for these.
                ring.unsent = None;
                log::info!("ring {}: {} slots of {:#x} bytes", shared.generation, shared.slots, shared.size);
            }
            Delivery::NotNow => {}
            Delivery::Gone => self.viewer_left(),
        }
        delivery
    }

    fn deliver_frame(&mut self, sink: &mut dyn FrameSink) -> Delivery {
        let can_show = self.frames.ring.as_ref().is_some_and(|ring| self.frames.viewer_can_show(ring.look));
        if !can_show {
            self.frames.complain("the viewer can't show this ring's format, so nothing is reaching it");
            return Delivery::NotNow;
        }
        let Some(ring) = self.frames.ring.as_mut() else { return Delivery::NotNow };
        let Some((slot, sequence)) = ring.slots.pending() else { return Delivery::NotNow };
        let light = self.frames.indirect.as_ref().and_then(|indirect| indirect.light[slot as usize]);
        let (generation, picture, look) = (ring.generation, ring.picture, ring.look);
        let frame = Frame { generation, slot, sequence, picture, look, light };
        let delivery = sink.frame_ready(&frame);
        if delivery == Delivery::Sent {
            ring.slots.sent(slot);
            self.frames.counts.sent += 1;
        }
        delivery
    }

    pub fn frame_counts(&self) -> FrameCounts {
        self.frames.counts
    }

    /// Everything that happens to the ring away from a message: making it, closing old ones, and
    /// catching up. It's all on this thread, but none of it holds a guest's reply back.
    pub(super) fn service_frames(&mut self, now: Instant) {
        self.close_retired_rings();
        self.service_indirect(now);
        if let Some(picture) = self.frames.wanted_ring() {
            match self.open_ring(picture) {
                Ok(ring) => {
                    self.frames.ring = Some(ring);
                    self.frames.behind = true;
                    self.frames.complained = false;
                }
                Err(error) => {
                    log::warn!("no frame ring for {picture:?}: {error}. The guest carries on unseen.");
                    self.frames.refused = Some(picture);
                }
            }
        }
        if self.copies.is_empty() && self.frames.should_catch_up(now) {
            self.catch_up();
        }
    }

    fn open_ring(&mut self, picture: Picture) -> Result<LiveRing, BackendError> {
        let space = self.own_space()?;
        let size = align_up(picture.bytes(), SLOT_GRAIN);
        let mut memory = Vec::new();
        let mut buffers = Vec::new();
        for _ in 0..RING_SLOTS {
            match self.make_slot(size, space) {
                Ok((slot, buffer)) => {
                    memory.push(slot);
                    buffers.push(buffer);
                }
                Err(error) => {
                    memory.iter().for_each(|slot| self.free_slot(slot, space));
                    return Err(error);
                }
            }
        }
        self.frames.generation = self.frames.generation.wrapping_add(1);
        let generation = self.frames.generation;
        let slots = Slots::new(RING_SLOTS);
        let look = Look::SDR;
        Ok(LiveRing {
            generation,
            picture,
            look,
            slot_size: size,
            space,
            memory,
            slots,
            unsent: Some(buffers),
        })
    }

    /// Makes one slot: memory the GPU can reach, and the file a viewer reads it by. It's video memory,
    /// unless the viewer's screen is on another GPU, which can't take the card's buffers. Then it's
    /// memory here, which the viewer copies from.
    fn make_slot(&mut self, size: u64, space: SpaceId) -> Result<(SlotMemory, OwnedFd), BackendError> {
        if self.frames.viewer_facts.wants_memory {
            return self.make_host_slot(size, space);
        }
        let (memory, given) = self.backend.allocate_video_memory(size, MemoryKind::PLAIN)?;
        if given < size {
            self.backend.release_memory(memory);
            return Err(BackendError::new("the card gave a slot less memory than the picture needs"));
        }
        let slot = self.place_slot(memory, given, None, space)?;
        match self.backend.export_memory(memory, given) {
            Ok(buffer) => Ok((slot, buffer)),
            Err(error) => {
                self.free_slot(&slot, space);
                Err(error)
            }
        }
    }

    pub(super) fn make_host_slot(
        &mut self,
        size: u64,
        space: SpaceId,
    ) -> Result<(SlotMemory, OwnedFd), BackendError> {
        let buffer = self.memory.host_buffer(size).map_err(|error| BackendError::new(error.to_string()))?;
        let memory = match self.backend.adopt_host_pages(buffer.host_address, size) {
            Ok(memory) => memory,
            Err(error) => {
                self.memory.release_host_buffer(buffer.host_address, size);
                return Err(error);
            }
        };
        let slot = self.place_slot(memory, size, Some(buffer.host_address), space)?;
        Ok((slot, buffer.file))
    }

    /// Maps a slot's memory into the session's own space. On failure the memory is released.
    fn place_slot(
        &mut self,
        memory: MemoryId,
        given: u64,
        host_address: Option<u64>,
        space: SpaceId,
    ) -> Result<SlotMemory, BackendError> {
        let release = |session: &mut Self| {
            session.backend.release_memory(memory);
            if let Some(at) = host_address {
                session.memory.release_host_buffer(at, given);
            }
        };
        let whole = align_up(given, ALLOCATION_ALIGN);
        let address = match self.gpu_addresses.take(whole, ALLOCATION_ALIGN, AddressUse::FrameRing) {
            Ok(address) => address,
            Err(error) => {
                release(self);
                return Err(error);
            }
        };
        let map = MapRequest {
            space,
            memory,
            offset: 0,
            length: given,
            address,
            small_pages: true,
            gpu_cached: false,
            readable: true,
            writable: true,
            executable: false,
            memory_type: guesthouse_backend::MapMemoryType::Default,
            driver_protection: 0,
        };
        if let Err(error) = self.backend.map(&map) {
            release(self);
            self.gpu_addresses.give_back(address, whole, ALLOCATION_ALIGN, AddressUse::FrameRing);
            return Err(error);
        }
        Ok(SlotMemory { memory, size: given, address, host_address })
    }

    /// Video memory stays while a viewer still holds its file, so this can't pull a picture from
    /// under it.
    pub(super) fn free_slot(&mut self, slot: &SlotMemory, space: SpaceId) {
        let unmap = UnmapRequest { space, memory: slot.memory, address: slot.address, length: slot.size };
        let whole = align_up(slot.size, ALLOCATION_ALIGN);
        match self.backend.unmap(&unmap) {
            Ok(()) => {
                self.gpu_addresses.give_back(slot.address, whole, ALLOCATION_ALIGN, AddressUse::FrameRing)
            }
            Err(error) => log::warn!("unmapping a slot: {error}"),
        }
        self.backend.release_memory(slot.memory);
        if let Some(at) = slot.host_address {
            self.memory.release_host_buffer(at, slot.size);
        }
    }

    /// Closes the rings nothing is writing. The GPU must have finished with a slot before it goes,
    /// as a copy into unmapped memory would fault.
    pub(super) fn close_retired_rings(&mut self) {
        let mut index = 0;
        while index < self.frames.retired.len() {
            if self.frames.retired[index].slots.is_writing() {
                index += 1;
                continue;
            }
            let ring = self.frames.retired.swap_remove(index);
            ring.memory.iter().for_each(|slot| self.free_slot(slot, ring.space));
        }
    }

    /// The surface a frame's whole picture is in. Each slot needs all of it, as the last one holds a
    /// different frame, and a copy to the screen may have been for a part. An image of the screen's
    /// size has it all and is in video memory. Anything else goes by the staging surface.
    fn source_of(&self, copy: &StagingCopy, handles: [u32; 2], picture: &Picture) -> Option<Source> {
        let size = |handle| self.allocations.get(&handle).map_or(0, |allocation| allocation.size);
        let holds = |surface: &SurfaceShape, size: u64| {
            picture_fits(&self.backend, picture, (surface, size), picture.bytes())
        };
        let (image, staging) = (copy.source, copy.target);
        if (image.width, image.height) == (staging.width, staging.height) && holds(&image, size(handles[0])) {
            return Some(Source { surface: image, address: copy.source_address, size: size(handles[0]) });
        }
        log::debug!("the ring reads the staging surface, as the image is {image:?}");
        holds(&staging, size(handles[1])).then_some(Source {
            surface: staging,
            address: copy.target_address,
            size: size(handles[1]),
        })
    }

    /// A frame has reached the staging surface. Starts its copy into the ring, if there's a place for
    /// it. `Some` means that copy is under way and will need [`Session::ring_copy_ended`].
    pub(super) fn begin_ring_copy(&mut self, copy: &StagingCopy, handles: [u32; 2]) -> Option<SlotInFlight> {
        if !copy.is_screen || self.frames.indirect.is_some() {
            return None;
        }
        self.frames.screen_copied(handles[1], copy.target, Instant::now());
        if !self.frames.viewer {
            return None;
        }
        let Some(picture) = Picture::of(&copy.target) else {
            self.frames.complain("the screen is a size the frame ring can't hold");
            return None;
        };
        let Some(source) = self.source_of(copy, handles, &picture) else {
            self.frames.complain("a screen's surfaces are smaller than their numbers say");
            return None;
        };
        self.start_ring_copy(source, copy.own_space)
    }

    fn start_ring_copy(&mut self, source: Source, own_space: SpaceId) -> Option<SlotInFlight> {
        let Some(ring) = self.frames.ring.as_mut() else {
            self.frames.behind = true;
            return None;
        };
        let slot_memory = ring.memory.iter().map(|slot| slot.size).min().unwrap_or(0);
        if !picture_fits(&self.backend, &ring.picture, (&source.surface, source.size), slot_memory) {
            self.frames.complain("a screen's surfaces are smaller than their numbers say");
            return None;
        }
        let Some(slot) = ring.slots.claim() else {
            self.frames.counts.skipped += 1;
            self.frames.behind = true;
            return None;
        };
        let copy = SlotCopy {
            source: source.surface,
            source_address: source.address,
            slot_address: ring.memory[slot as usize].address,
            slot_pitch: ring.picture.pitch,
            width: ring.picture.width,
            height: ring.picture.height,
            own_space,
        };
        let flight = SlotInFlight { generation: ring.generation, slot };
        match self.backend.start_slot_copy(&mut services!(self), &copy) {
            StagingStart::Running => Some(flight),
            StagingStart::Finished(status) => {
                self.ring_copy_ended(flight, status);
                None
            }
        }
    }

    /// The GPU is done with a slot. The frame is the viewer's to see from here, if the ring lasted.
    /// A failure costs one frame, and the guest doesn't hear of it: its own copy went well.
    pub(super) fn ring_copy_ended(&mut self, flight: SlotInFlight, status: Status) {
        let current = self.frames.is_current(flight.generation);
        let sequence = self.frames.sequence + 1;
        let Some(ring) = self.frames.ring_of(flight.generation) else { return };
        if current && status == Status::SUCCESS {
            let replaced = ring.slots.finished(flight.slot, sequence);
            self.frames.sequence = sequence;
            self.frames.counts.copied += 1;
            self.frames.counts.replaced += u64::from(replaced);
        } else {
            ring.slots.abandoned(flight.slot);
            if status != Status::SUCCESS {
                self.frames.complain("a frame didn't reach the ring: it may be 10-bit, or the copy failed");
            }
        }
        if current {
            self.frames.behind = false;
        }
    }

    /// Fetches the picture from the staging surface, for a ring that doesn't have the newest one
    /// and no frame on the way: a new viewer, a new size, or a last frame that was skipped. Nothing
    /// else is running, the surface can't go during this, and it takes one short copy.
    fn catch_up(&mut self) {
        self.frames.behind = false;
        let Some(screen) = self.frames.screen else { return };
        let size = self.allocations.get(&screen.staging).map_or(0, |allocation| allocation.size);
        let (Ok(space), Ok(address)) = (self.own_space(), self.surface_address(screen.staging)) else {
            return;
        };
        let source = Source { surface: screen.surface, address, size };
        if let Some(flight) = self.start_ring_copy(source, space) {
            let status = self.wait_for_copy();
            self.ring_copy_ended(flight, status);
        }
    }
}
