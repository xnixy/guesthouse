//! The Intel backend: the host's open xe driver, and the private data Intel's Windows driver passes
//! to its kernel driver.
//!
//! This is a first stage, a stand-in that listens. It reads the card through sysfs and xe's
//! read-only queries, builds the adapter record the guest's user-mode drivers ask for, and is meant
//! to answer the guest so that they can make devices, contexts, queues and allocations and submit.
//! None of it has met a guest yet. Nothing reaches the card: a submission comes back finished, unrun, and video memory is
//! only a name, as nothing writes it. What it is for is the log: every exchange that is Intel's own
//! gets a line, in [`traffic`], so one guest run says what the real backend has to do.
//!
//! The guest-protocol side, in [`protocol`], is plain byte work tested without a guest. The host
//! side is [`card`] and the modules under it, and `intel-probe` prints what it reads.
//!
//! Where the trait does not fit Intel:
//! - A submission's batch address and length are in the submission message, not in Intel's private
//!   data, and [`Work`] doesn't carry them. A backend that runs work will need them.
//! - Escapes have no status of their own, so one this backend's rules refuse can only be logged.
//! - [`Backend::finish_queue_data`] sees the queue's data with the message's padding after it, so
//!   its true size can't be checked.
//! - The core describes every node but the first as a copy engine, whatever gmmlib's numbering
//!   says the node is.

mod books;
pub mod card;
pub mod generation;
pub mod hwconfig;
pub mod ioctl;
pub mod protocol;
pub mod sysfs;
mod traffic;
mod watch;
pub mod xe_query;

use books::Books;
use card::CardFacts;
use generation::Generation;
use guesthouse_backend::guest_text::QUERY_USER_DRIVER_NAME;
use guesthouse_backend::{
    AdapterFacts, AllocationShape, Backend, BackendError, CardLabel, DriverFound, DriverSurvey, GpuRange,
    GuestBooks, HostServices, MapRequest, MemoryId, MemoryKind, PciCard, Refusal, RegistryName,
    RegistryValue, Setup, SpaceId, StagingCopy, StagingStart, Status, UnmapRequest, VendorQuery, Work,
    WorkStart,
};
pub use ioctl::{INTEL_VENDOR, intel_render_node};
use protocol::allocation::Resource;
use protocol::command_header::Header;
use protocol::context::Context;
use protocol::escape::Escape;
use protocol::record::{GtSystem, IpWord, Memory, Platform, RecordFacts};
use protocol::{gaps, guest_driver, record, shape};
use std::time::Instant;
use traffic::{QueryOutcome, Traffic};
use watch::PREFIX;
use xe_query::{Engine, Gt};

/// How this backend names the host driver's branch: the Linux kernel's xe.
const HOST_BRANCH: &str = "xe";
/// The part of the system's memory offered as video memory, as Windows gives an integrated card
/// half of the memory as shared.
const SHARED_MEMORY_PART: u64 = 2;

/// See the crate documentation.
#[derive(Debug)]
pub struct IntelBackend {
    card: CardFacts,
    generation: &'static Generation,
    facts: AdapterFacts,
    record: Vec<u8>,
    books: Books,
    traffic: Traffic,
    started: Instant,
    /// See [`IntelBackend::set_software_desktop`].
    software_desktop: bool,
}

impl IntelBackend {
    /// The backend for a card whose facts are already read, which is how a test makes one. Refuses
    /// a card whose graphics has no profile.
    pub fn from_card(card: CardFacts) -> Result<Self, Refusal> {
        let ip = card.main_gt().map(|gt| gt.ip).unwrap_or_default();
        let generation = generation::for_render_ip(ip).ok_or_else(|| {
            Refusal(format!("This Intel card's graphics IP is {ip}, which this build has no profile for."))
        })?;
        // compute-runtime works out threads per EU by dividing the thread count by the EU count, so a
        // record with none would stop it at the start, or worse.
        if card.eu_count() == 0 {
            return Err(Refusal(
                "The xe driver reported no EUs for this Intel card, so the guest can't be told its shape."
                    .into(),
            ));
        }
        let shared = card.system_memory() / SHARED_MEMORY_PART;
        let record = record::build(&record_facts(&card, generation, shared));
        let facts = AdapterFacts {
            card: PciCard { vendor: INTEL_VENDOR, device: card.pci.device, revision: card.pci.revision },
            name: format!("Intel GPU {:#06x}", card.pci.device),
            video_memory: shared,
            gtt_memory: 0,
            bar_size: shared,
            node_count: protocol::nodes::COUNT,
            engines: card.engine_families(),
            guest_driver_version: 0,
        };
        let backend = Self {
            card,
            generation,
            facts,
            record,
            books: Books::default(),
            traffic: Traffic::default(),
            started: Instant::now(),
            software_desktop: false,
        };
        backend.log_start();
        Ok(backend)
    }

    /// A lab switch, off unless set: the guest is refused the drivers for Direct3D 9, 10 and 11, so
    /// that the desktop composes in software and stays usable, while Direct3D 12, compute and
    /// Vulkan still come here. Nothing has tried it on a guest.
    pub fn set_software_desktop(&mut self, on: bool) {
        self.software_desktop = on;
        let state = if on { "on" } else { "off" };
        log::info!(
            "{PREFIX} software-desktop {state}: Direct3D 9, 10 and 11 drivers {}",
            if on { "refused" } else { "given" }
        );
    }

    pub fn card(&self) -> &CardFacts {
        &self.card
    }

    pub fn generation(&self) -> &'static Generation {
        self.generation
    }

    /// Blocks of memory still held, for a test to see that frees reach the backend.
    pub fn live_memory_blocks(&self) -> usize {
        self.books.live_memory()
    }

    /// The adapter record the guest is given, before its build stamp is put in.
    pub fn record(&self) -> &[u8] {
        &self.record
    }

    fn log_start(&self) {
        let card = &self.card;
        log::info!(
            "{PREFIX} card pci={:04x}:{:04x} rev={:#04x} subsystem={:04x}:{:04x} address={} driver={} kernel={} \
             profile=\"{}\" render_ip={} eus={} system_memory={:#x}",
            card.pci.vendor,
            card.pci.device,
            card.pci.revision,
            card.pci.subsystem_vendor,
            card.pci.subsystem_device,
            card.address.as_deref().unwrap_or("-"),
            card.driver,
            card.kernel.as_deref().unwrap_or("-"),
            self.generation.name,
            card.main_gt().map(|gt| gt.ip).unwrap_or_default(),
            card.eu_count(),
            card.system_memory()
        );
        for (what, error) in &card.unanswered {
            log::warn!("{PREFIX} card query=\"{what}\" unanswered=\"{error}\"");
        }
        log::info!("{PREFIX} record {}", record::summary(&self.record));
        log::info!("{PREFIX} record-partition [{}]", protocol::partition::Text(protocol::partition::ours()));
        for gap in gaps::unset(shape::filled(&self.record)) {
            log::info!("{PREFIX} record-gap \"{gap}\"");
        }
        log::warn!("{PREFIX} this stage runs nothing on the card: the guest's work is finished unrun");
    }
}

/// What the record is built from: the card where it can say, and the generation where it can't.
fn record_facts(card: &CardFacts, generation: &Generation, shared: u64) -> RecordFacts {
    let ip = |gt: Option<&Gt>| {
        gt.map_or(IpWord::default(), |gt| IpWord::new(gt.ip.major, gt.ip.minor, gt.ip.revision))
    };
    let config = |key| card.hwconfig.value(key).unwrap_or(0);
    let threads_per_eu =
        card.hwconfig.value(hwconfig::NUM_THREADS_PER_EU).unwrap_or(generation.threads_per_eu);
    let most_dual_subslices = config(hwconfig::MAX_DUAL_SUBSLICES_SUPPORTED);
    // The counts are of the cores the slice arrays can hold, so the arrays and the counts agree.
    let cores = shape::representable(&card.cores(), config(hwconfig::MAX_SLICES_SUPPORTED));
    let (eus, dual_subslices, slices) = if cores.is_empty() {
        (card.eu_count(), card.dual_subslices(), card.slices().unwrap_or(0))
    } else {
        let eus = cores.iter().fold(0u32, |sum, core| sum.saturating_add(core.eu_count));
        (eus, cores.len() as u32, shape::slices_in(&cores))
    };
    RecordFacts {
        platform: Platform {
            product_family: generation.product_family,
            pch_family: generation.pch_family,
            display_core: generation.display_core,
            render_core: generation.render_core,
            platform_type: generation.platform_type,
            device_id: card.pci.device,
            revision: card.pci.revision.into(),
            gt_type: generation.gt_type,
            render_ip: ip(card.main_gt()),
        },
        gt: GtSystem {
            eu_count: eus,
            thread_count: eus.saturating_mul(threads_per_eu),
            slice_count: slices,
            subslice_count: dual_subslices,
            dual_subslice_count: dual_subslices,
            max_eu_per_subslice: config(hwconfig::MAX_NUM_EU_PER_DSS),
            max_slices: config(hwconfig::MAX_SLICES_SUPPORTED),
            max_subslices: most_dual_subslices,
            max_dual_subslices: most_dual_subslices,
        },
        timestamp_hz: card.main_gt().map_or(0, |gt| gt.reference_clock),
        render_mhz: (card.frequencies.lowest.unwrap_or(0), card.frequencies.highest.unwrap_or(0)),
        memory: Memory { dedicated: 0, system_shared: shared, system_video: 0 },
        local_memory: card.config.has_vram(),
        cores,
        compute_engines: card.engines_of(Engine::COMPUTE),
        copy_engine: card.engines_of(Engine::COPY) > 0,
        address_bits: card.config.va_bits,
    }
}

impl Backend for IntelBackend {
    fn supports(card: &PciCard) -> bool {
        card.vendor == INTEL_VENDOR
    }

    /// The host is the Linux kernel's xe and the guest one release of Intel's driver. xe has no
    /// branches of its own: the host's side of that release's data is this backend, written for it,
    /// so the host is said to be on the guest release's branch. That is all that lets the pair
    /// through, and only for the release this backend knows. Nothing has been tested yet, so a start
    /// always warns.
    fn survey(setup: &Setup) -> Result<DriverSurvey, Refusal> {
        let version = setup.guest_driver_version.to_string();
        let known = guest_driver::GUEST_RELEASES.contains(&version.as_str());
        let kernel = sysfs::kernel_release().unwrap_or_else(|| "kernel".into());
        let host = format!("{HOST_BRANCH} {kernel}");
        let branch = if known { setup.guest_branch.clone() } else { HOST_BRANCH.into() };
        Ok(DriverSurvey {
            host: DriverFound { version: host, branch },
            guest: DriverFound { version, branch: setup.guest_branch.clone() },
            host_branch_known: true,
            guest_branch_known: known,
            host_version_tested: false,
            mixed_branches_tested: false,
            card: CardLabel::Experimental,
        })
    }

    fn open(setup: &Setup, _survey: &DriverSurvey) -> Result<Self, Refusal> {
        let address = setup.card_address.map(|address| address.to_string());
        let node = intel_render_node(address.as_deref()).ok_or_else(|| {
            Refusal("No Intel card on the xe driver was found at the address this guest was given.".into())
        })?;
        let card = CardFacts::read(&node)
            .map_err(|error| Refusal(format!("Opening the Intel card failed: {error}")))?;
        let mut backend = Self::from_card(card)?;
        backend.facts.guest_driver_version = setup.guest_driver_version.packed();
        Ok(backend)
    }

    fn facts(&self) -> &AdapterFacts {
        &self.facts
    }

    fn allocate_video_memory(
        &mut self,
        size: u64,
        _kind: MemoryKind,
    ) -> Result<(MemoryId, u64), BackendError> {
        Ok((self.books.new_memory(), size))
    }

    fn adopt_host_pages(&mut self, _host_address: u64, _size: u64) -> Result<MemoryId, BackendError> {
        Ok(self.books.new_memory())
    }

    fn release_memory(&mut self, memory: MemoryId) {
        self.books.release(memory);
    }

    /// Nothing has written the video memory, so there is nothing to move.
    fn preserve_contents(
        &mut self,
        _host: &mut dyn HostServices,
        _own_space: SpaceId,
        _from: MemoryId,
        _to: MemoryId,
        _size: u64,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn create_address_space(&mut self, ranges: &[GpuRange]) -> Result<SpaceId, BackendError> {
        Ok(self.books.create_space(ranges))
    }

    fn destroy_address_space(&mut self, space: SpaceId) {
        self.books.destroy_space(space);
    }

    fn map(&mut self, request: &MapRequest) -> Result<(), BackendError> {
        self.books.map(request)
    }

    fn unmap(&mut self, unmap: &UnmapRequest) -> Result<(), BackendError> {
        self.books.unmap(unmap)
    }

    /// Data this backend refuses gets a page, as the call fails anyway once
    /// [`Backend::allocation_data_valid`] says so.
    fn describe_allocation(&mut self, private: &[u8]) -> AllocationShape {
        match Resource::parse(private) {
            Ok(resource) => {
                self.traffic.allocation(private, &resource);
                if !resource.size_is_sane() {
                    self.traffic.refused(
                        "allocation-size",
                        "past every GPU address, given one page",
                        private,
                    );
                }
                resource.shape()
            }
            Err(_) => Resource::default().shape(),
        }
    }

    fn allocation_data_valid(&mut self, private: &[u8]) -> bool {
        let refused = Resource::parse(private).err();
        if let Some(refused) = refused {
            self.traffic.refused("allocation", refused.0, private);
        }
        refused.is_none()
    }

    /// Nothing is written back into an allocation's data.
    fn finish_allocation_data(&mut self, _private: &mut [u8], _size: u64) {}

    fn finish_queue_data(&mut self, queue: u32, private: &mut [u8], serial: u32) -> Result<(), BackendError> {
        self.traffic.queue(queue, serial, private);
        Ok(())
    }

    fn adapter_opened(&mut self, guest_adapter_id: u64) {
        log::info!("{PREFIX} adapter-opened guest_id={guest_adapter_id:#x}");
    }

    fn guest_restarted(&mut self) {
        log::info!("{PREFIX} guest-restarted");
    }

    /// A partition escape asking for one gets the record's. Every other is answered by leaving the
    /// guest's buffer as it came.
    fn escape(&mut self, process: u32, data: &mut [u8], _books: &GuestBooks<'_>) {
        let escape = Escape::parse(data);
        let partition = protocol::escape::answer_partition(data, &protocol::partition::ours());
        self.traffic.escape(process, data, &escape, partition);
    }

    fn adapter_answer(&mut self, query: &VendorQuery<'_>, out: &mut Vec<u8>) -> Option<Status> {
        let outcome = match query.kind {
            record::QUERY_TYPE => {
                let (status, asked) = record::answer(&self.record, query, out);
                QueryOutcome::Record(status, asked)
            }
            guest_driver::QUERY_OPENGL => {
                let driver = guest_driver::write_opengl_info(query, out);
                QueryOutcome::OpenGl(Status::SUCCESS, driver)
            }
            QUERY_USER_DRIVER_NAME => {
                let (status, name) = guest_driver::write_name(query, self.software_desktop, out);
                QueryOutcome::DriverName(status, name)
            }
            _ => QueryOutcome::LeftToTheCore,
        };
        self.traffic.query(query, outcome);
        match outcome {
            QueryOutcome::Record(status, _)
            | QueryOutcome::DriverName(status, _)
            | QueryOutcome::OpenGl(status, _) => Some(status),
            QueryOutcome::LeftToTheCore => None,
        }
    }

    fn registry_value(&mut self, name: &RegistryName) -> Option<RegistryValue> {
        let value = guest_driver::registry(name);
        self.traffic.registry(name, value.is_some());
        value
    }

    /// The host's clock stands in for the GPU's, which nothing reads here, in nanoseconds since
    /// the backend started.
    fn gpu_clock(&mut self) -> Result<u64, BackendError> {
        Ok(self.started.elapsed().as_nanos() as u64)
    }

    fn context_allowed(&mut self, node: u32, client_hint: u32) -> bool {
        let allowed = protocol::nodes::offered(node);
        if !allowed {
            self.traffic.context_refused(node, client_hint);
        }
        allowed
    }

    fn context_data_valid(&mut self, process: u32, node: u32, private: &[u8]) -> bool {
        match Context::parse(private) {
            Ok(context) => {
                self.traffic.context(process, node, private, &context);
                true
            }
            Err(refused) => {
                self.traffic.refused("context", refused.0, private);
                false
            }
        }
    }

    fn context_gone(&mut self, process: u32, node: u32) {
        log::debug!("{PREFIX} context-gone process={process:#x} node={node}");
    }

    fn process_gone(&mut self, process: u32) {
        log::debug!("{PREFIX} process-gone process={process:#x}");
    }

    /// Nothing runs in this stage, so no submission fails: each is logged with what its header
    /// says, as far as it can be read, and finished unrun.
    fn start_work(&mut self, _host: &mut dyn HostServices, work: &Work<'_>) -> WorkStart {
        self.traffic.submission(work, &Header::read(work.private));
        WorkStart::Finished
    }

    fn ring_doorbell(&mut self, _queue: u32, _doorbell: u64) -> Result<(), BackendError> {
        Ok(())
    }

    fn close_queue(&mut self, queue: u32) {
        self.traffic.queue_closed(queue);
    }

    /// What would be copied was never drawn, so the copy is done by leaving the target as it is.
    fn start_staging_copy(&mut self, _host: &mut dyn HostServices, copy: &StagingCopy) -> StagingStart {
        self.traffic.copy(if copy.is_screen { "screen" } else { "staging" });
        StagingStart::Finished(Status::SUCCESS)
    }

    fn poll_staging_copy(&mut self, _host: &mut dyn HostServices, _now: Instant) -> Option<Status> {
        Some(Status::SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::{DriverVersion, StartDecision, decide_start};

    fn setup_for(version: &str) -> Setup {
        Setup {
            guest_driver_version: DriverVersion::parse(version).unwrap(),
            guest_branch: version.into(),
            driver_folder: Default::default(),
            card: None,
            card_address: None,
        }
    }

    #[test]
    fn the_known_guest_release_starts_with_a_warning_and_another_is_refused() {
        let survey = IntelBackend::survey(&setup_for(guest_driver::GUEST_RELEASES[0])).unwrap();
        assert!(matches!(decide_start(&survey), StartDecision::RunWithWarning(_)));
        assert_eq!(survey.card, CardLabel::Experimental);
        assert!(!survey.mixed_branches_tested && !survey.host_version_tested, "nothing is claimed tested");
        let survey = IntelBackend::survey(&setup_for("31.0.101.5000")).unwrap();
        assert!(matches!(decide_start(&survey), StartDecision::Refuse(_)));
    }

    #[test]
    fn the_facts_come_from_the_card() {
        let backend = IntelBackend::from_card(card::tests::lunar_lake()).unwrap();
        let facts = backend.facts();
        assert_eq!((facts.card.vendor, facts.card.device, facts.card.revision), (INTEL_VENDOR, 0x64A0, 4));
        assert_eq!(facts.video_memory, 16 << 30, "half the system memory");
        assert!(facts.engines.graphics && facts.engines.compute);
        assert_eq!(facts.node_count, 15);
    }

    #[test]
    fn the_record_is_built_from_the_card_and_its_generation() {
        let backend = IntelBackend::from_card(card::tests::lunar_lake()).unwrap();
        let summary = record::summary(backend.record());
        assert_eq!((summary.product_family, summary.render_core, summary.device_id), (1275, 0xC09, 0x64A0));
        assert_eq!(summary.render_ip, IpWord::new(20, 4, 4).0);
        assert_eq!((summary.eu_count, summary.virtual_machine, summary.compression), (64, 1, false));
    }

    #[test]
    fn a_card_with_no_eus_is_refused_in_plain_words() {
        let mut card = card::tests::lunar_lake();
        card.topology.clear();
        let refusal = IntelBackend::from_card(card).unwrap_err();
        assert!(refusal.0.contains("no EUs"), "{}", refusal.0);
    }

    #[test]
    fn a_card_with_no_profile_is_refused_in_plain_words() {
        let mut card = card::tests::lunar_lake();
        card.gts[0].ip.minor = 1;
        let refusal = IntelBackend::from_card(card).unwrap_err();
        assert!(refusal.0.contains("no profile"), "{}", refusal.0);
    }

    #[test]
    fn the_thread_count_falls_back_to_the_generations_when_hwconfig_has_none() {
        let mut card = card::tests::lunar_lake();
        card.hwconfig = hwconfig::Hwconfig::default();
        let facts = record_facts(&card, &generation::XE2_LPG, 0);
        assert_eq!(facts.gt.thread_count, 64 * generation::XE2_LPG.threads_per_eu);
        assert_eq!(facts.gt.max_slices, 0, "not guessed");
    }
}
