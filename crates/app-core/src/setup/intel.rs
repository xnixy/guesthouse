//! What a Windows guest on an Intel card needs from setup. None of it can be done yet: Intel's driver
//! can't be unpacked for a guest, the guest's files for it aren't decided, and the daemon doesn't open
//! an Intel card. Everything that would go on from here to a running guest refuses with this, so an
//! Intel machine never falls through to another maker's steps.

pub const NO_INTEL_SUPPORT: &str =
    "Windows can't use an Intel card yet: setting up Intel's driver isn't part of this version.";
