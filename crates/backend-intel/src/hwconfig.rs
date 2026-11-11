//! The card's hardware configuration table, which xe passes on from the GuC as it is.
//!
//! The table is a list of key, length and value: a dword key, a dword length in dwords, then that
//! many dwords. The key numbers are Mesa's, from `src/intel/dev/intel_hwconfig_types.h`, which is
//! MIT. Only the keys the adapter record uses are named here; the probe prints the rest by number.

use crate::protocol::bytes::dword;

pub const MAX_SLICES_SUPPORTED: u32 = 1;
pub const MAX_DUAL_SUBSLICES_SUPPORTED: u32 = 2;
pub const MAX_NUM_EU_PER_DSS: u32 = 3;
pub const NUM_THREADS_PER_EU: u32 = 15;

/// The names of the keys above, for the probe.
const NAMES: [(u32, &str); 4] = [
    (MAX_SLICES_SUPPORTED, "max slices supported"),
    (MAX_DUAL_SUBSLICES_SUPPORTED, "max dual subslices supported"),
    (MAX_NUM_EU_PER_DSS, "max EUs per dual subslice"),
    (NUM_THREADS_PER_EU, "threads per EU"),
];

/// One entry of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: u32,
    pub values: Vec<u32>,
}

impl Entry {
    pub fn name(&self) -> Option<&'static str> {
        NAMES.iter().find(|(key, _)| *key == self.key).map(|(_, name)| *name)
    }
}

/// The whole table, as xe gave it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hwconfig {
    pub entries: Vec<Entry>,
}

impl Hwconfig {
    /// Reads entries until the bytes run out. An entry whose length runs past the end is dropped,
    /// with everything after it, as there is no telling where the next would start.
    pub fn parse(table: &[u8]) -> Self {
        let mut entries = Vec::new();
        let mut at = 0;
        while let (Some(key), Some(length)) = (dword(table, at), dword(table, at + 4)) {
            let start = at + 8;
            let values: Option<Vec<u32>> =
                (0..length as usize).map(|index| dword(table, start + index * 4)).collect();
            let Some(values) = values else { break };
            entries.push(Entry { key, values });
            at = start + length as usize * 4;
        }
        Self { entries }
    }

    /// The first value of a key, if the table has it.
    pub fn value(&self, key: u32) -> Option<u32> {
        self.entries.iter().find(|entry| entry.key == key)?.values.first().copied()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A table of the given keys, each with one value.
    pub fn table(pairs: &[(u32, u32)]) -> Vec<u8> {
        pairs.iter().flat_map(|&(key, value)| [key, 1, value]).flat_map(u32::to_le_bytes).collect()
    }

    #[test]
    fn entries_are_read_by_their_length() {
        let mut bytes = table(&[(MAX_SLICES_SUPPORTED, 1)]);
        bytes.extend([99u32, 2, 7, 8].iter().flat_map(|word| word.to_le_bytes()));
        bytes.extend(table(&[(NUM_THREADS_PER_EU, 8)]));
        let config = Hwconfig::parse(&bytes);
        assert_eq!(config.entries.len(), 3);
        assert_eq!(config.entries[1].values, [7, 8]);
        assert_eq!(config.value(NUM_THREADS_PER_EU), Some(8));
        assert_eq!(config.entries[2].name(), Some("threads per EU"));
        assert_eq!(config.value(MAX_NUM_EU_PER_DSS), None);
    }

    #[test]
    fn an_entry_running_past_the_end_stops_the_table() {
        let mut bytes = table(&[(MAX_SLICES_SUPPORTED, 1)]);
        bytes.extend([5u32, 3, 1].iter().flat_map(|word| word.to_le_bytes()));
        assert_eq!(Hwconfig::parse(&bytes).entries.len(), 1);
    }
}
