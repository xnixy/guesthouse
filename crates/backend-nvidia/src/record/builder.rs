//! Putting the record together. Where a field is the same on every card it is named here, and the
//! rest comes from the card and the host.

use super::card::{
    ARCHITECTURE_INFO, BUS_INFO, CHIPSET_INFO, CPU_INFO, CardReader, DMA_CAPS, FIFO_CAPS, FRAMEBUFFER_CAPS,
    FRAMEBUFFER_INFO, GPC_MASK, GPU_INFO, GRAPHICS_CAPS, GRAPHICS_INFO, JPEG_CAPS, NAME, PCI_INFO,
    SHORT_NAME, TPC_MASK, UUID, VIDEO_DECODE_CAPS, ZCULL_INFO,
};
use super::host::HostFacts;
use crate::answers::nodes::{Node, copy_record_flags, node_entry};
use crate::profile::{Generation, RecordLayout};
use crate::rm::classes::BUS_ENTRIES_THAT_CLEAR;
use crate::rm::{Rm, RmError};
use guesthouse_backend::Refusal;

const HEADER_MAGIC: &[u8; 4] = b"ADVN";
const HEADER_VERSION: u16 = 4;
const HEADER_REVISION: u16 = 1;
const BLOCK_MAGIC: &[u8; 4] = b"MKVN";
const BLOCK_VERSION: u32 = 1;

/// The driver model version, as a real install's record has it.
const DRIVER_MODEL_VERSION: u32 = 0x3200;

/// The Windows version in the record. The build is that of the guest this was tested with, and a
/// different guest gets this number too, as no message from the guest gives its own.
const WINDOWS_INFO_SIZE: u32 = 284;
const WINDOWS_MAJOR: u32 = 10;
const WINDOWS_MINOR: u32 = 0;
const WINDOWS_BUILD: u32 = 26300;
const WINDOWS_PLATFORM_NT: u32 = 2;
const WINDOWS_SUITE: u16 = 0x110;
const WINDOWS_WORKSTATION: u8 = 1;

const VIDEO_DECODE_CAPS_SIZE: usize = 8;
const VIDEO_DECODE_INSTANCES: u32 = 8;
const VIDEO_DECODE_INSTANCE_AT: usize = 8;
const JPEG_CAPS_SIZE: usize = 9;
const JPEG_INSTANCES: u32 = 8;
const JPEG_INSTANCE_AT: usize = 12;
const DMA_CAPS_PARAMS: [u8; 4] = [8, 0, 0, 0];

/// Bytes between one node's record and the next.
pub const RECORD_NODE_STRIDE: usize = 0x110;
const NODE_ENTRY_SIZE: usize = 40;
const NODE_ENGINE_COUNT_FIELD: usize = 0x08;
const NODE_COPY_ENGINE_FIELD: usize = 0x0C;

const CPU_KEPT: usize = 88;
const CPU_CAPS_FIELD: usize = 4;
const CPU_CAP_AVX: u32 = 1 << 19;

const GRAPHICS_CAPS_KEPT: usize = 23;
const GRAPHICS_ENTRIES: u32 = 0x3F;
const FRAMEBUFFER_ENTRIES: u32 = 0x45;
const BUS_ENTRIES: u32 = 0x34;
const GRAPHICS_GPC_COUNT: u32 = 0x14;
pub(super) const GRAPHICS_SM_VERSION: u32 = 0x0C;
pub(super) const TABLE_ENTRY_SIZE: usize = 8;

const FB_RAM: u32 = 7;
const FB_BAR1: u32 = 5;
const FB_HEAP: u32 = 9;
const FB_BAR1_AVAILABLE: u32 = 0x1D;
const FB_BAR1_CONTIGUOUS: u32 = 0x1F;
const FB_USABLE: u32 = 0x20;
const FB_RECLAIMABLE: u32 = 0x44;
const FB_MAILBOX: [u32; 3] = [0x30, 0x31, 0x32];

/// Fields a real install's record has filled and the Linux RM leaves at zero.
const MAILBOX_SIZE: u32 = 0x10000;
const MAILBOX_PEERS: u32 = 8;
const MAILBOX_BAR1_LIMIT: u32 = 0x80_0000;

const UUID_AS_BINARY: u32 = 2;
const ECID_LOW: u32 = 1;
const ECID_HIGH: u32 = 2;

/// (byte, bit) of what the driver reports differently on Windows.
const FB_CLEARED_ON_ALLOCATION: (usize, u8) = (2, 0x02);
const FIFO_NO_PIPELINED_PAGE_TABLE_BLIT: (usize, u8) = (0, 0x80);

/// The identifiers the guest's adapter goes by, which the core's identity answer repeats.
const ADAPTER_ID: [u32; 2] = [0xABC, 1];

/// The most classes the record's list has room for, before the next field.
const MOST_CLASSES: usize = 200;

/// (index, value) pairs, as the record lays them out.
type Table = Vec<(u32, u32)>;

/// Everything goes wrong the same way here: the backend can't start.
fn card_failed(error: RmError) -> Refusal {
    Refusal(format!("Reading the card failed: {error}"))
}

struct Record {
    data: Vec<u8>,
}

impl Record {
    fn put(&mut self, at: usize, bytes: &[u8]) {
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn word(&mut self, at: usize, value: u32) {
        self.put(at, &value.to_le_bytes());
    }

    fn words(&mut self, at: usize, values: &[u32]) {
        for (index, &value) in values.iter().enumerate() {
            self.word(at + index * 4, value);
        }
    }

    /// A list with its length in front. The guest's runtime copies as many words as the length says
    /// into a buffer of its own, so a word out of place here overruns its stack.
    fn counted_words(&mut self, at: usize, values: &[u32]) -> Result<(), Refusal> {
        if values.len() > MOST_CLASSES {
            return Err(Refusal(format!(
                "The card has {} classes, and the guest's record holds {MOST_CLASSES}.",
                values.len()
            )));
        }
        self.word(at, values.len() as u32);
        self.words(at + 4, values);
        Ok(())
    }

    fn table(&mut self, at: usize, values: &[(u32, u32)]) {
        for &(index, value) in values {
            self.words(at + index as usize * TABLE_ENTRY_SIZE, &[index, value]);
        }
    }
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn clear_bit(mut table: Vec<u8>, (byte, bit): (usize, u8)) -> Vec<u8> {
    table[byte] &= !bit;
    table
}

fn put_fixed_fields(record: &mut Record, layout: &RecordLayout) {
    record.put(0, HEADER_MAGIC);
    record.put(4, &HEADER_VERSION.to_le_bytes());
    record.put(6, &HEADER_REVISION.to_le_bytes());
    record.put(layout.block_magic_field, BLOCK_MAGIC);
    record.word(layout.block_magic_field + 4, BLOCK_VERSION);
    record.word(layout.driver_model_version, DRIVER_MODEL_VERSION);
    layout.always_set.iter().for_each(|&field| record.word(field, 1));
    let windows = [WINDOWS_INFO_SIZE, WINDOWS_MAJOR, WINDOWS_MINOR, WINDOWS_BUILD, WINDOWS_PLATFORM_NT];
    record.words(layout.windows_version, &windows);
    record.put(layout.windows_suite, &WINDOWS_SUITE.to_le_bytes());
    record.put(layout.windows_suite + 2, &[WINDOWS_WORKSTATION]);
}

fn put_engines<R: Rm>(
    record: &mut Record,
    layout: &RecordLayout,
    card: &mut CardReader<'_, R>,
    nodes: &[Node],
    copy_node_flags: Option<u32>,
) -> Result<(), RmError> {
    for instance in 0..VIDEO_DECODE_INSTANCES {
        let at = layout.video_decode_caps + instance as usize * VIDEO_DECODE_CAPS_SIZE;
        let caps =
            card.instance(VIDEO_DECODE_CAPS, instance, VIDEO_DECODE_INSTANCE_AT, VIDEO_DECODE_CAPS_SIZE);
        record.put(at, &caps);
    }
    for instance in 0..JPEG_INSTANCES {
        let at = layout.jpeg_caps + instance as usize * JPEG_CAPS_SIZE;
        record.put(at, &card.instance(JPEG_CAPS, instance, JPEG_INSTANCE_AT, JPEG_CAPS_SIZE));
    }
    record.put(layout.dma_caps, &card.read(DMA_CAPS, &DMA_CAPS_PARAMS)?[4..5]);
    record.word(layout.node_count, nodes.len() as u32);
    for (ordinal, node) in nodes.iter().enumerate() {
        record.put(layout.node_entries + ordinal * NODE_ENTRY_SIZE, &node_entry(node));
        if node.copy_engine != 0 {
            let at = layout.node_records + ordinal * RECORD_NODE_STRIDE;
            let physical = card.copy_engine_mask(node.copy_engine)?;
            let capabilities = match copy_node_flags {
                Some(flags) => flags,
                None => card.copy_engine_caps(node.copy_engine)?,
            };
            record.word(at, copy_record_flags(node, capabilities));
            record.word(at + NODE_ENGINE_COUNT_FIELD, physical.count_ones());
            record.word(at + NODE_COPY_ENGINE_FIELD, node.copy_engine);
        }
    }
    Ok(())
}

/// The framebuffer table, with the fields the RM leaves for another OS filled in.
fn windows_memory_table<R: Rm>(card: &mut CardReader<'_, R>) -> Result<Vec<(u32, u32)>, RmError> {
    let mut values = Vec::with_capacity(FRAMEBUFFER_ENTRIES as usize);
    for index in 0..FRAMEBUFFER_ENTRIES {
        values.push((index, card.entry(FRAMEBUFFER_INFO, index)?));
    }
    let value = |index: u32| values[index as usize].1;
    let bar1 = value(FB_BAR1);
    let changes = [
        (FB_RECLAIMABLE, 0),
        (FB_BAR1_AVAILABLE, bar1),
        (FB_BAR1_CONTIGUOUS, bar1),
        (FB_MAILBOX[0], MAILBOX_SIZE * MAILBOX_PEERS),
        (FB_MAILBOX[1], MAILBOX_SIZE),
        (FB_MAILBOX[2], MAILBOX_BAR1_LIMIT),
    ];
    for (index, new) in changes {
        values[index as usize].1 = new;
    }
    Ok(values)
}

fn put_cpu<R: Rm>(
    record: &mut Record,
    layout: &RecordLayout,
    card: &mut CardReader<'_, R>,
    host: &HostFacts,
) -> Result<(), RmError> {
    let mut cpu = card.read(CPU_INFO, &[])?;
    cpu.truncate(CPU_KEPT);
    let capabilities = word(&cpu, CPU_CAPS_FIELD) | if host.has_avx { CPU_CAP_AVX } else { 0 };
    cpu[CPU_CAPS_FIELD..CPU_CAPS_FIELD + 4].copy_from_slice(&capabilities.to_le_bytes());
    cpu[CPU_CAPS_FIELD + 4..CPU_CAPS_FIELD + 8].copy_from_slice(&host.cpu_megahertz.to_le_bytes());
    record.put(layout.cpu, &cpu);
    let short = [&cpu[0..1], &cpu[4..7], &cpu[8..12]].concat();
    record.put(layout.cpu_short, &short);
    let megabytes = (host.total_ram_kib / 1024) as u32;
    layout.ram_megabytes.iter().for_each(|&field| record.word(field, megabytes));
    Ok(())
}

fn shader_set(generation: &Generation, sm_version: u32) -> Result<u32, Refusal> {
    let known = generation.shader_sets.iter().find(|set| set.0 == sm_version);
    known
        .map(|set| set.1)
        .ok_or_else(|| Refusal(format!("SM {sm_version:#x} has not been tested with this project.")))
}

fn put_tables<R: Rm>(
    record: &mut Record,
    layout: &RecordLayout,
    card: &mut CardReader<'_, R>,
    generation: &Generation,
) -> Result<(Table, Table), Refusal> {
    let mut graphics = Vec::with_capacity(GRAPHICS_ENTRIES as usize);
    for index in 0..GRAPHICS_ENTRIES {
        graphics.push((index, card.entry(GRAPHICS_INFO, index).map_err(card_failed)?));
    }
    record.table(layout.graphics_table, &graphics);
    let sm_version = graphics[GRAPHICS_SM_VERSION as usize].1;
    record.word(layout.shader_set, shader_set(generation, sm_version)?);
    let framebuffer = windows_memory_table(card).map_err(card_failed)?;
    record.table(layout.framebuffer_table, &framebuffer);
    let mut bus = Vec::with_capacity(BUS_ENTRIES as usize);
    for index in 0..BUS_ENTRIES {
        let left = BUS_ENTRIES_THAT_CLEAR.contains(&index);
        bus.push((index, if left { 0 } else { card.entry(BUS_INFO, index).map_err(card_failed)? }));
    }
    record.table(layout.bus_table, &bus);
    Ok((graphics, framebuffer))
}

/// Builds the record.
pub fn build<R: Rm>(
    layout: &RecordLayout,
    generation: &Generation,
    card: &mut CardReader<'_, R>,
    host: &HostFacts,
    nodes: &[Node],
) -> Result<Vec<u8>, Refusal> {
    let mut record = Record { data: vec![0; layout.size] };
    put_fixed_fields(&mut record, layout);
    record.put(layout.system_ram, &(host.total_ram_kib << 10).to_le_bytes());
    put_engines(&mut record, layout, card, nodes, generation.copy_node_flags).map_err(card_failed)?;

    record.word(layout.gpu_id, card.gpu_id().map_err(card_failed)?);
    let pci = card.read(PCI_INFO, &[]).map_err(card_failed)?;
    let (device, subsystem, revision, extended) =
        (word(&pci, 0), word(&pci, 4), word(&pci, 8), word(&pci, 12));
    record.words(layout.pci, &[extended, subsystem, revision, device]);
    put_cpu(&mut record, layout, card, host).map_err(card_failed)?;

    let chipset = card.read(CHIPSET_INFO, &[]).map_err(card_failed)?;
    record.put(layout.chipset, &chipset[0..4]);
    let bridge_vendor = u16::from_le_bytes([chipset[8], chipset[9]]);
    let bridge_device = u16::from_le_bytes([chipset[10], chipset[11]]);
    record.words(layout.host_bridge, &[u32::from(bridge_device), u32::from(bridge_vendor)]);
    record.put(layout.architecture, &card.read(ARCHITECTURE_INFO, &[]).map_err(card_failed)?[..12]);

    let caps = card.read(FRAMEBUFFER_CAPS, &[]).map_err(card_failed)?;
    record.put(layout.framebuffer_caps, &clear_bit(caps, FB_CLEARED_ON_ALLOCATION));
    let caps = card.read(FIFO_CAPS, &[]).map_err(card_failed)?;
    record.put(layout.fifo_caps, &clear_bit(caps, FIFO_NO_PIPELINED_PAGE_TABLE_BLIT));
    let caps = card.read(GRAPHICS_CAPS, &[]).map_err(card_failed)?;
    record.put(layout.graphics_caps, &caps[..GRAPHICS_CAPS_KEPT]);

    let (graphics, framebuffer) = put_tables(&mut record, layout, card, generation)?;

    let params = [0, UUID_AS_BINARY, 0].map(u32::to_le_bytes).concat();
    record.put(layout.uuid, &card.read(UUID, &params).map_err(card_failed)?[12..28]);
    record.put(layout.gpc_mask, &card.read(GPC_MASK, &[]).map_err(card_failed)?[16..20]);
    for gpc in 0..graphics[GRAPHICS_GPC_COUNT as usize].1 {
        let params = [&[0; 16][..], &gpc.to_le_bytes()].concat();
        let mask = card.read(TPC_MASK, &params).map_err(card_failed)?;
        record.put(layout.tpc_masks + gpc as usize * 4, &mask[20..24]);
    }
    record.put(layout.zcull, &card.read(ZCULL_INFO, &[]).map_err(card_failed)?);
    record.word(layout.ecid, card.entry(GPU_INFO, ECID_LOW).map_err(card_failed)?);
    record.word(layout.ecid + 4, card.entry(GPU_INFO, ECID_HIGH).map_err(card_failed)?);

    let ram = u64::from(framebuffer[FB_RAM as usize].1) << 10;
    let heap = u64::from(framebuffer[FB_HEAP as usize].1) << 10;
    record.put(layout.dedicated, &heap.to_le_bytes());
    let usable = u64::from(framebuffer[FB_USABLE as usize].1) << 10;
    let sizes = [usable, heap, (host.total_ram_kib << 10) / 2, ram];
    for (index, size) in sizes.into_iter().enumerate() {
        record.put(layout.memory + index * 8, &size.to_le_bytes());
        record.word(layout.memory_low + index * 4, size as u32);
    }

    record.words(layout.video_caps, &generation.video_record_flags.caps);
    record.put(layout.decoder_flags, &[generation.video_record_flags.decoder]);
    record.put(layout.encoder_flags, &[generation.video_record_flags.encoder]);
    record.counted_words(layout.classes, &card.classes().map_err(card_failed)?)?;

    let (graphics_hertz, memory_hertz) = host.rated_clocks;
    let graphics_khz = (graphics_hertz / 1000) as u32;
    let memory_khz = (memory_hertz / 1000) as u32;
    record.words(layout.rated_clocks, &[graphics_khz, memory_khz, graphics_khz, graphics_khz]);
    record.words(layout.current_clocks, &[graphics_khz, memory_khz, graphics_khz]);
    record.words(layout.adapter_id, &ADAPTER_ID);
    let name = card.text(NAME, 4).map_err(card_failed)?;
    let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    record.put(layout.name, &name);
    record.put(layout.short_name, card.text(SHORT_NAME, 0).map_err(card_failed)?.as_bytes());
    record.word(layout.size_field, layout.size as u32);
    Ok(record.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::BLACKWELL;

    #[test]
    fn an_sm_version_nobody_has_run_refuses() {
        assert!(shader_set(&BLACKWELL, BLACKWELL.shader_sets[0].0).is_ok());
        assert!(shader_set(&BLACKWELL, 0xFFF).is_err());
    }
}
