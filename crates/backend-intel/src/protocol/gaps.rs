//! What the adapter record leaves at zero because nothing here knows the right value yet.
//!
//! Each is a part of the record the guest's driver may read. None is made up: a field stays zero
//! until the card, Intel's open sources or a guest's own traffic says what belongs there. The log
//! names every one at start, so a tester's log says what the guest was not told.

/// The parts of the record left at zero, in words for the log.
pub const RECORD: [&str; 14] = [
    "the kernel driver's interface version (the record's first dword)",
    "the device's registry path",
    "the display IP version",
    "the platform word between the display and render IP versions",
    "the per-adapter block after the platform",
    "the workaround table, apart from the one entry for a guest",
    "the core and bus frequencies, the power limit and the fill rate",
    "the command buffer, patch list and allocation list sizes",
    "the kernel driver's capability bits",
    "the cache sizes and bank count, and the slice and subslice arrays",
    "the GPU address partition",
    "the adapter's bus, device and function",
    "the names, counts and flags at the record's end",
    "the second video engine block and the deferred wait block",
];
