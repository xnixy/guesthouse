//! A new context's private data, CREATECONTEXT_PVTDATA.
//!
//! The guest's driver sends it with every context. The kernel driver takes none at all, or at
//! least [`SIZE`] bytes. Most of it is what the kernel driver would set up for the context itself:
//! what the context is for, its power hint, its preemption. On this host the kernel's own driver
//! sets those up its own way, so they are only read here, for the log.
//!
//! The first field is a pointer into the guest process, where the kernel driver would write the
//! IDs of the hardware contexts it made. The host can't write there, and doesn't need to: with the
//! record's virtual machine flag set, the guest's driver sends no pointer.

use super::bytes::{byte, dword, qword};

/// The size the kernel driver needs, when there is any data.
pub const SIZE: usize = 0x90;

/// Where the fields are. The names are compute-runtime's, where it has one.
mod at {
    pub const HW_CONTEXT_ID_POINTER: usize = 0x00;
    pub const HW_CONTEXT_ID_COUNT: usize = 0x08;
    pub const PROCESS_ID: usize = 0x0C;
    pub const IS_PROTECTED_PROCESS: usize = 0x10;
    pub const IS_DWM: usize = 0x11;
    pub const IS_MEDIA_USAGE: usize = 0x13;
    pub const GPU_VA_CONTEXT: usize = 0x24;
    pub const NO_RING_FLUSHES: usize = 0x44;
    /// Bits 0 to 3 choose which of the engine's instances the context runs on.
    pub const ENGINE_INSTANCES: usize = 0x48;
    pub const UMD_CONTEXT_TYPE: usize = 0x50;
    /// Bits 0 to 6 are the hint, bit 7 says it is set.
    pub const POWER_HINT: usize = 0x55;
    pub const DEBUG_SIP_INSTALLED: usize = 0x56;
    pub const HW_QUEUE_COUNT: usize = 0x7C;
    pub const DISABLE_WMTP: usize = 0x7D;
    pub const NOTIFY_PREEMPT_EXCEEDED: usize = 0x7E;
    pub const PREEMPT_EVENT: usize = 0x80;
    /// Bit 0: the context uses 64-bit tokens.
    pub const USE_64_BIT_TOKEN: usize = 0x8C;
}

/// The ranges [`Context::parse`] reads, so a log line can list what else the data holds.
pub const DECODED: [(usize, usize); 11] = [
    (at::HW_CONTEXT_ID_POINTER, at::PROCESS_ID + 4),
    (at::IS_PROTECTED_PROCESS, at::IS_DWM + 1),
    (at::IS_MEDIA_USAGE, at::IS_MEDIA_USAGE + 1),
    (at::GPU_VA_CONTEXT, at::GPU_VA_CONTEXT + 1),
    (at::NO_RING_FLUSHES, at::NO_RING_FLUSHES + 1),
    (at::ENGINE_INSTANCES, at::ENGINE_INSTANCES + 1),
    (at::UMD_CONTEXT_TYPE, at::UMD_CONTEXT_TYPE + 4),
    (at::POWER_HINT, at::DEBUG_SIP_INSTALLED + 1),
    (at::HW_QUEUE_COUNT, at::NOTIFY_PREEMPT_EXCEEDED + 1),
    (at::PREEMPT_EVENT, at::PREEMPT_EVENT + 8),
    (at::USE_64_BIT_TOKEN, at::USE_64_BIT_TOKEN + 1),
];

const ENGINE_INSTANCE_BITS: u8 = 0x0F;
const POWER_HINT_VALUE: u8 = 0x7F;
const POWER_HINT_SET: u8 = 0x80;

/// Why the kernel driver would refuse this data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// What a context's data says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Context {
    pub size: usize,
    pub hw_context_id_pointer: u64,
    pub hw_context_id_count: u32,
    pub process_id: u32,
    pub protected_process: bool,
    pub dwm: bool,
    pub media_usage: bool,
    pub gpu_va_context: bool,
    pub no_ring_flushes: bool,
    pub engine_instances: u8,
    pub umd_context_type: u32,
    pub power_hint: Option<u8>,
    pub debug_sip_installed: bool,
    pub hw_queue_count: u8,
    pub disable_wmtp: bool,
    pub notify_preempt_exceeded: bool,
    pub preempt_event: u64,
    pub use_64_bit_token: bool,
}

impl Context {
    pub fn parse(data: &[u8]) -> Result<Self, Refused> {
        if data.is_empty() {
            return Ok(Self::default());
        }
        if data.len() < SIZE {
            return Err(Refused("its private data is shorter than the kernel driver takes"));
        }
        let flag = |at| byte(data, at).unwrap_or(0) != 0;
        let power = byte(data, at::POWER_HINT).unwrap_or(0);
        Ok(Self {
            size: data.len(),
            hw_context_id_pointer: qword(data, at::HW_CONTEXT_ID_POINTER).unwrap_or(0),
            hw_context_id_count: dword(data, at::HW_CONTEXT_ID_COUNT).unwrap_or(0),
            process_id: dword(data, at::PROCESS_ID).unwrap_or(0),
            protected_process: flag(at::IS_PROTECTED_PROCESS),
            dwm: flag(at::IS_DWM),
            media_usage: flag(at::IS_MEDIA_USAGE),
            gpu_va_context: flag(at::GPU_VA_CONTEXT),
            no_ring_flushes: flag(at::NO_RING_FLUSHES),
            engine_instances: byte(data, at::ENGINE_INSTANCES).unwrap_or(0) & ENGINE_INSTANCE_BITS,
            umd_context_type: dword(data, at::UMD_CONTEXT_TYPE).unwrap_or(0),
            power_hint: (power & POWER_HINT_SET != 0).then_some(power & POWER_HINT_VALUE),
            debug_sip_installed: flag(at::DEBUG_SIP_INSTALLED),
            hw_queue_count: byte(data, at::HW_QUEUE_COUNT).unwrap_or(0),
            disable_wmtp: flag(at::DISABLE_WMTP),
            notify_preempt_exceeded: flag(at::NOTIFY_PREEMPT_EXCEEDED),
            preempt_event: qword(data, at::PREEMPT_EVENT).unwrap_or(0),
            use_64_bit_token: flag(at::USE_64_BIT_TOKEN),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::bytes::put_dword;

    /// The data the guest's Direct3D 12 driver sends for a context, as far as it is known, on an
    /// adapter flagged as a virtual machine: no pointer, one ID, the process and the context type.
    pub fn direct3d_12() -> Vec<u8> {
        let mut data = vec![0u8; SIZE];
        put_dword(&mut data, at::HW_CONTEXT_ID_COUNT, 1);
        put_dword(&mut data, at::PROCESS_ID, 4242);
        data[at::GPU_VA_CONTEXT] = 1;
        put_dword(&mut data, at::UMD_CONTEXT_TYPE, 7);
        data
    }

    #[test]
    fn the_fields_known_are_read() {
        let mut data = direct3d_12();
        data[at::IS_DWM] = 1;
        data[at::ENGINE_INSTANCES] = 0xF3;
        data[at::POWER_HINT] = 0x80 | 5;
        data[at::USE_64_BIT_TOKEN] = 1;
        let context = Context::parse(&data).unwrap();
        assert_eq!((context.process_id, context.umd_context_type, context.hw_context_id_count), (4242, 7, 1));
        assert!(context.dwm && context.gpu_va_context && context.use_64_bit_token);
        assert_eq!(context.engine_instances, 3, "only the low four bits choose instances");
        assert_eq!(context.power_hint, Some(5));
        assert_eq!(context.hw_context_id_pointer, 0);
    }

    #[test]
    fn a_power_hint_without_its_set_bit_is_none() {
        let mut data = direct3d_12();
        data[at::POWER_HINT] = 5;
        assert_eq!(Context::parse(&data).unwrap().power_hint, None);
    }

    #[test]
    fn no_data_is_taken_and_short_data_is_refused() {
        assert_eq!(Context::parse(&[]), Ok(Context::default()));
        assert!(Context::parse(&direct3d_12()[..SIZE - 1]).is_err());
        assert!(Context::parse(&[0; SIZE + 0x10]).is_ok(), "longer is taken");
    }
}
