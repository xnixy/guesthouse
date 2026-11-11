//! What the adapter record leaves at zero because nothing here knows the right value or place yet.
//!
//! Each is a part of the record the guest's driver may read. None is made up: a field stays zero
//! until the card, Intel's open sources or a guest's own traffic says what belongs there and
//! where. The log names every one at start, so a tester's log says what the guest was not told.

/// The parts of the record always left at zero, in words for the log.
pub const RECORD: [&str; 19] = [
    "the kernel driver's interface version (the record's first dword)",
    "the device's registry path",
    "the display IP version",
    "the media IP version",
    "the platform word between the display and render IP versions",
    "the per-adapter block after the platform",
    "the workaround table, every entry but the one that marks a virtual machine's adapter",
    "the feature flags whose places aren't known: Tile64Optimization, the video and video-enhancement \
     ring flags, and the rest of the table",
    "the feature flags whose places are only inferred: 3D mid-batch preemption, 3D object-level \
     preemption and the per-context preemption granularity control",
    "the core and bus frequencies, the power limit and the fill rate",
    "the command buffer, patch list and allocation list sizes",
    "the kernel driver's capability bits",
    "the cache sizes and bank count",
    "the slice array's slots past the ones seen: dual subslices past the sixth in a slice, and the \
     flag that says slices are populated dynamically",
    "the video, video-enhancement and multi-tile blocks of the GPU's shape",
    "the GPU address partition's eleventh range, which has no name",
    "the adapter's bus, device and function",
    "the names, counts and flags at the record's end",
    "the deferred wait block",
];

/// The slice, subslice and EU arrays, left empty when the card gave no topology or no hwconfig
/// that says how its cores divide into slices.
const SLICE_ARRAYS: &str = "the slice, subslice and EU arrays of the GPU's shape, which compute-runtime \
     needs to open a device (the card did not say how its cores divide into slices)";

/// Everything left at zero in a record, given whether its slice arrays were filled.
pub fn unset(slice_arrays_filled: bool) -> Vec<&'static str> {
    let mut gaps = RECORD.to_vec();
    if !slice_arrays_filled {
        gaps.push(SLICE_ARRAYS);
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slice_arrays_are_a_gap_only_when_not_filled() {
        assert_eq!(unset(true).len(), RECORD.len());
        assert_eq!(unset(false).len(), RECORD.len() + 1);
        assert!(unset(false).last().is_some_and(|gap| gap.contains("slice")));
    }
}
