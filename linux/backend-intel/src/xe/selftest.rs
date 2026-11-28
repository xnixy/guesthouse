//! The host's own proof that this backend's use of xe is right, with no guest: `intel-probe --run`.
//!
//! It makes a VM, three small pieces of memory of the kinds a guest's work uses (system memory, the
//! card's own, and host pages of a shared file), binds them at the addresses a guest uses, runs one
//! tiny batch on each engine class with a fence the card writes, copies with the copy engine, reads
//! the engine clock, makes a render queue with its colour cache keyed by binding table where xe
//! offers that, and frees everything. Each step prints `PASS`, `FAIL` with what the kernel
//! said, or `SKIP` with what it was waiting on, and a failed step doesn't stop the ones that don't
//! need it.
//!
//! Everything it makes is its own and tiny, and it goes again at the end whatever failed. Two more
//! steps, a fault and a hang, are made on purpose to see xe recover. They can disturb the desktop
//! on the same card, so they run only when asked for and after a typed yes: see [`Options`].

use super::abi::{EngineInstance, Keying, Sync};
use super::batch;
use super::device::{CpuMap, Xe};
use super::guest_registers::{self, GuestWrite};
use super::plan::{self, BufferUse, Placements, Point, pat};
use crate::card::CardFacts;
use crate::protocol::partition;
use crate::xe_query::{Engine, MemoryRegion};
use guesthouse_backend::GPU_RANGES;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// The size of each buffer the test makes.
const BUFFER_BYTES: u64 = 64 << 10;
/// The host pages: a shared file's, as the guest's are.
const HOST_BYTES: u64 = 2 << 20;
/// Where the host pages are bound low: the bottom of the addresses every space has.
const HOST_LOW: u64 = GPU_RANGES[0].start;
/// And high: just under the top of the CPU's user addresses, where a 48-bit card's guest may map.
const HOST_HIGH: u64 = (1 << 47) - HOST_BYTES;
/// Where the test's own batch lies, right after the low host pages.
const BATCH_AT: u64 = HOST_LOW + HOST_BYTES;
/// The card address bits [`HOST_HIGH`] needs.
const HIGH_ADDRESS_BITS: u64 = 48;

/// Offsets inside the pieces of memory: where each engine's store lands, where the fence is, and
/// where a copy lands in the host pages.
const STORE_OFFSET: u64 = 0x100;
const HIGH_STORE_OFFSET: u64 = 0x200;
const FENCE_OFFSET: u64 = 0x800;
const COPY_OFFSET: u64 = 0x1_0000;
/// Where the register reads land in the low host pages, past the copy: two dwords a write.
const REGISTERS_OFFSET: u64 = 0x2_0000;
/// Where the keyed render queue's store lands in the low host pages, and its register read after it.
const KEYING_OFFSET: u64 = 0x2_1000;
const KEYING_REGISTER_OFFSET: u64 = KEYING_OFFSET + 8;
/// The register xe sets at the start of a render queue keyed by binding table, and the bit.
const COMMON_SLICE_CHICKEN3: u32 = 0x7304;
const STATE_CACHE_PERF_FIX_OFF: u32 = 1 << 13;

/// How long a run that should finish at once is waited for.
const RUN_PATIENCE: Duration = Duration::from_secs(2);
/// How long xe is given to end a fault or a hang. Its own job timeout is a few seconds.
const BREAK_PATIENCE: Duration = Duration::from_secs(30);
/// What a person types to let the break steps run.
pub const BREAK_WORD: &str = "yes";

/// The marks stored by batches, told apart by engine class and target.
const STORE_MARK: u64 = 0x6775_6573_7400_0000;
const FENCE_MARK: u64 = 0x6665_6E63_6500_0000;

/// What to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Also make a fault and a hang on purpose, after a typed yes.
    pub break_steps: bool,
}

/// How the steps came out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    /// The names of the steps that failed, in the order they ran.
    pub failed_steps: Vec<String>,
    /// Whether the step `render cache keying` passed: xe made a render queue keyed by binding
    /// table and ran a batch on it.
    pub cache_keyed: bool,
}

impl Summary {
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }

    /// The failed steps that the guest's work needs: see [`work_needs`].
    pub fn failed_that_work_needs(&self) -> Vec<String> {
        self.failed_steps.iter().filter(|step| work_needs(step)).cloned().collect()
    }

    /// The failed steps that work on the card can do without.
    pub fn failed_that_work_does_without(&self) -> Vec<String> {
        self.failed_steps.iter().filter(|step| !work_needs(step)).cloned().collect()
    }
}

/// The engine classes the guest's work runs on: render, compute and copy. The video engines only
/// serve the guest's video work, which can fail alone.
const ENGINES_WORK_NEEDS: [&str; 3] = ["engine rcs", "engine ccs", "engine bcs"];

/// The other steps the guest's work needs: an address space, buffers, host pages and binds. The
/// clock reading and the cleanup aren't among them, nor the break steps, nor `copy`: nothing on the
/// work path sends its hand-written copy instruction (frame copies are the CPU's), so its failure
/// is a warning only.
/// Nor `render cache keying`: without it the guest's driver is told to flush for itself.
const STEPS_WORK_NEEDS: [&str; 4] = ["vm", "buffers", "userptr", "binds"];

/// Whether a failure of `step` keeps the guest's work off the card.
pub fn work_needs(step: &str) -> bool {
    STEPS_WORK_NEEDS.contains(&step) || ENGINES_WORK_NEEDS.iter().any(|engine| step.starts_with(engine))
}

/// Why a recorded card can't be tested: there is no card behind it.
pub fn recorded_card_refusal(recording: &Path) -> String {
    format!(
        "{} is a recording of a card, set by {}. There is no card to run on, so nothing is run.",
        recording.display(),
        crate::recording::RECORDED_CARD_VARIABLE
    )
}

/// What a kernel refusal most likely means, for a person reading the output.
fn meaning(error: &io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => "no permission on the render node",
        Some(libc::EINVAL) => "xe found the request wrong",
        Some(libc::ENOMEM | libc::ENOBUFS | libc::ENOSPC) => "no memory or room for it",
        Some(libc::EOPNOTSUPP) => "this kernel's xe doesn't support it",
        Some(libc::ENOENT) => "xe doesn't know the thing named",
        Some(libc::ECANCELED) => "the queue or its address space was reset or banned",
        Some(libc::ENODEV) => "the device is gone or wedged",
        Some(libc::EFAULT) => "xe couldn't reach the memory given",
        Some(libc::ENOTTY) => "this kernel's xe doesn't know the call",
        _ => "not a refusal this test knows",
    }
}

fn failed(what: &str, error: &io::Error) -> String {
    format!("{what}: {error} ({})", meaning(error))
}

/// A buffer the test made, with the CPU's view of it.
struct TestBuffer {
    handle: u32,
    map: CpuMap,
    /// The page-table entry it is bound with, which follows how the CPU caches it.
    pat_index: u16,
}

/// Host pages of a shared in-memory file, mapped in this process.
struct HostPages {
    start: *mut u8,
    length: usize,
}

impl HostPages {
    fn new(length: u64) -> io::Result<Self> {
        let length = length as usize;
        // SAFETY: a fresh in-memory file sized and mapped shared, as the guest's memory is; the
        // descriptor is closed once mapped, and the mapping is this value's alone.
        unsafe {
            let file = libc::memfd_create(c"guesthouse-intel-selftest".as_ptr(), libc::MFD_CLOEXEC);
            if file < 0 {
                return Err(io::Error::last_os_error());
            }
            let sized = libc::ftruncate(file, length as libc::off_t);
            let start = if sized == 0 {
                libc::mmap(
                    std::ptr::null_mut(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file,
                    0,
                )
            } else {
                libc::MAP_FAILED
            };
            let error = io::Error::last_os_error();
            libc::close(file);
            if start == libc::MAP_FAILED {
                return Err(error);
            }
            Ok(Self { start: start.cast(), length })
        }
    }

    fn map(&self) -> CpuMap {
        // SAFETY: the pages live as long as this value, which outlives every map the test takes.
        unsafe { CpuMap::borrowed(self.start, self.length) }
    }
}

impl Drop for HostPages {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe { libc::munmap(self.start.cast(), self.length) };
    }
}

fn read_qword(map: &CpuMap, offset: u64) -> Option<u64> {
    let mut bytes = [0; 8];
    map.read(offset, &mut bytes).then(|| u64::from_le_bytes(bytes))
}

/// A recognisable pattern for a buffer's bytes: each qword holds its own offset and a seed.
fn pattern(seed: u64, bytes: u64) -> Vec<u8> {
    (0..bytes / 8)
        .flat_map(|index| (seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes())
        .collect()
}

/// One run of the test: what it has made so far, and where it prints.
struct Run<'a> {
    xe: &'a mut dyn Xe,
    card: &'a CardFacts,
    out: &'a mut dyn Write,
    summary: Summary,
    vm: Option<u32>,
    binds: Option<u32>,
    bind_point: u64,
    done: Option<u32>,
    done_point: u64,
    system: Option<TestBuffer>,
    vram: Option<TestBuffer>,
    batch: Option<TestBuffer>,
    host: Option<HostPages>,
    /// Which pieces are bound, as each bind can fail alone.
    system_at: Option<u64>,
    vram_at: Option<u64>,
    host_low: bool,
    host_high: bool,
    batch_bound: bool,
    queues: Vec<u32>,
    extra_vms: Vec<u32>,
}

/// A step's result: what to print after PASS, or after FAIL, or why it was skipped.
enum Step {
    Pass(String),
    Fail(String),
    Skip(String),
}

impl<T> From<Result<T, String>> for Step
where
    T: Into<String>,
{
    fn from(result: Result<T, String>) -> Self {
        match result {
            Ok(detail) => Self::Pass(detail.into()),
            Err(why) => Self::Fail(why),
        }
    }
}

impl<'a> Run<'a> {
    fn step(&mut self, name: &str, body: impl FnOnce(&mut Self) -> Step) {
        let started = Instant::now();
        let step = body(self);
        let took = started.elapsed().as_millis();
        let line = match step {
            Step::Pass(detail) if detail.is_empty() => {
                self.summary.passed += 1;
                format!("PASS {name} ({took} ms)")
            }
            Step::Pass(detail) => {
                self.summary.passed += 1;
                format!("PASS {name} ({took} ms): {detail}")
            }
            Step::Fail(why) => {
                self.summary.failed += 1;
                self.summary.failed_steps.push(name.to_owned());
                format!("FAIL {name}: {why}")
            }
            Step::Skip(why) => {
                self.summary.skipped += 1;
                format!("SKIP {name}: {why}")
            }
        };
        let _ = writeln!(self.out, "{line}");
    }

    fn placements(&self) -> Result<Placements, String> {
        Placements::of(&self.card.regions).ok_or_else(|| "xe named no system memory region".to_string())
    }

    fn open(&mut self, sysfs: Option<&Path>) -> Step {
        let pages: Vec<String> = self
            .card
            .regions
            .iter()
            .map(|region| {
                let class = if region.class == MemoryRegion::VRAM { "vram" } else { "system" };
                format!("{class} page {:#x}", region.min_page_size)
            })
            .collect();
        let timeouts = sysfs.map(job_timeouts).unwrap_or_default();
        let timeouts =
            if timeouts.is_empty() { "job timeouts not found in sysfs".into() } else { timeouts.join(", ") };
        let faults = if self.xe.names_faults() { "fault addresses can be asked" } else { "no fault query" };
        Step::Pass(format!("{}; {timeouts}; {faults}", pages.join(", ")))
    }

    fn make_vm(&mut self) -> Result<String, String> {
        // No scratch page: a stray address must fault, not read zero.
        let vm = self.xe.create_vm(0).map_err(|error| failed("making an address space", &error))?;
        self.vm = Some(vm);
        let binds = self.xe.create_syncobj().map_err(|error| failed("making a sync object", &error))?;
        self.binds = Some(binds);
        let done = self.xe.create_syncobj().map_err(|error| failed("making a sync object", &error))?;
        self.done = Some(done);
        Ok(String::new())
    }

    /// Makes a buffer, writes a pattern through the CPU and reads it back.
    fn make_buffer(&mut self, usage: BufferUse, seed: u64) -> Result<TestBuffer, String> {
        let request = self.placements()?.buffer(BUFFER_BYTES, usage);
        let handle = self.xe.create_buffer(&request).map_err(|error| failed("making the buffer", &error))?;
        let map = match self.xe.map_buffer(handle, request.size) {
            Ok(map) => map,
            Err(error) => {
                let _ = self.xe.close_buffer(handle);
                return Err(failed("mapping the buffer for the CPU", &error));
            }
        };
        let mut buffer = TestBuffer { handle, map, pat_index: pat::for_buffer(request.cpu_caching, usage) };
        let written = pattern(seed, BUFFER_BYTES);
        let mut read = vec![0; written.len()];
        if !(buffer.map.write(0, &written) && buffer.map.read(0, &mut read)) || read != written {
            let _ = self.xe.close_buffer(buffer.handle);
            return Err("the CPU read back something other than it wrote".into());
        }
        Ok(buffer)
    }

    fn buffers(&mut self) -> Step {
        let has_vram = match self.placements() {
            Ok(placements) => placements.has_vram(),
            Err(why) => return Step::Fail(why),
        };
        let mut said = Vec::new();
        let mut failures = Vec::new();
        match self.make_buffer(BufferUse::HostFilled, 1) {
            Ok(buffer) => self.batch = Some(buffer),
            Err(why) => failures.push(format!("batch buffer: {why}")),
        }
        match self.make_buffer(BufferUse::HostFilled, 2) {
            Ok(buffer) => {
                self.system = Some(buffer);
                said.push("system memory");
            }
            Err(why) => failures.push(format!("system buffer: {why}")),
        }
        if has_vram {
            match self.make_buffer(BufferUse::Card, 3) {
                Ok(buffer) => {
                    self.vram = Some(buffer);
                    said.push("card memory");
                }
                Err(why) => failures.push(format!("card buffer: {why}")),
            }
        } else {
            said.push("no card memory on this card");
        }
        if failures.is_empty() { Step::Pass(said.join(", ")) } else { Step::Fail(failures.join("; ")) }
    }

    fn host_pages(&mut self) -> Result<String, String> {
        let pages =
            HostPages::new(HOST_BYTES).map_err(|error| failed("making a shared file's pages", &error))?;
        self.host = Some(pages);
        Ok(String::new())
    }

    /// One bind, signalled on the bind timeline and waited for.
    fn bind(&mut self, op: super::abi::BindOp) -> Result<(), String> {
        let (Some(vm), Some(binds)) = (self.vm, self.binds) else { return Err("no address space".into()) };
        let point = self.bind_point + 1;
        let signal = Sync::Signal { timeline: binds, point };
        self.xe
            .bind(vm, &op, Some(signal))
            .map_err(|error| failed(&format!("binding at {:#x}", op.address), &error))?;
        self.bind_point = point;
        match self.xe.point_reached(binds, point, RUN_PATIENCE) {
            Ok(true) => Ok(()),
            Ok(false) => {
                Err(format!("the bind at {:#x} was not in place after {RUN_PATIENCE:?}", op.address))
            }
            Err(error) => Err(failed("waiting for the bind", &error)),
        }
    }

    fn buffer_bind(buffer: &TestBuffer, address: u64) -> super::abi::BindOp {
        super::abi::BindOp {
            op: super::abi::BIND_MAP,
            buffer: buffer.handle,
            offset_or_host_address: 0,
            length: BUFFER_BYTES,
            address,
            pat_index: buffer.pat_index,
        }
    }

    fn binds(&mut self) -> Step {
        if self.vm.is_none() {
            return Step::Skip("needs the address space".into());
        }
        let mut failures = Vec::new();
        let mut bound = Vec::new();
        if let Some(op) = self.batch.as_ref().map(|buffer| Self::buffer_bind(buffer, BATCH_AT)) {
            match self.bind(op) {
                Ok(()) => self.batch_bound = true,
                Err(why) => failures.push(why),
            }
        }
        let address = partition::first_heap_32().base;
        if let Some(op) = self.system.as_ref().map(|buffer| Self::buffer_bind(buffer, address)) {
            match self.bind(op) {
                Ok(()) => {
                    self.system_at = Some(address);
                    bound.push(format!("system memory at {address:#x}"));
                }
                Err(why) => failures.push(why),
            }
        }
        let address = partition::standard().base;
        if let Some(op) = self.vram.as_ref().map(|buffer| Self::buffer_bind(buffer, address)) {
            match self.bind(op) {
                Ok(()) => {
                    self.vram_at = Some(address);
                    bound.push(format!("card memory at {address:#x}"));
                }
                Err(why) => failures.push(why),
            }
        }
        if let Some(host) = self.host.as_ref().map(|host| host.start as u64) {
            let high = self.card.config.va_bits >= HIGH_ADDRESS_BITS;
            for (address, wanted) in [(HOST_LOW, true), (HOST_HIGH, high)] {
                if !wanted {
                    bound.push(format!("no host pages at {address:#x}: the card has too few address bits"));
                    continue;
                }
                let op = super::abi::BindOp {
                    op: super::abi::BIND_MAP_USERPTR,
                    buffer: 0,
                    offset_or_host_address: host,
                    length: HOST_BYTES,
                    address,
                    pat_index: pat::SHARED_WITH_CPU,
                };
                match self.bind(op) {
                    Ok(()) => {
                        if address == HOST_LOW {
                            self.host_low = true;
                        } else {
                            self.host_high = true;
                        }
                        bound.push(format!("host pages at {address:#x}"));
                    }
                    Err(why) => failures.push(why),
                }
            }
        }
        if failures.is_empty() { Step::Pass(bound.join(", ")) } else { Step::Fail(failures.join("; ")) }
    }

    /// Where the fence goes: the high host pages if they are bound, as that is the address the
    /// design most needs proven, and the low ones otherwise.
    fn fence_address(&self) -> Option<u64> {
        if self.host_high {
            Some(HOST_HIGH + FENCE_OFFSET)
        } else {
            self.host_low.then_some(HOST_LOW + FENCE_OFFSET)
        }
    }

    /// Writes a batch into the test's batch buffer.
    fn write_batch(&mut self, dwords: &[u32]) -> Result<(), String> {
        let batch = self.batch.as_mut().filter(|_| self.batch_bound).ok_or("no batch buffer bound")?;
        if batch.map.write(0, &batch::bytes(dwords)) { Ok(()) } else { Err("the batch didn't fit".into()) }
    }

    /// Sends the batch on a new queue of one engine. Returns the queue.
    fn run_batch(
        &mut self,
        vm: u32,
        engine: EngineInstance,
        fence: Option<(u64, u64)>,
    ) -> Result<u32, String> {
        self.run_batch_keyed(vm, engine, fence, Keying::BindingIndex)
    }

    fn run_batch_keyed(
        &mut self,
        vm: u32,
        engine: EngineInstance,
        fence: Option<(u64, u64)>,
        keying: Keying,
    ) -> Result<u32, String> {
        let done = self.done.ok_or("no sync object")?;
        let queue =
            self.xe.create_queue(vm, engine, keying).map_err(|error| failed("making a queue", &error))?;
        self.queues.push(queue);
        self.done_point += 1;
        let done = Point { timeline: done, point: self.done_point };
        let last_bind = match (self.binds, self.bind_point) {
            (Some(timeline), point) if point > 0 && Some(vm) == self.vm => Some(Point { timeline, point }),
            _ => None,
        };
        let syncs = plan::exec_syncs(last_bind, fence, done);
        self.xe.exec(queue, BATCH_AT, &syncs).map_err(|error| failed("sending the batch", &error))?;
        Ok(queue)
    }

    fn wait_done(&mut self, patience: Duration) -> Result<bool, String> {
        let done = self.done.ok_or("no sync object")?;
        self.xe
            .point_reached(done, self.done_point, patience)
            .map_err(|error| failed("waiting for the batch", &error))
    }

    /// Destroys a queue a step is done with. One that won't go stays listed for the cleanup.
    fn close_queue(&mut self, queue: u32) {
        if self.xe.destroy_queue(queue).is_ok() {
            self.queues.retain(|kept| *kept != queue);
        }
    }

    /// One batch on one engine: a store into each kind of memory at each binding, then the fence.
    fn engine(&mut self, engine: EngineInstance) -> Step {
        let (Some(vm), Some(fence_at)) = (self.vm, self.fence_address()) else {
            return Step::Skip("needs the address space and the host pages bound".into());
        };
        if !self.batch_bound {
            return Step::Skip("needs the batch buffer bound".into());
        }
        let mark = |target: u64| STORE_MARK | u64::from(engine.class) << 8 | target;
        // (GPU address, expected value, which piece, offset in it)
        let mut stores = Vec::new();
        if let Some(address) = self.system_at {
            stores.push((address + STORE_OFFSET, mark(1), "system memory", STORE_OFFSET));
        }
        if let Some(address) = self.vram_at {
            stores.push((address + STORE_OFFSET, mark(2), "card memory", STORE_OFFSET));
        }
        if self.host_low {
            stores.push((HOST_LOW + STORE_OFFSET, mark(3), "host pages low", STORE_OFFSET));
        }
        if self.host_high {
            stores.push((HOST_HIGH + HIGH_STORE_OFFSET, mark(4), "host pages high", HIGH_STORE_OFFSET));
        }
        let mut dwords: Vec<u32> =
            stores.iter().flat_map(|(address, value, ..)| batch::store_qword(*address, *value)).collect();
        dwords.push(batch::BATCH_END);
        let fence_value = FENCE_MARK | u64::from(engine.class);
        let result = (|| -> Result<String, String> {
            self.write_batch(&dwords)?;
            let queue = self.run_batch(vm, engine, Some((fence_at, fence_value)))?;
            if !self.wait_done(RUN_PATIENCE)? {
                return Err(format!("the batch had not finished after {RUN_PATIENCE:?}"));
            }
            // The wait ends when the job does, faulted or not, so a ban is asked about first: a
            // faulted job then reads as one, not as a fence that wasn't written.
            match self.xe.queue_banned(queue) {
                Ok(false) => {}
                Ok(true) => {
                    let name =
                        Engine { class: engine.class, instance: engine.instance, gt: engine.gt }.class_name();
                    return Err(format!(
                        "xe banned the queue on {name}{}: its job faulted or hung",
                        engine.instance
                    ));
                }
                Err(error) => return Err(failed("asking whether the queue is banned", &error)),
            }
            let host = self.host.as_ref().ok_or("no host pages")?.map();
            let fence = read_qword(&host, FENCE_OFFSET);
            if fence != Some(fence_value) {
                return Err(format!(
                    "the card did not write the fence: {:#x} where {fence_value:#x} was expected",
                    fence.unwrap_or(0)
                ));
            }
            for (_, value, piece, offset) in &stores {
                let found = match *piece {
                    "system memory" => {
                        self.system.as_ref().and_then(|buffer| read_qword(&buffer.map, *offset))
                    }
                    "card memory" => self.vram.as_ref().and_then(|buffer| read_qword(&buffer.map, *offset)),
                    _ => read_qword(&host, *offset),
                };
                if found != Some(*value) {
                    return Err(format!(
                        "the store into {piece} did not land: read {:#x}",
                        found.unwrap_or(0)
                    ));
                }
            }
            self.close_queue(queue);
            Ok(format!("{} stores and the fence at {fence_at:#x}", stores.len()))
        })();
        result.into()
    }

    fn copy(&mut self) -> Step {
        let Some(engine) = plan::engine(&self.card.engines, Engine::COPY, 0) else {
            return Step::Skip("the card lists no copy engine".into());
        };
        let source = self.vram_at.or(self.system_at);
        let (Some(vm), Some(source), true, true) = (self.vm, source, self.host_low, self.batch_bound) else {
            return Step::Skip("needs a buffer, the host pages and the batch buffer bound".into());
        };
        let result = (|| -> Result<String, String> {
            let seed = 0x636F_7079;
            let written = pattern(seed, BUFFER_BYTES);
            let source_buffer =
                if self.vram_at.is_some() { self.vram.as_mut() } else { self.system.as_mut() };
            let source_buffer = source_buffer.ok_or("no source buffer")?;
            if !source_buffer.map.write(0, &written) {
                return Err("the source could not be written".into());
            }
            let copy =
                batch::copy_bytes(source, HOST_LOW + COPY_OFFSET, BUFFER_BYTES, batch::XE2_UNCACHED_MOCS)
                    .map_err(|_| "the copy doesn't fit one instruction")?;
            let mut dwords = copy.to_vec();
            dwords.push(batch::BATCH_END);
            self.write_batch(&dwords)?;
            let queue = self.run_batch(vm, engine, None)?;
            if !self.wait_done(RUN_PATIENCE)? {
                return Err(format!("the copy had not finished after {RUN_PATIENCE:?}"));
            }
            let mut copied = vec![0; written.len()];
            let host = self.host.as_ref().ok_or("no host pages")?.map();
            if !host.read(COPY_OFFSET, &mut copied) || copied != written {
                return Err("the host pages don't hold what the buffer held".into());
            }
            self.close_queue(queue);
            Ok(format!("{} KiB from {source:#x} into the host pages", BUFFER_BYTES >> 10))
        })();
        result.into()
    }

    /// Sets each register the guest's driver sets from its render batches, as it does, reading
    /// it just before and just after, and says what became of each write. It fails only when the
    /// batch doesn't run: what the registers do is for a person to read.
    fn guest_registers(&mut self) -> Step {
        let engine = plan::one_engine_per_class(&self.card.engines)
            .into_iter()
            .find(|engine| engine.class == Engine::RENDER);
        let Some(engine) = engine else {
            return Step::Skip("the card lists no render engine".into());
        };
        let Some(vm) = self.vm.filter(|_| self.host_low && self.batch_bound) else {
            return Step::Skip(
                "needs the address space, the low host pages and the batch buffer bound".into(),
            );
        };
        let writes = &guest_registers::RENDER_WRITES;
        let slot = |index: usize| REGISTERS_OFFSET + 8 * index as u64;
        let mut dwords = Vec::new();
        for (index, write) in writes.iter().enumerate() {
            dwords.extend(batch::store_register(write.register, HOST_LOW + slot(index)));
            dwords.extend(batch::load_register(write.register, write.value));
            dwords.extend(batch::store_register(write.register, HOST_LOW + slot(index) + 4));
        }
        dwords.push(batch::BATCH_END);
        let result = (|| -> Result<String, String> {
            let mut host = self.host.as_ref().ok_or("no host pages")?.map();
            let unread = guest_registers::UNREAD.to_le_bytes().repeat(2 * writes.len());
            if !host.write(slot(0), &unread) {
                return Err("the host pages hold no room for the reads".into());
            }
            self.write_batch(&dwords)?;
            let queue = self.run_batch(vm, engine, None)?;
            if !self.wait_done(RUN_PATIENCE)? {
                return Err(format!("the batch had not finished after {RUN_PATIENCE:?}"));
            }
            match self.xe.queue_banned(queue) {
                Ok(false) => {}
                Ok(true) => return Err("xe banned the queue: its job faulted or hung".into()),
                Err(error) => return Err(failed("asking whether the queue is banned", &error)),
            }
            self.close_queue(queue);
            let mut read = vec![0; unread.len()];
            if !host.read(slot(0), &mut read) {
                return Err("the reads could not be read back".into());
            }
            let dword =
                |at: usize| u32::from_le_bytes(read[4 * at..4 * at + 4].try_into().expect("four bytes"));
            let said: Vec<String> = writes
                .iter()
                .enumerate()
                .map(|(index, write)| describe(write, dword(2 * index), dword(2 * index + 1)))
                .collect();
            Ok(said.join(", "))
        })();
        result.into()
    }

    /// A render queue with its colour cache keyed by binding table and index, made as the guest's
    /// render queues are when this passes: a store and the fence, and a read of the register xe
    /// sets for such a queue. A register a batch can't read is no failure, as many can't be; one
    /// that reads without the bit is. The record tells the guest's driver the cache is keyed so
    /// only when this passed: see [`Summary::cache_keyed`].
    fn cache_keying(&mut self) -> Step {
        if !self.card.config.offers_cache_keying() {
            return Step::Skip("this kernel's xe doesn't offer it; Linux v7.1 and later do".into());
        }
        let engine = plan::one_engine_per_class(&self.card.engines)
            .into_iter()
            .find(|engine| engine.class == Engine::RENDER);
        let Some(engine) = engine else {
            return Step::Skip("the card lists no render engine".into());
        };
        let (Some(vm), Some(fence_at), true, true) =
            (self.vm, self.fence_address(), self.host_low, self.batch_bound)
        else {
            return Step::Skip(
                "needs the address space, the low host pages and the batch buffer bound".into(),
            );
        };
        let mark = STORE_MARK | 0x4B;
        let fence_value = FENCE_MARK | 0x4B;
        let mut dwords = batch::store_qword(HOST_LOW + KEYING_OFFSET, mark).to_vec();
        dwords.extend(batch::store_register(COMMON_SLICE_CHICKEN3, HOST_LOW + KEYING_REGISTER_OFFSET));
        dwords.push(batch::BATCH_END);
        let result = (|| -> Result<String, String> {
            let mut host = self.host.as_ref().ok_or("no host pages")?.map();
            let blank = [[0; 8].as_slice(), &guest_registers::UNREAD.to_le_bytes()].concat();
            if !host.write(KEYING_OFFSET, &blank) {
                return Err("the host pages hold no room for the store".into());
            }
            self.write_batch(&dwords)?;
            let queue = self.run_batch_keyed(
                vm,
                engine,
                Some((fence_at, fence_value)),
                Keying::BindingTableAndIndex,
            )?;
            if !self.wait_done(RUN_PATIENCE)? {
                return Err(format!("the batch had not finished after {RUN_PATIENCE:?}"));
            }
            match self.xe.queue_banned(queue) {
                Ok(false) => {}
                Ok(true) => return Err("xe banned the queue: its job faulted or hung".into()),
                Err(error) => return Err(failed("asking whether the queue is banned", &error)),
            }
            self.close_queue(queue);
            let fence = read_qword(&host, FENCE_OFFSET);
            if fence != Some(fence_value) {
                return Err(format!(
                    "the card did not write the fence: {:#x} where {fence_value:#x} was expected",
                    fence.unwrap_or(0)
                ));
            }
            let stored = read_qword(&host, KEYING_OFFSET);
            if stored != Some(mark) {
                return Err(format!("the store did not land: read {:#x}", stored.unwrap_or(0)));
            }
            let mut read = [0; 4];
            if !host.read(KEYING_REGISTER_OFFSET, &mut read) {
                return Err("the register read could not be read back".into());
            }
            let register = u32::from_le_bytes(read);
            if register == guest_registers::UNREAD {
                Ok(format!("{COMMON_SLICE_CHICKEN3:#x} can't be read from a batch, so its bit isn't seen"))
            } else if register & STATE_CACHE_PERF_FIX_OFF != 0 {
                Ok(format!("{COMMON_SLICE_CHICKEN3:#x} reads {register:#x}, with the bit set"))
            } else {
                // A batch's read of such a register isn't to be trusted on every card: one xe sets
                // in every render context reads as zero this way on Battlemage. xe took the
                // property and the queue ran, which is what the choice rests on.
                Ok(format!(
                    "{COMMON_SLICE_CHICKEN3:#x} reads {register:#x} from a batch, without the bit; xe took \
                     the property and the queue ran"
                ))
            }
        })();
        if result.is_ok() {
            self.summary.cache_keyed = true;
        }
        result.into()
    }

    fn clock(&mut self) -> Step {
        let Some(engine) = plan::one_engine_per_class(&self.card.engines).first().copied() else {
            return Step::Skip("the card lists no engine".into());
        };
        let result = (|| -> Result<String, String> {
            let read = |xe: &mut dyn Xe| {
                xe.engine_cycles(engine).map_err(|error| failed("reading the engine clock", &error))
            };
            let first = read(self.xe)?;
            std::thread::sleep(Duration::from_millis(2));
            let second = read(self.xe)?;
            let cycles = second.cycles.wrapping_sub(first.cycles);
            let nanoseconds = second.cpu_nanoseconds.saturating_sub(first.cpu_nanoseconds);
            if cycles == 0 || nanoseconds == 0 {
                return Err("the engine clock did not move".into());
            }
            let measured = u128::from(cycles) * 1_000_000_000 / u128::from(nanoseconds);
            let said = self.card.main_gt().map_or(0, |gt| gt.reference_clock);
            Ok(format!("{measured} cycles a second measured, {said} Hz reported, {} bits", second.width))
        })();
        result.into()
    }

    /// A store to an address nothing is bound at, in a VM of its own. xe should end the run without
    /// writing the fence and ban the queue.
    fn fault(&mut self) -> Step {
        self.broken_batch("the store to an unbound address", &{
            let mut dwords = batch::store_qword(HOST_HIGH - HOST_BYTES, STORE_MARK).to_vec();
            dwords.push(batch::BATCH_END);
            dwords
        })
    }

    /// A batch that jumps to itself. xe should end it at its job timeout and ban the queue.
    fn hang(&mut self) -> Step {
        self.broken_batch("the endless batch", &batch::batch_start(BATCH_AT))
    }

    fn broken_batch(&mut self, what: &str, dwords: &[u32]) -> Step {
        let Some(op) = self.batch.as_ref().map(|buffer| Self::buffer_bind(buffer, BATCH_AT)) else {
            return Step::Skip("needs the batch buffer".into());
        };
        let Some(engine) = plan::one_engine_per_class(&self.card.engines).first().copied() else {
            return Step::Skip("the card lists no engine".into());
        };
        let fence_offset = FENCE_OFFSET;
        let result = (|| -> Result<String, String> {
            let vm = self.xe.create_vm(0).map_err(|error| failed("making an address space", &error))?;
            self.extra_vms.push(vm);
            self.xe.bind(vm, &op, None).map_err(|error| failed("binding the batch buffer", &error))?;
            let batch = self.batch.as_mut().ok_or("no batch buffer")?;
            if !(batch.map.write(0, &batch::bytes(dwords)) && batch.map.write(fence_offset, &[0; 8])) {
                return Err("the batch didn't fit".into());
            }
            let started = Instant::now();
            let queue = self.run_batch(vm, engine, Some((BATCH_AT + fence_offset, FENCE_MARK)))?;
            if !self.wait_done(BREAK_PATIENCE)? {
                return Err(format!("xe had not ended {what} after {BREAK_PATIENCE:?}"));
            }
            let ended = started.elapsed();
            let fence = self.batch.as_ref().and_then(|batch| read_qword(&batch.map, fence_offset));
            if fence == Some(FENCE_MARK) {
                return Err(format!("{what} ran to its end and the fence was written"));
            }
            let banned = match self.xe.queue_banned(queue) {
                Ok(banned) => banned,
                Err(error) => return Err(failed("asking whether the queue is banned", &error)),
            };
            // A banned queue also refuses its next batch.
            self.done_point += 1;
            let done = Point { timeline: self.done.ok_or("no sync object")?, point: self.done_point };
            let next = self.xe.exec(queue, BATCH_AT, &plan::exec_syncs(None, None, done));
            let cancelled = next.as_ref().is_err_and(|error| error.raw_os_error() == Some(libc::ECANCELED));
            if !banned && !cancelled {
                return Err(format!(
                    "xe ended {what} but neither banned the queue nor refused its next batch"
                ));
            }
            self.close_queue(queue);
            Ok(format!(
                "ended after {} ms, fence unwritten, banned {banned}, next batch cancelled {cancelled}",
                ended.as_millis()
            ))
        })();
        result.into()
    }

    /// Frees everything made, whatever failed on the way. Queues first, then the address spaces,
    /// which takes their binds, then the buffers and the host pages.
    fn clean_up(&mut self) -> Step {
        let mut failures = Vec::new();
        let mut note = |what: &str, result: io::Result<()>| {
            if let Err(error) = result {
                failures.push(failed(what, &error));
            }
        };
        for queue in std::mem::take(&mut self.queues) {
            note("destroying a queue", self.xe.destroy_queue(queue));
        }
        for vm in std::mem::take(&mut self.extra_vms).into_iter().chain(self.vm.take()) {
            note("destroying an address space", self.xe.destroy_vm(vm));
        }
        for syncobj in [self.binds.take(), self.done.take()].into_iter().flatten() {
            note("destroying a sync object", self.xe.destroy_syncobj(syncobj));
        }
        for buffer in [self.batch.take(), self.system.take(), self.vram.take()].into_iter().flatten() {
            note("closing a buffer", self.xe.close_buffer(buffer.handle));
        }
        self.host = None;
        if failures.is_empty() { Step::Pass(String::new()) } else { Step::Fail(failures.join("; ")) }
    }
}

/// One write as the self-test prints it: the register, what was written, what became of it, and
/// what it read before and after when it could be read.
fn describe(write: &GuestWrite, before: u32, after: u32) -> String {
    let outcome = write.outcome(before, after);
    let (register, value) = (write.register, write.value);
    match outcome {
        guest_registers::Outcome::Unreadable => format!("{register:#x}={value:#x} {outcome}"),
        _ => format!("{register:#x}={value:#x} {outcome} ({before:#x} to {after:#x})"),
    }
}

/// Each engine class's job timeout in milliseconds, as xe's sysfs gives it under the card's device
/// folder: `tile*/gt*/engines/CLASS/job_timeout_ms`.
fn job_timeouts(device: &Path) -> Vec<String> {
    const TIMEOUT_FILE: &str = "job_timeout_ms";
    let folders = |folder: &Path, prefix: &str| -> Vec<std::path::PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with(prefix)))
            .collect();
        found.sort();
        found
    };
    let mut timeouts = Vec::new();
    for tile in folders(device, "tile") {
        for gt in folders(&tile, "gt") {
            for class in folders(&gt.join("engines"), "") {
                let Ok(text) = std::fs::read_to_string(class.join(TIMEOUT_FILE)) else { continue };
                let name = class.file_name().unwrap_or_default().to_string_lossy().into_owned();
                timeouts.push(format!("{name} job timeout {} ms", text.trim()));
            }
        }
    }
    timeouts
}

/// What the break steps do, said before the typed yes.
const BREAK_WARNING: &str = "\
The next two steps break things on purpose: a batch that writes to an address nothing is mapped at,
and a batch that never ends. The xe driver should end each within a few seconds and ban only the
queue it ran on. If this card also draws your desktop, the screen may freeze for a moment, and if
the driver can't recover, the desktop may need a restart or the PC a reboot. Save your work first.";

/// Asks for the typed word. Anything else, or no input at all, is a no.
pub fn confirm_break(mut input: &mut dyn BufRead, out: &mut dyn Write) -> bool {
    let _ = writeln!(out, "{BREAK_WARNING}");
    let _ = write!(out, "Type {BREAK_WORD} to run them: ");
    let _ = out.flush();
    let answer = guesthouse_basics::capped_line::read_capped_line(&mut input, 64).ok().flatten();
    answer.is_some_and(|word| word.trim() == BREAK_WORD)
}

/// Runs the test. `sysfs` is the card's device folder, for the job timeouts. The break steps need
/// both the option and the typed word from `input`.
pub fn run(
    xe: &mut dyn Xe,
    card: &CardFacts,
    sysfs: Option<&Path>,
    options: Options,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Summary {
    let mut run = Run {
        xe,
        card,
        out,
        summary: Summary::default(),
        vm: None,
        binds: None,
        bind_point: 0,
        done: None,
        done_point: 0,
        system: None,
        vram: None,
        batch: None,
        host: None,
        system_at: None,
        vram_at: None,
        host_low: false,
        host_high: false,
        batch_bound: false,
        queues: Vec::new(),
        extra_vms: Vec::new(),
    };
    run.step("open", |run| run.open(sysfs));
    run.step("vm", |run| run.make_vm().into());
    run.step("buffers", Run::buffers);
    run.step("userptr", |run| run.host_pages().into());
    run.step("binds", Run::binds);
    for engine in plan::one_engine_per_class(&card.engines) {
        let name = Engine { class: engine.class, instance: engine.instance, gt: engine.gt }.class_name();
        run.step(&format!("engine {name}{}", engine.instance), |run| run.engine(engine));
    }
    run.step("copy", Run::copy);
    run.step("clock", Run::clock);
    run.step("guest registers", Run::guest_registers);
    run.step("render cache keying", Run::cache_keying);
    if options.break_steps {
        if confirm_break(input, run.out) {
            run.step("fault", Run::fault);
            run.step("hang", Run::hang);
        } else {
            let _ = writeln!(run.out, "Not confirmed, so the fault and hang steps are left out.");
        }
    }
    run.step("cleanup", Run::clean_up);
    let summary = std::mem::take(&mut run.summary);
    let _ = writeln!(
        run.out,
        "{} passed, {} failed, {} skipped",
        summary.passed, summary.failed, summary.skipped
    );
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card;
    use crate::xe::abi;
    use crate::xe::fake::{Call, FakeXe};

    fn run_on(xe: &FakeXe, card: &CardFacts, options: Options, typed: &str) -> (Summary, String) {
        let mut out = Vec::new();
        let mut xe = xe.clone();
        let summary = run(&mut xe, card, None, options, &mut typed.as_bytes(), &mut out);
        (summary, String::from_utf8(out).unwrap())
    }

    #[test]
    fn every_step_passes_on_a_card_with_memory_of_its_own_and_nothing_is_left() {
        let xe = FakeXe::default();
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        assert!(summary.all_passed() && summary.skipped == 0, "{said}");
        for step in ["open", "vm", "buffers", "userptr", "binds", "copy", "clock", "cleanup"] {
            assert!(said.lines().any(|line| line.starts_with(&format!("PASS {step} "))), "{step}: {said}");
        }
        for engine in ["rcs0", "bcs0", "vcs0", "ccs0"] {
            assert!(said.contains(&format!("PASS engine {engine} ")), "{engine}: {said}");
        }
        assert!(said.contains("host pages at 0x7fffffe00000"), "{said}");
        assert!(said.ends_with(&format!("{} passed, 0 failed, 0 skipped\n", summary.passed)));
        assert_eq!(xe.live(), 0, "everything made was freed");
        assert!(!said.contains("fault") || said.contains("no fault query"), "no break step by default");
    }

    #[test]
    fn only_tiny_things_are_made_and_no_address_space_has_a_scratch_page() {
        let xe = FakeXe::default();
        run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        for call in xe.calls() {
            match call {
                Call::CreateBuffer(buffer) => assert!(buffer.size <= BUFFER_BYTES, "{buffer:?}"),
                Call::CreateVm { flags } => assert_eq!(flags & abi::VM_SCRATCH_PAGE, 0),
                Call::Bind { op, .. } => assert!(op.length <= HOST_BYTES, "{op:?}"),
                _ => {}
            }
        }
        assert_eq!(xe.calls_named("create_buffer").len(), 3);
    }

    #[test]
    fn a_card_without_memory_of_its_own_passes_without_the_card_buffer() {
        let xe = FakeXe::default();
        xe.set_names_faults(true);
        let (summary, said) = run_on(&xe, &card::tests::lunar_lake(), Options::default(), "");
        assert!(summary.all_passed(), "{said}");
        assert!(said.contains("no card memory on this card"), "{said}");
        assert!(said.contains("fault addresses can be asked"), "{said}");
        assert_eq!(xe.calls_named("create_buffer").len(), 2);
        assert_eq!(xe.live(), 0);
    }

    #[test]
    fn a_failed_address_space_skips_what_needs_it_and_the_rest_still_runs() {
        let xe = FakeXe::default();
        xe.fail("create_vm", libc::EACCES);
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        assert!(said.contains("FAIL vm: making an address space:"), "{said}");
        assert!(said.contains("no permission on the render node"), "{said}");
        assert!(said.contains("PASS buffers"), "{said}");
        assert!(said.contains("SKIP binds: needs the address space"), "{said}");
        assert!(said.contains("SKIP engine rcs0: "), "{said}");
        assert!(said.contains("SKIP copy: "), "{said}");
        assert!(said.contains("PASS clock"), "{said}");
        assert_eq!(summary.failed, 1);
        assert_eq!(xe.live(), 0);
    }

    #[test]
    fn each_register_the_guest_sets_is_set_as_it_does_and_read_before_and_after() {
        let xe = FakeXe::default();
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        assert!(summary.all_passed(), "{said}");
        let line = said.lines().find(|line| line.starts_with("PASS guest registers")).expect("the step ran");
        assert!(line.contains("0x7004=0x400040 unreadable"), "a register no batch reaches: {line}");
        assert_eq!(line.matches("unreadable").count(), guest_registers::RENDER_WRITES.len(), "{line}");

        let xe = FakeXe::default();
        xe.reach_registers(&[0x7004, 0x221C]);
        let (_, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        let line = said.lines().find(|line| line.starts_with("PASS guest registers")).expect("the step ran");
        assert!(line.contains("0x7004=0x400040 took (0x0 to 0x400040)"), "{line}");
        assert!(line.contains("0x221c=0x1 took (0x0 to 0x1)"), "{line}");
        assert!(line.contains("0x6204=0x20202020 unreadable"), "{line}");
    }

    #[test]
    fn a_render_queue_keyed_by_binding_table_is_made_and_run_where_xe_offers_it() {
        let xe = FakeXe::default();
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        let line = said.lines().find(|line| line.starts_with("PASS render cache keying")).expect("{said}");
        assert!(line.ends_with("0x7304 can't be read from a batch, so its bit isn't seen"), "{line}");
        assert!(summary.cache_keyed);
        let keyed: Vec<Call> = xe
            .calls_named("create_queue")
            .into_iter()
            .filter(|call| {
                matches!(call, Call::CreateQueue { keying: abi::Keying::BindingTableAndIndex, .. })
            })
            .collect();
        assert_eq!(keyed.len(), 1, "the step's own queue alone");
        let Call::CreateQueue { engine, .. } = keyed[0] else { unreachable!() };
        assert_eq!(engine.class, Engine::RENDER);
        assert_eq!(xe.live(), 0);

        // Where a batch can read the register, it is read with the bit xe set.
        let xe = FakeXe::default();
        xe.reach_registers(&[COMMON_SLICE_CHICKEN3]);
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        assert!(
            said.contains("PASS render cache keying")
                && said.contains("0x7304 reads 0x2000, with the bit set")
        );
        assert!(summary.cache_keyed);
    }

    #[test]
    fn a_kernel_without_the_keying_skips_it_and_one_that_refuses_it_fails_it_alone() {
        let xe = FakeXe::default();
        let (summary, said) = run_on(&xe, &card::tests::lunar_lake(), Options::default(), "");
        assert!(said.contains("SKIP render cache keying: this kernel's xe doesn't offer it"), "{said}");
        assert!(!summary.cache_keyed && summary.all_passed());
        assert!(
            !xe.calls().iter().any(|call| matches!(
                call,
                Call::CreateQueue { keying: abi::Keying::BindingTableAndIndex, .. }
            ))
        );

        // The flag says yes, and the kernel turns the property down all the same.
        let xe = FakeXe::default();
        xe.without_keying();
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        assert!(said.contains("FAIL render cache keying: making a queue:"), "{said}");
        assert_eq!(summary.failed_steps, ["render cache keying"], "{said}");
        assert!(!summary.cache_keyed && summary.failed_that_work_needs().is_empty());
        assert_eq!(xe.live(), 0);
    }

    #[test]
    fn a_keyed_queue_whose_register_reads_without_the_bit_still_passes_and_says_what_it_read() {
        let xe = FakeXe::default();
        xe.reach_registers(&[COMMON_SLICE_CHICKEN3]);
        xe.ignore_keying();
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        let line = said.lines().find(|line| line.starts_with("PASS render cache keying")).expect("{said}");
        assert!(line.contains("0x7304 reads 0x0 from a batch, without the bit"), "{line}");
        assert!(summary.cache_keyed);
    }

    #[test]
    fn a_refused_user_fence_fails_each_engine_step_and_says_what_xe_said() {
        let xe = FakeXe::default();
        xe.fail("exec", libc::EOPNOTSUPP);
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        assert!(said.contains("FAIL engine ccs0: sending the batch:"), "{said}");
        assert!(said.contains("this kernel's xe doesn't support it"), "{said}");
        assert_eq!(
            summary.failed, 7,
            "four engines, the copy, the guest registers and the render cache keying: {said}"
        );
        assert!(!summary.cache_keyed);
        assert_eq!(xe.live(), 0);
    }

    #[test]
    fn the_summary_names_the_failed_steps_and_which_of_them_work_needs() {
        let xe = FakeXe::default();
        xe.fail("exec", libc::EOPNOTSUPP);
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        let steps = [
            "engine rcs0",
            "engine bcs0",
            "engine vcs0",
            "engine ccs0",
            "copy",
            "guest registers",
            "render cache keying",
        ];
        let mut in_order: Vec<String> = steps.iter().map(|step| (*step).to_owned()).collect();
        in_order.sort_by_key(|step| said.find(&format!("FAIL {step}:")));
        assert_eq!(summary.failed_steps, in_order, "{said}");
        let needed = summary.failed_that_work_needs();
        assert_eq!(
            needed,
            ["engine rcs0", "engine bcs0", "engine ccs0"],
            "all but the video engine, the copy, the guest registers and the render cache keying"
        );
        assert_eq!(
            summary.failed_that_work_does_without(),
            ["engine vcs0", "copy", "guest registers", "render cache keying"]
        );
    }

    #[test]
    fn a_job_that_faulted_is_reported_as_banned_with_its_engine_and_not_as_an_unwritten_fence() {
        let xe = FakeXe::default();
        xe.fault_runs();
        let (summary, said) = run_on(&xe, &card::tests::battlemage(), Options::default(), "");
        for engine in ["rcs0", "bcs0", "ccs0"] {
            let line = said.lines().find(|line| line.contains(&format!("FAIL engine {engine}:"))).unwrap();
            assert!(line.contains(&format!("xe banned the queue on {engine}: its job faulted")), "{line}");
            assert!(!line.contains("did not write the fence"), "{line}");
        }
        assert!(summary.failed_that_work_needs().contains(&"engine ccs0".to_owned()), "the step still fails");
    }

    #[test]
    fn work_needs_the_memory_and_the_render_compute_and_copy_engines_only() {
        for step in ["vm", "buffers", "userptr", "binds", "engine rcs0", "engine ccs0", "engine bcs0"] {
            assert!(work_needs(step), "{step}");
        }
        for step in [
            "open",
            "engine vcs0",
            "engine vcs1",
            "engine vecs0",
            "copy",
            "clock",
            "cleanup",
            "fault",
            "hang",
        ] {
            assert!(!work_needs(step), "{step}");
        }
    }

    #[test]
    fn nothing_is_left_behind_whichever_call_fails() {
        let calls = {
            let xe = FakeXe::default();
            run_on(&xe, &card::tests::battlemage(), Options { break_steps: true }, "yes\n");
            xe.calls().len()
        };
        assert!(calls > 40, "the run makes this many calls: {calls}");
        for index in 0..calls {
            let xe = FakeXe::default();
            xe.fail_call(index, libc::EIO);
            let (_, said) = run_on(&xe, &card::tests::battlemage(), Options { break_steps: true }, "yes\n");
            // The one failure that can't be made up for is of a free in the cleanup itself, and
            // that is said.
            let failed_call = xe.calls().get(index).map(Call::name).unwrap_or_default();
            assert!(
                xe.live() == 0 || said.contains("FAIL cleanup"),
                "call {index} ({failed_call}) left something unsaid: {said}"
            );
        }
    }

    #[test]
    fn the_break_steps_need_the_option_and_the_typed_word() {
        let card = card::tests::battlemage();
        for (options, typed, runs) in [
            (Options::default(), "yes\n", false),
            (Options { break_steps: true }, "", false),
            (Options { break_steps: true }, "y\n", false),
            (Options { break_steps: true }, "Yes please\n", false),
            (Options { break_steps: true }, "yes\n", true),
        ] {
            let xe = FakeXe::default();
            let (summary, said) = run_on(&xe, &card, options, typed);
            assert_eq!(said.contains("PASS fault"), runs, "{options:?} {typed:?}: {said}");
            assert_eq!(said.contains("PASS hang"), runs, "{options:?} {typed:?}: {said}");
            assert_eq!(xe.faults().len(), usize::from(runs), "{options:?} {typed:?}");
            assert_eq!(said.contains("break things on purpose"), options.break_steps);
            assert!(summary.all_passed(), "{said}");
            assert_eq!(xe.live(), 0);
        }
    }

    #[test]
    fn a_recorded_card_is_refused_in_plain_words() {
        let refusal = recorded_card_refusal(Path::new("/lab/arc-b580.card"));
        assert!(refusal.contains("/lab/arc-b580.card") && refusal.contains("nothing is run"), "{refusal}");
    }

    #[test]
    fn job_timeouts_are_read_from_the_cards_engine_folders() {
        let device = std::env::temp_dir().join(format!("guesthouse-intel-timeouts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&device);
        for (class, timeout) in [("rcs", "5000"), ("ccs", "10000")] {
            let folder = device.join("tile0/gt0/engines").join(class);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("job_timeout_ms"), format!("{timeout}\n")).unwrap();
        }
        assert_eq!(job_timeouts(&device), ["ccs job timeout 10000 ms", "rcs job timeout 5000 ms"]);
        assert!(job_timeouts(&device.join("missing")).is_empty());
        std::fs::remove_dir_all(&device).unwrap();
    }
}
