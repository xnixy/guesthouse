//! The page kind of a depth buffer's mapping. On Turing, Ampere and Ada the depth unit faults on depth
//! pages of the wrong kind, and a heap the guest's driver maps a second time for depth says which
//! kind in the driver's protection value of the map. Blackwell's page table entries carry no kind.

/// Where the depth class sits in the driver's protection value. Zero means the map isn't for depth.
const CLASS_SHIFT: u32 = 8;
const CLASS_MASK: u64 = 0xFF;
const CLASS_NONE: u8 = 0;

// The depth classes, in the driver's own numbering, and what each one packs.
const CLASS_S8: u8 = 1;
const CLASS_Z16: u8 = 2;
const CLASS_S8Z24: u8 = 3;
const CLASS_Z24S8: u8 = 4;
const CLASS_ZF32_X24S8: u8 = 5;

// The kind field of a page table entry for each, which these three generations share.
const KIND_Z16: u32 = 0x01;
const KIND_S8: u32 = 0x02;
const KIND_S8Z24: u32 = 0x03;
const KIND_ZF32_X24S8: u32 = 0x04;
const KIND_Z24S8: u32 = 0x05;

/// The page kind of each depth class.
const CLASS_KINDS: [(u8, u32); 5] = [
    (CLASS_S8, KIND_S8),
    (CLASS_Z16, KIND_Z16),
    (CLASS_S8Z24, KIND_S8Z24),
    (CLASS_Z24S8, KIND_Z24S8),
    (CLASS_ZF32_X24S8, KIND_ZF32_X24S8),
];

/// Whether a generation's page table entries carry a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKinds {
    /// Pages of depth memory need the kind of their packing. Turing, Ampere and Ada.
    InEntries,
    /// No kind in the entries, so a map has none to set. Blackwell.
    NotCarried,
}

/// The depth class a driver protection value names, or zero for none.
pub fn depth_class(driver_protection: u64) -> u8 {
    (driver_protection >> CLASS_SHIFT & CLASS_MASK) as u8
}

/// The page kind a mapping with this driver protection value needs, if it is a depth mapping on a
/// generation whose entries carry a kind. A class this table doesn't know gets none, and is logged.
pub fn page_kind(rule: PageKinds, driver_protection: u64) -> Option<u32> {
    let class = depth_class(driver_protection);
    if rule == PageKinds::NotCarried || class == CLASS_NONE {
        return None;
    }
    let kind = CLASS_KINDS.iter().find(|known| known.0 == class).map(|known| known.1);
    if kind.is_none() {
        log::warn!("a map asks for depth class {class}, which has no page kind here, so it stays plain");
    }
    kind
}

#[cfg(test)]
mod tests {
    use super::*;
    use PageKinds::{InEntries, NotCarried};

    const fn with_class(class: u64) -> u64 {
        class << CLASS_SHIFT
    }

    #[test]
    fn each_depth_class_has_its_kind() {
        for (class, kind) in [(1, 0x02), (2, 0x01), (3, 0x03), (4, 0x05), (5, 0x04)] {
            assert_eq!(page_kind(InEntries, with_class(class)), Some(kind), "class {class}");
        }
    }

    #[test]
    fn only_the_class_byte_counts() {
        let others = 0xFFFF_FFFF_FFFF_00FF;
        assert_eq!(page_kind(InEntries, with_class(4) | others), Some(KIND_Z24S8));
        assert_eq!(page_kind(InEntries, others), None);
    }

    #[test]
    fn a_map_with_no_class_or_an_unknown_one_gets_no_kind() {
        assert_eq!(page_kind(InEntries, 0), None);
        assert_eq!(page_kind(InEntries, with_class(6)), None);
        assert_eq!(page_kind(InEntries, with_class(0xFF)), None);
    }

    #[test]
    fn a_generation_without_kinds_in_its_entries_gets_none() {
        for class in 0..=5 {
            assert_eq!(page_kind(NotCarried, with_class(class)), None, "class {class}");
        }
    }
}
