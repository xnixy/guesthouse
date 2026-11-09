//! An allocation's private data: the header, a surface descriptor per surface, and an optional
//! second information block.

/// The size of one surface descriptor, VCAM_SURFACE_DESC in AMD's open interop header.
pub const SURFACE_SIZE: usize = 0x120;
