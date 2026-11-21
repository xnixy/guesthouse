//! A GPU address space of its own for each space the core makes.
//!
//! amdgpu gives each open render node one VM. Guest processes place their own reservations, and two
//! processes often place them at the same addresses, so each space opens the node again and gets a
//! VM of its own. Memory is made on the backend's first open and taken into a space's open with a
//! dma-buf before it is mapped there. amdgpu won't hand out adopted host pages that way, so a space
//! adopts those pages itself, unless they reached the first open as a dma-buf of the guest's memory
//! file: see [`crate::udmabuf`]. A buffer taken in is kept while any of its mappings in the space
//! are, and let go after the last.

use crate::device::Device;
use crate::fault::{self, Fault};
use crate::fence_ib::{FenceIbs, Writer};
use crate::info::HW_IP_VCN_ENC;
use crate::memory::{self, GemHandle, MapFlags};
use crate::reset::{self, ContextCalls, Contexts};
use crate::run_wait::{self, Patience, Waited};
use crate::submit::{self, Context, Ib, Syncobj};
use crate::video_ib::{self, Copied, VideoIb};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io;

/// Which buffers a space has taken in, and its mappings of them. Kept apart from the files so the
/// counting can be tested without a card.
#[derive(Debug, Default)]
pub struct Books {
    /// By the buffer's handle on the first open: its handle here, and how many mappings use it.
    taken_in: HashMap<GemHandle, (GemHandle, u32)>,
    /// By address: the buffer's handle on the first open, and the mapping's place in it.
    mappings: BTreeMap<u64, Placed>,
}

/// Where in its buffer a mapping starts, and how long it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placed {
    buffer: GemHandle,
    offset: u64,
    length: u64,
}

/// A mapping on its way out, and whether the buffer's handle here must be let go after it.
#[derive(Debug, PartialEq, Eq)]
pub struct Unmapping {
    pub handle: GemHandle,
    pub address: u64,
    pub length: u64,
    pub last_use: bool,
}

impl Books {
    /// The handle here for `buffer`, if it was taken in.
    pub fn handle_of(&self, buffer: GemHandle) -> Option<GemHandle> {
        self.taken_in.get(&buffer).map(|&(handle, _)| handle)
    }

    /// Notes `length` bytes of `buffer` from `offset`, mapped at `address` through `handle`.
    pub fn add(&mut self, buffer: GemHandle, handle: GemHandle, address: u64, offset: u64, length: u64) {
        self.taken_in.entry(buffer).or_insert((handle, 0)).1 += 1;
        self.mappings.insert(address, Placed { buffer, offset, length });
    }

    pub fn remove(&mut self, address: u64) -> Option<Unmapping> {
        let Placed { buffer, length, .. } = self.mappings.remove(&address)?;
        let (handle, uses) = self.taken_in.get_mut(&buffer)?;
        let handle = *handle;
        *uses -= 1;
        let last_use = *uses == 0;
        if last_use {
            self.taken_in.remove(&buffer);
        }
        Some(Unmapping { handle, address, length, last_use })
    }

    /// The handles here of every buffer taken in.
    pub fn handles(&self) -> Vec<GemHandle> {
        self.taken_in.values().map(|&(handle, _)| handle).collect()
    }

    /// Every mapping of `buffer` here.
    pub fn addresses_of(&self, buffer: GemHandle) -> Vec<u64> {
        let of_buffer = self.mappings.iter().filter(|(_, placed)| placed.buffer == buffer);
        of_buffer.map(|(&address, _)| address).collect()
    }

    /// The buffer behind an address, by its handle on the first open, and how far into it the
    /// address is.
    pub fn locate(&self, address: u64) -> Option<(GemHandle, u64)> {
        let (&start, placed) = self.mappings.range(..=address).next_back()?;
        (address - start < placed.length).then_some((placed.buffer, placed.offset + (address - start)))
    }

    /// Whether `bytes` from `address` lie inside one mapping here.
    pub fn holds(&self, address: u64, bytes: u64) -> bool {
        let Some((&start, placed)) = self.mappings.range(..=address).next_back() else {
            return false;
        };
        let end = start.saturating_add(placed.length);
        address.checked_add(bytes).is_some_and(|wanted| wanted <= end)
    }

    /// Whether every byte of `bytes` from `address` is mapped here, in one mapping or in several that
    /// follow one another, as a guest may lay two buffers end to end.
    pub fn covers(&self, address: u64, bytes: u64) -> bool {
        let Some(end) = address.checked_add(bytes) else {
            return false;
        };
        let mut reached = address;
        while reached < end {
            let mapped_to = self
                .mappings
                .range(..=reached)
                .next_back()
                .map(|(&start, placed)| start.saturating_add(placed.length));
            match mapped_to {
                Some(mapped_to) if mapped_to > reached => reached = mapped_to,
                _ => return false,
            }
        }
        true
    }
}

/// Where a buffer on the backend's first open came from, which says how a space takes it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Made on the card, and handed over as a dma-buf.
    Made,
    /// Host pages at this address, of this size. A space adopts them again by the address, or takes
    /// them in as it does memory made on the card when the first open has them as a dma-buf.
    HostPages { address: u64, size: u64, as_dma_buf: bool },
}

/// One space's open of the render node, and what it holds.
#[derive(Debug)]
pub struct SpaceVm {
    file: File,
    books: Books,
    /// One amdgpu context for each engine family, made on first use and again after a reset.
    /// Closing the file frees them.
    contexts: Contexts<Context>,
    /// Whether work here has faulted, after which nothing more is run here.
    faults: fault::Watch,
    /// The command buffers that have the card write a fence, made on first use.
    fence_ibs: Option<FenceIbs>,
    /// The daemon's copy of the video commands being run here, made on first use.
    video_ib: Option<VideoIb>,
    /// A run from that buffer was sent and never seen to end, so the card may still read it. It is
    /// not written again, and no more video work is run here: see [`Self::run_video`].
    video_in_doubt: bool,
    /// Whether that has been said yet, as it is said once.
    video_refusal_said: bool,
    /// The runs sent without waiting that the kernel hasn't been seen to retire, oldest first.
    on_card: VecDeque<OnCard>,
    /// The fences of runs seen to retire since they were last asked for.
    retired: Vec<GuestFence>,
}

/// Where a run's fence is in guest memory, by guest physical address, and the value it ends with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFence {
    pub queue: u32,
    pub address: u64,
    pub value: u64,
}

/// A run on the card: the slot its fence writer is in, what the kernel signals when it retires, and
/// the fence the run writes last.
#[derive(Debug)]
struct OnCard {
    slot: usize,
    retired: Syncobj,
    /// None once its queue has gone, as the fence's page may then be another queue's.
    fence: Option<GuestFence>,
}

/// How a run on the card ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ran {
    Retired,
    /// amdgpu kept saying try again, so nothing ran, and the work can come again later.
    Refused,
    /// The kernel neither finished nor reset it in time.
    Hung,
    /// Work here reached an address with nothing mapped, in this run or an earlier one. Nothing
    /// more is run here.
    Faulted(Fault),
}

impl SpaceVm {
    pub fn open(device: &Device) -> io::Result<Self> {
        Ok(Self {
            file: device.open_again()?,
            books: Books::default(),
            contexts: Contexts::default(),
            faults: fault::Watch::default(),
            fence_ibs: None,
            video_ib: None,
            video_in_doubt: false,
            video_refusal_said: false,
            on_card: VecDeque::new(),
            retired: Vec::new(),
        })
    }

    /// The buffer behind an address here, by its handle on the first open, and the offset into it.
    pub fn locate(&self, address: u64) -> Option<(GemHandle, u64)> {
        self.books.locate(address)
    }

    /// Whether `bytes` from `address` lie inside one mapping here.
    pub fn holds(&self, address: u64, bytes: u64) -> bool {
        self.books.holds(address, bytes)
    }

    /// Whether every byte of `bytes` from `address` is mapped here, in one mapping or several.
    pub fn covers(&self, address: u64, bytes: u64) -> bool {
        self.books.covers(address, bytes)
    }

    /// The first command buffer not wholly mapped here. The card would fault reading it, and a fault
    /// costs every program on the card a reset.
    pub fn first_unmapped<'a>(&self, ibs: &'a [Ib]) -> Option<&'a Ib> {
        ibs.iter().find(|ib| !self.books.holds(ib.address, u64::from(ib.bytes)))
    }

    /// The fault that stopped this space, if its work has had one.
    pub fn faulted(&self) -> Option<Fault> {
        self.faults.found()
    }

    /// Runs command buffers already mapped here on the engine family `ip_type`, in order, and waits
    /// for the last to retire, as long as `patience` allows. Nothing is run once work here has
    /// faulted. amdgpu can record a fault a moment after its run retires, so there is a look before
    /// a run as well as after it. `queue` is the guest's queue the work is from, or `None` for the
    /// daemon's own.
    pub fn run(
        &mut self,
        ip_type: u32,
        queue: Option<u32>,
        ibs: &[Ib],
        patience: Patience,
    ) -> io::Result<Ran> {
        self.run_with(ip_type, queue, ibs, None, patience)
    }

    /// Runs a copy of a guest's video commands on the card's video ring, and waits for it as
    /// [`Self::run`] does. The copy is written into a buffer of the daemon's own here, made at the
    /// first run: see [`crate::video_ib`] for why the guest's own buffers are never run there.
    /// `alignment` is the card's for a new buffer.
    ///
    /// The one buffer is written again for each run, so a run has to be over before the next. One
    /// that was sent and not seen to end, because it hung past the daemon's patience or the wait
    /// for it failed, may still be read by the card, with its own lengths. Writing the next run's
    /// commands over it would have the card run a mix of the two. So from then on nothing is
    /// written and no video work is run here: see [`Self::video_stopped`].
    pub fn run_video(
        &mut self,
        alignment: u64,
        queue: u32,
        copied: &Copied,
        patience: Patience,
    ) -> io::Result<Ran> {
        if self.video_in_doubt {
            return Err(io::Error::other(VIDEO_STOPPED));
        }
        if self.video_ib.is_none() {
            let made = VideoIb::make(&self.file, alignment)
                .map_err(|error| step("making the buffer for video commands", error))?;
            self.video_ib = Some(made);
        }
        let ib = self.video_ib.as_mut().expect("made above");
        ib.write(copied);
        let own = ib.handle();
        self.run_with(HW_IP_VCN_ENC, Some(queue), copied.runs(), Some(own), patience)
    }

    /// Whether video work is no longer run here, as an earlier run was sent and never seen to end.
    pub fn video_stopped(&self) -> bool {
        self.video_in_doubt
    }

    /// True the first time it is asked, for a warning that is to be said once for this space.
    pub fn first_video_refusal(&mut self) -> bool {
        !std::mem::replace(&mut self.video_refusal_said, true)
    }

    /// [`Self::run`], with a buffer of the daemon's own here that the run reads its commands from.
    fn run_with(
        &mut self,
        ip_type: u32,
        queue: Option<u32>,
        ibs: &[Ib],
        own: Option<GemHandle>,
        patience: Patience,
    ) -> io::Result<Ran> {
        only_copies_for_the_video_ring(ip_type, ibs)?;
        if let Some(fault) = self.faults.look(&self.file) {
            return Ok(Ran::Faulted(fault));
        }
        let ran = self.submit_and_wait(ip_type, queue, ibs, own, patience);
        match (ran, self.faults.look(&self.file)) {
            (Ok(Ran::Retired), Some(fault)) => Ok(Ran::Faulted(fault)),
            (ran, _) => ran,
        }
    }

    fn submit_and_wait(
        &mut self,
        ip_type: u32,
        queue: Option<u32>,
        ibs: &[Ib],
        own: Option<GemHandle>,
        patience: Patience,
    ) -> io::Result<Ran> {
        let signal = Syncobj::create(&self.file).map_err(|error| step("making a sync object", error))?;
        // The guest doesn't say which buffers the work uses, so it may use any mapped here.
        let mut buffers = self.books.handles();
        buffers.extend(own);
        let submitted = self.contexts.submit(&self.file, ip_type, queue, |context| {
            submit::submit_all(&self.file, context, ip_type, ibs, &buffers, &signal).map(|_| ())
        });
        let ran = match submitted {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(Ran::Refused),
            Err(error) => Err(step("submitting", error)),
            Ok(()) => {
                let waited = signal
                    .arm_eventfd(&self.file)
                    .map_err(|error| step("arming the eventfd", error))
                    .and_then(|event| run_wait::wait_for_run(&event, patience, ip_type))
                    .map(|waited| match waited {
                        Waited::Retired { .. } => Ran::Retired,
                        Waited::Hung => Ran::Hung,
                    });
                // The card reads the daemon's video buffer for as long as the run is on it.
                self.video_in_doubt |= own.is_some() && may_still_be_on_the_card(&waited);
                waited
            }
        };
        signal.destroy(&self.file);
        ran
    }

    /// Writes the command buffer that has the card store `value` at `fence` when a run is through,
    /// to go last in that run. Returns its slot with it, or `None` while every slot's run is still
    /// on the card.
    pub fn fence_writer(
        &mut self,
        alignment: u64,
        writer: Writer,
        fence: u64,
        value: u64,
    ) -> io::Result<Option<(usize, Ib)>> {
        self.take_back_slots(false);
        if self.fence_ibs.is_none() {
            let made =
                FenceIbs::make(&self.file, alignment).map_err(|error| step("making fence writers", error))?;
            self.fence_ibs = Some(made);
        }
        if let Some(written) = self.fence_ibs.as_mut().and_then(|ibs| ibs.write(writer, fence, value)) {
            return Ok(Some(written));
        }
        // Runs on other engines can retire before the oldest one does.
        self.take_back_slots(true);
        Ok(self.fence_ibs.as_mut().and_then(|ibs| ibs.write(writer, fence, value)))
    }

    /// Frees the slot of a fence writer whose run won't be sent.
    pub fn drop_fence_writer(&mut self, slot: usize) {
        if let Some(ibs) = &mut self.fence_ibs {
            ibs.give_back(slot);
        }
    }

    /// Sends command buffers already mapped here to run in order on the engine family `ip_type`,
    /// and doesn't wait for them. The last is the fence writer in `slot`, which writes `fence`.
    pub fn send(&mut self, ip_type: u32, ibs: &[Ib], slot: usize, fence: GuestFence) -> io::Result<()> {
        only_copies_for_the_video_ring(ip_type, ibs)?;
        let Some(fence_ibs) = &self.fence_ibs else {
            return Err(io::Error::other("no fence writers were made here"));
        };
        // As in `run`: nothing more is sent to a space whose work has faulted.
        if let Some(fault) = self.faults.look(&self.file) {
            return Err(io::Error::other(format!(
                "work here reached {:#x}, where nothing is mapped (fault status {:#x}), so nothing more is sent",
                fault.address, fault.status
            )));
        }
        let retired = Syncobj::create(&self.file).map_err(|error| step("making a sync object", error))?;
        // The guest doesn't say which buffers the work uses, so it may use any mapped here.
        let mut buffers = self.books.handles();
        buffers.push(fence_ibs.handle());
        let submitted = self.contexts.submit(&self.file, ip_type, Some(fence.queue), |context| {
            submit::submit_all(&self.file, context, ip_type, ibs, &buffers, &retired).map(|_| ())
        });
        match submitted {
            Ok(_) => {
                self.on_card.push_back(OnCard { slot, retired, fence: Some(fence) });
                Ok(())
            }
            Err(error) => {
                retired.destroy(&self.file);
                Err(error)
            }
        }
    }

    /// The fences of the runs the kernel has retired since this was last asked, looking at them all.
    pub fn take_retired(&mut self) -> Vec<GuestFence> {
        self.take_back_slots(true);
        std::mem::take(&mut self.retired)
    }

    /// Forgets the fences of a queue that has gone.
    pub fn forget_queue(&mut self, queue: u32) {
        self.contexts.forget_queue(queue);
        self.retired.retain(|fence| fence.queue != queue);
        for run in &mut self.on_card {
            run.fence = run.fence.filter(|fence| fence.queue != queue);
        }
    }

    /// Frees the slots of runs the kernel says have retired: the oldest ones, or with `all` any.
    fn take_back_slots(&mut self, all: bool) {
        let Some(fence_ibs) = &mut self.fence_ibs else { return };
        let mut still_on_card = VecDeque::new();
        while let Some(run) = self.on_card.pop_front() {
            // A sync object that can't be asked about is one the kernel has no run for.
            if run.retired.signalled(&self.file).unwrap_or(true) {
                run.retired.destroy(&self.file);
                fence_ibs.give_back(run.slot);
                self.retired.extend(run.fence);
            } else if all {
                still_on_card.push_back(run);
            } else {
                self.on_card.push_front(run);
                break;
            }
        }
        still_on_card.append(&mut self.on_card);
        self.on_card = still_on_card;
    }

    /// Maps `length` bytes of `buffer`, a handle on `device`, from `offset`, at `address`.
    #[allow(clippy::too_many_arguments)]
    pub fn map(
        &mut self,
        device: &Device,
        buffer: GemHandle,
        origin: Origin,
        address: u64,
        offset: u64,
        length: u64,
        flags: MapFlags,
    ) -> io::Result<()> {
        let fresh = self.books.handle_of(buffer).is_none();
        let handle = match (self.books.handle_of(buffer), origin) {
            (Some(handle), _) => handle,
            (None, Origin::Made | Origin::HostPages { as_dma_buf: true, .. }) => {
                let shared =
                    memory::export(device, buffer).map_err(|error| step("handing the buffer over", error))?;
                memory::import(&self.file, &shared).map_err(|error| step("taking the buffer in", error))?
            }
            (None, Origin::HostPages { address, size, as_dma_buf: false }) => {
                memory::adopt_userptr(&self.file, address, size, false)
                    .map_err(|error| step("adopting the pages", error))?
            }
        };
        if let Err(error) = memory::map(&self.file, handle, address, offset, length, flags) {
            if fresh {
                memory::close(&self.file, handle);
            }
            return Err(error);
        }
        self.books.add(buffer, handle, address, offset, length);
        Ok(())
    }

    /// Takes back the mapping at `address`, if there is one.
    pub fn unmap(&mut self, address: u64) -> io::Result<()> {
        let Some(unmapping) = self.books.remove(address) else {
            return Ok(());
        };
        let result = memory::unmap(&self.file, unmapping.handle, unmapping.address, unmapping.length);
        if unmapping.last_use {
            memory::close(&self.file, unmapping.handle);
        }
        result
    }

    /// Takes back every mapping of `buffer`, as before it is freed. Returns where they were.
    pub fn forget(&mut self, buffer: GemHandle) -> Vec<u64> {
        let addresses = self.books.addresses_of(buffer);
        for &address in &addresses {
            if let Err(error) = self.unmap(address) {
                log::warn!("GEM_VA unmap at {address:#x}: {error}");
            }
        }
        addresses
    }
}

impl ContextCalls for File {
    type Context = Context;

    fn make(&self) -> io::Result<Context> {
        Context::create(self)
    }

    fn spoiled(&self, context: &Context, error: &io::Error) -> bool {
        let flags = context.reset_flags(self).ok();
        let spoiled = reset::spoiled_by_reset(error, flags);
        if spoiled {
            let flag = |bit| flags.map_or("unknown".to_string(), |flags| (flags & bit != 0).to_string());
            log::error!(
                "amdgpu refused work after a reset ({error}). Video memory lost: {}, this space's \
                 work hung the card: {}. The space's contexts are made again, and guest queues from \
                 before the reset fail",
                flag(reset::FLAG_VRAM_LOST),
                flag(reset::FLAG_GUILTY)
            );
        }
        spoiled
    }

    fn free(&self, context: Context) {
        context.destroy(self);
    }
}

/// `error`, saying which step it came from.
fn step(doing: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{doing}: {error}"))
}

/// Why video work isn't run in a space whose video buffer is in doubt.
const VIDEO_STOPPED: &str = "an earlier video run here was sent and never seen to end, so the buffer its \
                             commands are in is not written again";
/// Why a command buffer a guest placed isn't sent to the video ring.
const NOT_A_COPY: &str = "the video ring is only given the daemon's copy of a guest's commands";

/// Refuses work for the video ring unless every command buffer lies in the daemon's own video
/// buffer, where no guest mapping can be. amdgpu maps a video command buffer with the CPU, and for
/// one that is guest pages that is a fault in the host's kernel: see [`crate::video_ib`]. Every
/// submission here passes this, so no caller can send the ring a guest's buffer by mistake.
fn only_copies_for_the_video_ring(ip_type: u32, ibs: &[Ib]) -> io::Result<()> {
    if ip_type == HW_IP_VCN_ENC && !ibs.iter().all(video_ib::holds) {
        return Err(io::Error::other(NOT_A_COPY));
    }
    Ok(())
}

/// Whether a run that was sent may still be on the card: when it ended any way but with the kernel
/// saying it retired, which it also says of a run it reset.
fn may_still_be_on_the_card(waited: &io::Result<Ran>) -> bool {
    !matches!(waited, Ok(Ran::Retired))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUFFER: GemHandle = GemHandle(7);
    const HERE: GemHandle = GemHandle(3);
    const AT: u64 = 0x40_0000;
    const SIZE: u64 = 0x1_0000;

    /// A space with no card behind its file: every call to the kernel on it fails, so a test sees
    /// what is refused before the kernel is asked.
    fn space_without_a_card() -> SpaceVm {
        SpaceVm {
            file: File::open("/dev/null").unwrap(),
            books: Books::default(),
            contexts: Contexts::default(),
            faults: fault::Watch::default(),
            fence_ibs: None,
            video_ib: None,
            video_in_doubt: false,
            video_refusal_said: false,
            on_card: VecDeque::new(),
            retired: Vec::new(),
        }
    }

    /// A command buffer where a guest places its own, and one inside the daemon's video buffer.
    const GUESTS: Ib = Ib { address: 0x4_2000_1000, bytes: 0x40 };
    const COPY: Ib = Ib { address: video_ib::ADDRESS + 256, bytes: 0x40 };

    #[test]
    fn the_video_ring_takes_only_command_buffers_in_the_daemons_video_buffer() {
        use crate::info::{HW_IP_COMPUTE, HW_IP_DMA, HW_IP_GFX};
        let refused =
            |ibs: &[Ib]| only_copies_for_the_video_ring(HW_IP_VCN_ENC, ibs).unwrap_err().to_string();
        assert_eq!(refused(&[GUESTS]), NOT_A_COPY);
        assert_eq!(refused(&[COPY, GUESTS]), NOT_A_COPY, "one guest buffer among copies");
        let past_the_end = Ib { address: video_ib::ADDRESS + video_ib::BYTES as u64 - 4, bytes: 8 };
        assert_eq!(refused(&[past_the_end]), NOT_A_COPY);
        assert!(only_copies_for_the_video_ring(HW_IP_VCN_ENC, &[COPY, COPY]).is_ok());
        // The other engines run a guest's buffers where they lie, as before.
        for ip_type in [HW_IP_GFX, HW_IP_COMPUTE, HW_IP_DMA] {
            assert!(only_copies_for_the_video_ring(ip_type, &[GUESTS]).is_ok());
        }
    }

    #[test]
    fn neither_way_of_sending_work_gives_the_video_ring_a_guests_buffer() {
        let mut space = space_without_a_card();
        let waited = space.run(HW_IP_VCN_ENC, Some(1), &[GUESTS], Patience::RUN);
        assert_eq!(waited.unwrap_err().to_string(), NOT_A_COPY);
        let fence = GuestFence { queue: 1, address: 0x1000, value: 1 };
        let sent = space.send(HW_IP_VCN_ENC, &[GUESTS, COPY], 0, fence);
        assert_eq!(sent.unwrap_err().to_string(), NOT_A_COPY);
        // Refused before the kernel was asked anything, so nothing here is in doubt.
        assert!(!space.video_stopped());
    }

    #[test]
    fn a_run_is_over_only_when_the_kernel_says_it_retired() {
        assert!(!may_still_be_on_the_card(&Ok(Ran::Retired)));
        assert!(may_still_be_on_the_card(&Ok(Ran::Hung)));
        assert!(may_still_be_on_the_card(&Err(io::Error::other("arming the eventfd"))));
    }

    #[test]
    fn no_video_work_is_run_once_a_video_run_was_never_seen_to_end() {
        let mut copied = Copied::default();
        copied.place(8, 256).unwrap().copy_from_slice(&[0x8, 0x1]);
        let mut space = space_without_a_card();
        // Nothing was sent yet: the run fails making its buffer, and the space isn't stopped by that.
        let failed = space.run_video(0x1000, 1, &copied, Patience::RUN).unwrap_err().to_string();
        assert!(failed.starts_with("making the buffer for video commands: "), "{failed}");
        assert!(!space.video_stopped());

        space.video_in_doubt = true;
        assert!(space.video_stopped());
        let refused = space.run_video(0x1000, 1, &copied, Patience::RUN).unwrap_err().to_string();
        assert_eq!(refused, VIDEO_STOPPED);
        assert!(space.video_ib.is_none(), "nothing was made or written");
        assert!(space.first_video_refusal(), "said the first time");
        assert!(!space.first_video_refusal() && !space.first_video_refusal(), "and not again");
        assert!(space.video_stopped());
    }

    #[test]
    fn a_buffer_mapped_twice_is_let_go_after_its_second_mapping() {
        let mut books = Books::default();
        books.add(BUFFER, HERE, AT, 0, SIZE);
        books.add(BUFFER, HERE, AT + SIZE, SIZE, SIZE);
        assert_eq!(books.handle_of(BUFFER), Some(HERE));
        let first = books.remove(AT).unwrap();
        assert_eq!(first, Unmapping { handle: HERE, address: AT, length: SIZE, last_use: false });
        assert!(books.remove(AT + SIZE).unwrap().last_use);
        assert_eq!(books.handle_of(BUFFER), None);
    }

    #[test]
    fn an_address_with_nothing_mapped_has_nothing_to_take_back() {
        let mut books = Books::default();
        assert_eq!(books.remove(AT), None);
    }

    #[test]
    fn a_buffers_mappings_are_found_by_the_buffer() {
        let mut books = Books::default();
        books.add(BUFFER, HERE, AT, 0, SIZE);
        books.add(GemHandle(8), GemHandle(4), AT + SIZE, 0, SIZE);
        assert_eq!(books.addresses_of(BUFFER), vec![AT]);
    }

    #[test]
    fn a_buffer_mapped_twice_is_listed_once() {
        let mut books = Books::default();
        books.add(BUFFER, HERE, AT, 0, SIZE);
        books.add(BUFFER, HERE, AT + SIZE, SIZE, SIZE);
        books.add(GemHandle(8), GemHandle(4), AT + 2 * SIZE, 0, SIZE);
        let mut handles = books.handles();
        handles.sort_by_key(|handle| handle.0);
        assert_eq!(handles, vec![HERE, GemHandle(4)]);
    }

    #[test]
    fn an_address_is_found_in_its_buffer_by_the_mapping_s_offset() {
        let mut books = Books::default();
        books.add(BUFFER, HERE, AT, 0x3000, SIZE);
        assert_eq!(books.locate(AT), Some((BUFFER, 0x3000)));
        assert_eq!(books.locate(AT + 0x10), Some((BUFFER, 0x3010)));
        assert_eq!(books.locate(AT + SIZE), None);
        assert_eq!(books.locate(AT - 1), None);
    }

    #[test]
    fn a_range_is_covered_across_mappings_that_follow_one_another() {
        let mut books = Books::default();
        books.add(BUFFER, HERE, AT, 0, SIZE);
        books.add(GemHandle(8), GemHandle(4), AT + SIZE, 0, SIZE);
        books.add(GemHandle(9), GemHandle(5), AT + 3 * SIZE, 0, SIZE);
        assert!(books.covers(AT, SIZE));
        assert!(books.covers(AT + 0x100, 2 * SIZE - 0x100));
        assert!(books.covers(AT, 0));
        assert!(!books.covers(AT, 2 * SIZE + 1));
        assert!(!books.covers(AT + SIZE, 3 * SIZE));
        assert!(!books.covers(AT - 1, 2));
        assert!(!books.covers(AT + 2 * SIZE, 1));
        assert!(!books.covers(u64::MAX - 1, 4));
    }

    #[test]
    fn a_range_is_held_only_inside_one_mapping() {
        let mut books = Books::default();
        books.add(BUFFER, HERE, AT, 0, SIZE);
        assert!(books.holds(AT, SIZE));
        assert!(books.holds(AT + 0x100, 0x100));
        assert!(!books.holds(AT + 0x100, SIZE));
        assert!(!books.holds(AT - 0x100, 0x200));
        assert!(!books.holds(AT + SIZE, 4));
        assert!(!books.holds(u64::MAX - 1, 4));
    }
}
