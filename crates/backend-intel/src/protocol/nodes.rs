//! The engine nodes the guest is offered, numbered as gmmlib's GPUNODE_ORDINAL in `UmKmEnum.h`.
//!
//! There are [`COUNT`] of them. gmmlib names the first ten; the others have no public name, and
//! are offered so that a context on one is made and logged rather than refused. Nothing runs on any
//! node in this stage, so a node is a label for the log. The core describes node 0 as the 3D engine
//! and every other as a copy engine, which the trait gives no way to change.

pub const COUNT: u32 = 15;

/// GPUNODE_ORDINAL's names, by ordinal.
const NAMES: [&str; 10] =
    ["3d", "video", "blt", "video-enhance", "vcs2", "ccs0", "pics", "overlay", "gdi2d", "vxd"];

/// A node's name for the log.
pub fn name(node: u32) -> &'static str {
    match usize::try_from(node).ok().and_then(|node| NAMES.get(node)) {
        Some(name) => name,
        None if node < COUNT => "unnamed",
        None => "past-the-nodes",
    }
}

pub fn offered(node: u32) -> bool {
    node < COUNT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_have_gmmlibs_names_and_the_rest_are_unnamed() {
        assert_eq!((name(0), name(2), name(5)), ("3d", "blt", "ccs0"));
        assert_eq!(name(13), "unnamed");
        assert_eq!(name(COUNT), "past-the-nodes");
        assert!(offered(COUNT - 1) && !offered(COUNT));
    }
}
