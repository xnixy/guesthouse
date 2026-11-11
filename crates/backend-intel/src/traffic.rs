//! One log line for each exchange with the guest's driver, in a fixed form a tester's log can be
//! searched by: `intel <kind> name=value ...`.
//!
//! The kinds are `query`, `registry`, `escape`, `partition`, `context`, `queue`, `allocation`,
//! `submit`, `queue-closed`, `copy` and `refused`. Each line names the fields decoded, then
//! `undecoded=` with the non-zero words nothing names yet. The first few of each kind are at info,
//! told apart by the guest's process where the trait passes one (escapes and contexts) and by its
//! address space for submissions, so a second program's traffic isn't hidden behind the first's.
//! Queries, registry values, queues and allocations come with no process, so those are told apart
//! by what they say alone. The repeats are at debug, and with `GUESTHOUSE_VERBOSE` every buffer is
//! dumped whole as well, up to 4 KiB.

use crate::protocol::allocation::{self, Resource};
use crate::protocol::command_header::{self, Compact, Header};
use crate::protocol::context::{self, Context};
use crate::protocol::escape::{self, Escape, PartitionAsk};
use crate::protocol::guest_driver::NameAnswer;
use crate::protocol::hw_queue::{self, Queue};
use crate::protocol::nodes;
use crate::protocol::partition;
use crate::protocol::record::Asked;
use crate::watch::{Hex, PREFIX, Seen, Undecoded, dump, tell};
use guesthouse_backend::{RegistryName, Status, VendorQuery, Work, WorkSource};
use std::collections::HashMap;
use std::fmt;

/// How many bytes of an escape's payload a line shows.
const ESCAPE_BYTES_SHOWN: usize = 32;

/// What became of an adapter query the vendor was offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryOutcome {
    Record(Status, Asked),
    DriverName(Status, NameAnswer),
    /// Not one of this backend's, so the core answers it or says it can't.
    LeftToTheCore,
}

impl fmt::Display for QueryOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(_, Asked::Count) => f.write_str("answered=adapter-count"),
            Self::Record(_, Asked::Record { build_stamp }) => {
                write!(f, "answered=record guest_build={build_stamp:#x}")
            }
            Self::Record(status, Asked::Refused(why)) => write!(f, "refused={:#x} why=\"{why}\"", status.0),
            Self::DriverName(_, NameAnswer::Given(name)) => write!(f, "answered=driver-name driver={name}"),
            Self::DriverName(status, NameAnswer::No32BitName) => {
                write!(f, "refused={:#x} why=\"no 32-bit driver name is known\"", status.0)
            }
            Self::DriverName(status, NameAnswer::Withheld) => {
                write!(f, "refused={:#x} why=\"held back by the software desktop switch\"", status.0)
            }
            Self::LeftToTheCore => f.write_str("answered=left-to-the-core"),
        }
    }
}

impl QueryOutcome {
    /// What tells one outcome from another for [`Seen`], leaving out the guest's build stamp.
    fn kind(&self) -> (u32, Option<Asked>, Option<NameAnswer>) {
        match *self {
            Self::Record(status, Asked::Record { .. }) => {
                (status.0, Some(Asked::Record { build_stamp: 0 }), None)
            }
            Self::Record(status, asked) => (status.0, Some(asked), None),
            Self::DriverName(status, name) => (status.0, None, Some(name)),
            Self::LeftToTheCore => (0, None, None),
        }
    }
}

/// What has been seen of each kind, and the submissions on each queue.
#[derive(Debug, Default)]
pub struct Traffic {
    queries: Seen,
    registry: Seen,
    escapes: Seen,
    contexts: Seen,
    queues: Seen,
    allocations: Seen,
    submissions: Seen,
    refusals: Seen,
    copies: Seen,
    submitted: HashMap<u32, u64>,
}

/// A bool as the lines write it.
fn bit(value: bool) -> u8 {
    value.into()
}

impl Traffic {
    pub fn query(&mut self, query: &VendorQuery<'_>, outcome: QueryOutcome) {
        let telling =
            self.queries.worth_telling((query.kind, query.size, query.process_is_32_bit, outcome.kind()));
        tell!(
            telling,
            "{PREFIX} query kind={} size={} wow64={} {outcome}",
            query.kind,
            query.size,
            bit(query.process_is_32_bit)
        );
        dump(format_args!("query kind={}", query.kind), query.data);
    }

    pub fn registry(&mut self, name: &RegistryName, found: bool) {
        let shown = match name {
            RegistryName::DriverStorePath => "<driver store path>",
            RegistryName::DriverImagePath => "<driver image path>",
            RegistryName::Value(name) => name.as_str(),
        };
        let telling = self.registry.worth_telling((shown, found));
        let answer = if found { "answered" } else { "not-ours" };
        tell!(telling, "{PREFIX} registry name=\"{shown}\" {answer}");
    }

    pub fn escape(&mut self, process: u32, data: &[u8], escape: &Escape, partition: Option<PartitionAsk>) {
        let kind =
            (process, escape.code, escape.minor_code, escape.gmm_action, escape.length, escape.verdict);
        let telling = self.escapes.worth_telling(kind);
        let action = DisplayOption(escape.gmm_action.map(|action| HexNumber(action.into())));
        let payload = data.get(escape::HEADER_SIZE..).unwrap_or_default();
        tell!(
            telling,
            "{PREFIX} escape process={process:#x} length={} payload_size={} checksum={:#x} code={:#x} minor={:#x} \
             gmm_action={action} kernel_would={} why=\"{}\" answer={} payload={}",
            escape.length,
            escape.payload_size,
            escape.checksum,
            escape.code,
            escape.minor_code,
            escape.verdict.word(),
            escape.verdict.why(),
            match partition {
                Some(PartitionAsk::Get) => "partition",
                _ => "echoed",
            },
            Hex { bytes: payload, most: ESCAPE_BYTES_SHOWN }
        );
        dump(format_args!("escape code={:#x}", escape.code), data);
        match partition {
            Some(PartitionAsk::Get) => log::info!(
                "{PREFIX} partition process={process:#x} ask=get answered=[{}]",
                partition::Text(partition::read(&data[escape::HEADER_SIZE + 8..]))
            ),
            Some(PartitionAsk::Set(theirs)) => {
                log::info!(
                    "{PREFIX} partition process={process:#x} ask=set kept=[{}]",
                    partition::Text(theirs)
                )
            }
            None => {}
        }
    }

    pub fn context(&mut self, process: u32, node: u32, private: &[u8], context: &Context) {
        let kind =
            (process, node, private.len(), context.umd_context_type, context.dwm, context.engine_instances);
        let telling = self.contexts.worth_telling(kind);
        tell!(
            telling,
            "{PREFIX} context process={process:#x} node={node} node_name={} size={} type={} guest_pid={} dwm={} protected={} \
             media={} gpu_va={} id_pointer={:#x} ids={} instances={:#x} power_hint={} no_ring_flushes={} \
             queues={} wmtp_off={} preempt_notify={} preempt_event={:#x} token64={} sip={} undecoded=[{}]",
            nodes::name(node),
            private.len(),
            context.umd_context_type,
            context.process_id,
            bit(context.dwm),
            bit(context.protected_process),
            bit(context.media_usage),
            bit(context.gpu_va_context),
            context.hw_context_id_pointer,
            context.hw_context_id_count,
            context.engine_instances,
            DisplayOption(context.power_hint),
            bit(context.no_ring_flushes),
            context.hw_queue_count,
            bit(context.disable_wmtp),
            bit(context.notify_preempt_exceeded),
            context.preempt_event,
            bit(context.use_64_bit_token),
            bit(context.debug_sip_installed),
            Undecoded { bytes: private, decoded: &context::DECODED }
        );
        dump(format_args!("context node={node}"), private);
    }

    /// `private` is the guest's data and the padding after it, as the core passes it on.
    pub fn queue(&mut self, queue: u32, serial: u32, private: &[u8]) {
        self.submitted.insert(queue, 0);
        let data = &private[..private.len().min(hw_queue::SIZE)];
        let fields = Queue::parse(data);
        let telling = self.queues.worth_telling((fields, Hashed(data)));
        tell!(
            telling,
            "{PREFIX} queue id={queue:#x} serial={serial} bytes_with_padding={} priority={} written_back={:#x} \
             flag={} answer=unchanged undecoded=[{}]",
            private.len(),
            fields.priority,
            fields.written_back,
            bit(fields.flag),
            Undecoded { bytes: data, decoded: &hw_queue::DECODED }
        );
        dump(format_args!("queue id={queue:#x}"), private);
    }

    pub fn allocation(&mut self, private: &[u8], resource: &Resource) {
        let kind = (
            private.len(),
            resource.resource_type,
            resource.format,
            resource.gpu_flags,
            resource.info_flags,
            resource.usage,
            resource.tile_mode,
            resource.companion.is_some(),
        );
        let telling = self.allocations.worth_telling(kind);
        tell!(
            telling,
            "{PREFIX} allocation size={} type={} format={:#x} bpp={} gpu_flags={:#x} info_flags={:#x} wa_flags={:#x} \
             width={} height={} array={} usage={:#x} pitch={} bytes={:#x} cc_bytes={:#x} unpadded={:#x} \
             reported={:#x} tile_mode={} ccs_align={} client={} guest_build={:#x} compressed={} linear={} \
             companion={} undecoded=[{}]",
            private.len(),
            resource.resource_type,
            resource.format,
            resource.bits_per_pixel,
            resource.gpu_flags,
            resource.info_flags,
            resource.wa_flags,
            resource.width,
            resource.height,
            resource.array_size,
            resource.usage,
            resource.pitch,
            resource.size,
            resource.cc_size,
            resource.unpadded_size,
            resource.size_reported,
            resource.tile_mode,
            resource.ccs_mode_align,
            resource.client_type,
            resource.build_stamp,
            bit(resource.compressed()),
            bit(resource.linear()),
            DisplayOption(resource.companion.map(|companion| {
                CompanionText(companion.marker, companion.size, companion.offset, companion.alignment)
            })),
            Undecoded { bytes: private, decoded: &allocation::DECODED }
        );
        dump(format_args!("allocation type={}", resource.resource_type), private);
    }

    pub fn submission(&mut self, work: &Work<'_>, header: &Header) {
        if let Some(count) = self.submitted.get_mut(&work.queue) {
            *count += 1;
        }
        let kind = (work.space, work.node, work.client_hint, work.source == WorkSource::Context);
        let telling = self.submissions.worth_telling((kind, command_header::kind_of(work.private)));
        // Only a hardware queue's submissions are known to carry this header.
        let kernel_would = match work.source {
            WorkSource::HardwareQueue => header.verdict.word(),
            WorkSource::Context => "unknown",
        };
        let source = match work.source {
            WorkSource::HardwareQueue => "hardware-queue",
            WorkSource::Context => "context",
        };
        let undecoded = header.compact.map(|_| Undecoded {
            bytes: &work.private[..work.private.len().min(command_header::FULL_SIZE)],
            decoded: &command_header::DECODED,
        });
        tell!(
            telling,
            "{PREFIX} submit source={source} queue={:#x} node={} node_name={} hint={:#x} fence_value={} length={} \
             kernel_would={kernel_would} answer=finished-unrun fields=[{}] undecoded=[{}]",
            work.queue,
            work.node,
            nodes::name(work.node),
            work.client_hint,
            work.fence_value,
            header.length,
            DisplayOption(header.compact.as_ref().map(CompactText)),
            DisplayOption(undecoded)
        );
        dump(format_args!("submit queue={:#x}", work.queue), work.private);
    }

    /// A context on a node past those offered.
    pub fn context_refused(&mut self, node: u32, client_hint: u32) {
        let telling = self.refusals.worth_telling(("context-node", node, client_hint));
        tell!(
            telling,
            "{PREFIX} refused what=context why=\"node {node} is past those offered\" node_name={} hint={client_hint:#x}",
            nodes::name(node)
        );
    }

    /// How many submissions a queue had, once it has gone.
    pub fn queue_closed(&mut self, queue: u32) {
        let count = self.submitted.remove(&queue).unwrap_or(0);
        log::info!("{PREFIX} queue-closed id={queue:#x} submissions={count}");
    }

    /// Something the kernel driver would refuse, or this backend does.
    pub fn refused(&mut self, what: &str, why: &str, private: &[u8]) {
        let telling = self.refusals.worth_telling((what, why, private.len()));
        tell!(
            telling,
            "{PREFIX} refused what={what} why=\"{why}\" size={} first={}",
            private.len(),
            Hex { bytes: private, most: ESCAPE_BYTES_SHOWN }
        );
        dump(format_args!("refused {what}"), private);
    }

    /// A copy to the screen, which this stage answers as done without copying.
    pub fn copy(&mut self, kind: &'static str) {
        let telling = self.copies.worth_telling(kind);
        tell!(telling, "{PREFIX} copy kind={kind} answer=done-without-copying");
    }
}

/// An option written as its value, or `-`.
struct DisplayOption<T>(Option<T>);

impl<T: fmt::Display> fmt::Display for DisplayOption<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(value) => value.fmt(f),
            None => f.write_str("-"),
        }
    }
}

/// Hashes a buffer's bytes as they are.
struct Hashed<'a>(&'a [u8]);

impl std::hash::Hash for Hashed<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write(self.0);
    }
}

/// A number in hex, as a value a line can hold without making a string.
struct HexNumber(u64);

impl fmt::Display for HexNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// A compact header's fields, by name.
struct CompactText<'a>(&'a Compact);

impl fmt::Display for CompactText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header = self.0;
        write!(
            f,
            "stamp={:#x} type={} patch_list={} first_flags={:#x} requests={:#x}/{:#x} slice_state={} \
             subslices={} eus={} bit23={} flags={:#x} unnamed_flags={:#x} perf_tag={:#x} tag={} monitor_fence={}",
            header.stamp,
            header.context_type,
            bit(header.patch_list),
            header.first_flags,
            header.request_low,
            header.request_high,
            header.slice_state,
            header.subslice_count,
            header.eu_count,
            bit(header.request_bit_23),
            header.flags,
            header.unnamed_flags(),
            header.perf_tag,
            DisplayOption(header.tag.map(|(address, value)| Pair(address, value.into()))),
            DisplayOption(header.monitor_fence.map(|(address, value)| Pair(address, value)))
        )
    }
}

/// An address and a value, as `address:value`.
struct Pair(u64, u64);

impl fmt::Display for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}:{:#x}", self.0, self.1)
    }
}

/// A companion resource, as `marker/size@offset/alignment`.
struct CompanionText([u8; 4], u64, u64, u32);

impl fmt::Display for CompanionText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let marker = self.0.map(|byte| if byte.is_ascii_graphic() { byte as char } else { '.' });
        let [a, b, c, d] = marker;
        write!(f, "{a}{b}{c}{d}/{:#x}@{:#x}/{:#x}", self.1, self.2, self.3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_read_plainly() {
        assert_eq!(QueryOutcome::Record(Status::SUCCESS, Asked::Count).to_string(), "answered=adapter-count");
        assert_eq!(
            QueryOutcome::Record(Status::SUCCESS, Asked::Record { build_stamp: 0x10 }).to_string(),
            "answered=record guest_build=0x10"
        );
        assert_eq!(QueryOutcome::LeftToTheCore.to_string(), "answered=left-to-the-core");
        let refused = QueryOutcome::DriverName(Status::NOT_SUPPORTED, NameAnswer::No32BitName).to_string();
        assert!(refused.starts_with("refused="), "{refused}");
    }

    #[test]
    fn a_companion_and_a_missing_value_have_short_forms() {
        assert_eq!(CompanionText(*b"LCTS", 0x10, 0x20, 0x1000).to_string(), "LCTS/0x10@0x20/0x1000");
        assert_eq!(DisplayOption::<u8>(None).to_string(), "-");
        assert_eq!(Pair(0x10, 2).to_string(), "0x10:0x2");
    }

    #[test]
    fn a_queue_counts_its_submissions_until_it_closes() {
        let mut traffic = Traffic::default();
        traffic.queue(7, 1, &[0; hw_queue::SIZE]);
        let work = Work {
            source: guesthouse_backend::WorkSource::HardwareQueue,
            queue: 7,
            space: guesthouse_backend::SpaceId(1),
            node: 0,
            client_hint: 0,
            private: &[],
            fence: 0,
            fence_gpu_address: 0,
            fence_value: 1,
            may_run: true,
            queue_failed: false,
        };
        traffic.submission(&work, &Header::read(&[]));
        traffic.submission(&work, &Header::read(&[]));
        assert_eq!(traffic.submitted.get(&7), Some(&2));
        traffic.queue_closed(7);
        assert!(traffic.submitted.is_empty());
    }
}
