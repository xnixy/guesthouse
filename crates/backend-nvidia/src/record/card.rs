//! Reading the live card. Every control here only reads.

use crate::rm::device::Device;
use crate::rm::{Rm, RmError};

/// (command, parameter size). All are in the public headers.
pub type Control = (u32, usize);

pub const CPU_INFO: Control = (0x102, 108);
pub const CHIPSET_INFO: Control = (0x104, 156);
pub const CLASS_LIST: Control = (0x80_0292, 804);
pub const FRAMEBUFFER_CAPS: Control = (0x80_1307, 3);
pub const FIFO_CAPS: Control = (0x80_1713, 2);
pub const GRAPHICS_CAPS: Control = (0x80_1109, 48);
pub const GPU_INFO: Control = (0x2080_0102, 580);
pub const FRAMEBUFFER_INFO: Control = (0x2080_1303, 1028);
pub const GRAPHICS_INFO: Control = (0x2080_1228, 528);
pub const BUS_INFO: Control = (0x2080_1823, 420);
pub const NAME: Control = (0x2080_0110, 68);
pub const SHORT_NAME: Control = (0x2080_0111, 64);
pub const UUID: Control = (0x2080_014A, 268);
pub const PCI_INFO: Control = (0x2080_1801, 16);
pub const ARCHITECTURE_INFO: Control = (0x2080_1701, 16);
pub const ZCULL_INFO: Control = (0x2080_1206, 40);
pub const GPC_MASK: Control = (0x2080_122A, 24);
pub const TPC_MASK: Control = (0x2080_122B, 24);
pub const ENGINES: Control = (0x2080_0170, 340);
pub const ENGINE_PARTNERS: Control = (0x2080_0147, 0x90);
pub const VIDEO_DECODE_CAPS: Control = (0x80_1C02, 12);
pub const JPEG_CAPS: Control = (0x80_1F02, 16);
pub const DMA_CAPS: Control = (0x80_180D, 12);
pub const COPY_ENGINE_MASK: Control = (0x2080_2A02, 8);
pub const COPY_ENGINE_CAPS: Control = (0x2080_2A03, 8);

const CHANNEL_CLASS_LOW_BYTE: u32 = 0x6F;
const ENGINE_GRAPHICS: u32 = 1;
const ENTRY_VALUE_FIELD: usize = 8;

/// Where the caps table starts in its control's parameters. It's two bytes.
const COPY_CAPS_TABLE: usize = 4;
const COPY_CAP_SYSTEM_MEMORY: u32 = 0x20;
const COPY_CAP_LARGE_BLOCK_LINEAR: u32 = 0x80;
const COPY_CAP_NON_PIPELINED_BLOCK_LINEAR: u32 = 0x100;
/// The caps a copy node's record carries, at the same bits.
const COPY_CAPS_IN_RECORD: u32 =
    COPY_CAP_SYSTEM_MEMORY | COPY_CAP_LARGE_BLOCK_LINEAR | COPY_CAP_NON_PIPELINED_BLOCK_LINEAR;

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|value| value.to_le_bytes()).collect()
}

/// Reads through a device.
pub struct CardReader<'a, R> {
    device: &'a mut Device<R>,
}

impl<'a, R: Rm> CardReader<'a, R> {
    pub fn new(device: &'a mut Device<R>) -> Self {
        Self { device }
    }

    /// Runs a control with these parameters at the front, at the size this branch's driver takes.
    pub fn read(&mut self, control: Control, params: &[u8]) -> Result<Vec<u8>, RmError> {
        let (command, size) = control;
        let size = self.device.host_control_size(command).unwrap_or(size);
        let mut buffer = params.to_vec();
        buffer.resize(size.max(params.len()), 0);
        let target = self.device.control_target(command);
        self.device.control(target, command, &mut buffer)?;
        Ok(buffer)
    }

    /// The RM's number for this card, which changes with where the card sits in the machine.
    pub fn gpu_id(&mut self) -> Result<u32, RmError> {
        self.device.gpu_id()
    }

    /// One entry of a control that reads a table by index.
    pub fn entry(&mut self, control: Control, index: u32) -> Result<u32, RmError> {
        Ok(word(&self.read(control, &words(&[1, index, 0]))?, ENTRY_VALUE_FIELD))
    }

    /// Text up to the first zero, after `skip` bytes.
    pub fn text(&mut self, control: Control, skip: usize) -> Result<String, RmError> {
        let answer = self.read(control, &[])?;
        let text = answer[skip..].split(|&byte| byte == 0).next().unwrap_or(&[]);
        Ok(String::from_utf8_lossy(text).into_owned())
    }

    fn counted_list(&mut self, control: Control) -> Result<Vec<u32>, RmError> {
        let answer = self.read(control, &[])?;
        let count = word(&answer, 0) as usize;
        Ok((0..count).map(|index| word(&answer, 4 + index * 4)).collect())
    }

    pub fn engines(&mut self) -> Result<Vec<u32>, RmError> {
        self.counted_list(ENGINES)
    }

    pub fn classes(&mut self) -> Result<Vec<u32>, RmError> {
        self.counted_list(CLASS_LIST)
    }

    /// The copy engine the RM pairs with graphics, if it names one.
    pub fn graphics_copy(&mut self) -> Result<Option<u32>, RmError> {
        let classes = self.classes()?;
        let channels = classes.iter().filter(|class| *class & 0xFF == CHANNEL_CLASS_LOW_BYTE);
        let newest = channels.max().copied().unwrap_or(0);
        let said = self.read(ENGINE_PARTNERS, &words(&[ENGINE_GRAPHICS, newest, 0, 0]))?;
        Ok((word(&said, 12) != 0).then(|| word(&said, 16)))
    }

    /// One engine's caps, or zeros if the card has no such engine.
    pub fn instance(&mut self, control: Control, instance: u32, at: usize, size: usize) -> Vec<u8> {
        let mut params = vec![0; at];
        params.extend_from_slice(&instance.to_le_bytes());
        match self.read(control, &params) {
            Ok(answer) => answer[..size].to_vec(),
            Err(_) => vec![0; size],
        }
    }

    /// What a copy engine can do, as a copy node's record says it.
    pub fn copy_engine_caps(&mut self, engine: u32) -> Result<u32, RmError> {
        let answer = self.read(COPY_ENGINE_CAPS, &engine.to_le_bytes())?;
        let table = [answer[COPY_CAPS_TABLE], answer[COPY_CAPS_TABLE + 1]];
        Ok(u32::from(u16::from_le_bytes(table)) & COPY_CAPS_IN_RECORD)
    }

    /// Which physical engines serve a copy engine.
    pub fn copy_engine_mask(&mut self, engine: u32) -> Result<u32, RmError> {
        Ok(word(&self.read(COPY_ENGINE_MASK, &engine.to_le_bytes())?, 4))
    }
}
