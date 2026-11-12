//! The host driver's resource manager, or RM: objects made and freed by handle, and controls run
//! on them.
//!
//! Everything else in this crate goes through [`Rm`], which is what lets the tests run with no
//! card. `linux.rs` is the real one, and `fake.rs` plays its part in tests.

pub mod classes;
pub mod device;
pub mod dmabuf;
#[cfg(any(test, feature = "fake-rm"))]
pub mod fake;
#[cfg(any(test, feature = "fake-rm"))]
pub mod fake_card;
#[cfg(any(test, feature = "fake-rm"))]
pub mod fake_engine;
pub mod instance;
pub mod linux;
pub mod listing;

use std::fmt;
use std::os::fd::OwnedFd;

/// An object's name in the RM.
pub type Handle = u32;

/// The RM's word for success.
pub const STATUS_OK: u32 = 0;

/// The RM's status for something not allowed here.
pub const STATUS_NOT_SUPPORTED: u32 = 0x56;

/// The RM's status for parameters that aren't the size of the control's structure.
pub const STATUS_INVALID_PARAM_STRUCT: u32 = 0x3A;

/// The RM's status for a device whose GPU this client hasn't opened the device file of.
pub const STATUS_INSUFFICIENT_PERMISSIONS: u32 = 0x1B;

/// The RM's status for an argument that names nothing.
pub const STATUS_INVALID_ARGUMENT: u32 = 0x1F;

/// What went wrong in a call to the RM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RmError {
    /// The RM answered, and said no.
    Refused { what: String, status: u32 },
    /// The call never reached the RM.
    System(String),
}

impl RmError {
    pub fn refused(what: impl Into<String>, status: u32) -> Self {
        Self::Refused { what: what.into(), status }
    }

    pub(crate) fn check(what: impl FnOnce() -> String, status: u32) -> Result<(), Self> {
        if status == STATUS_OK { Ok(()) } else { Err(Self::refused(what(), status)) }
    }
}

impl fmt::Display for RmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { what, status } => write!(f, "{what}: status {status:#x}"),
            Self::System(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for RmError {}

/// A mapping of memory into a GPU address range, at a fixed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaMap {
    pub device: Handle,
    /// The range object holding the address.
    pub range: Handle,
    pub memory: Handle,
    pub offset: u64,
    pub length: u64,
    pub address: u64,
    pub small_pages: bool,
    pub gpu_cached: bool,
    /// The page kind this one mapping takes in place of the memory's own, if any.
    pub page_kind: Option<u32>,
}

/// The RM's calls, as this backend uses them.
pub trait Rm {
    /// Makes the client that every other object hangs from.
    fn new_client(&mut self) -> Result<Handle, RmError>;

    /// Makes an object. `params` go in and come back changed. Returns the RM's status.
    fn alloc(
        &mut self,
        parent: Handle,
        handle: Handle,
        class: u32,
        params: &mut [u8],
    ) -> Result<u32, RmError>;

    /// Runs a control. `params` go in and come back changed. Returns the RM's status.
    fn control(&mut self, object: Handle, command: u32, params: &mut [u8]) -> Result<u32, RmError>;

    fn free(&mut self, parent: Handle, handle: Handle) -> Result<(), RmError>;

    /// Makes host pages, already mapped at `address` in this process, usable by the GPU.
    fn alloc_host_pages(
        &mut self,
        device: Handle,
        handle: Handle,
        address: u64,
        size: u64,
    ) -> Result<(), RmError>;

    fn map_dma(&mut self, map: &DmaMap) -> Result<(), RmError>;

    fn unmap_dma(
        &mut self,
        device: Handle,
        range: Handle,
        memory: Handle,
        address: u64,
        length: u64,
    ) -> Result<(), RmError>;

    /// Hands out video memory as a dmabuf. The memory stays until both this and the RM's object go.
    fn export_dmabuf(&mut self, device: Handle, memory: Handle, size: u64) -> Result<OwnedFd, RmError>;

    /// Maps the page whose writes reach the GPU's doorbell. One serves every channel.
    fn map_doorbell(&mut self, subdevice: Handle, usermode: Handle) -> Result<(), RmError>;

    /// Tells the GPU a channel has new work. Needs [`Rm::map_doorbell`] first.
    fn ring_doorbell(&mut self, token: u32);

    /// The GPU's timer in nanoseconds, read from the doorbell's page, so no RM lock is taken. `None`
    /// until [`Rm::map_doorbell`] has run, or if the reading won't hold still.
    fn read_gpu_timer(&self) -> Option<u64>;
}
