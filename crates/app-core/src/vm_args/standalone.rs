//! A GPU machine's words for a QEMU and a daemon that the app doesn't start, such as libvirt's. The
//! app gives them the guest's memory as descriptors that only its own children can have, and puts a
//! jail launcher in front of QEMU that opens its sockets. Here the memory is two files that both open
//! by path, QEMU runs as it is, and its sockets are paths. The words come from the same code as a
//! start from the app, so the two can't drift.

use super::VmPlan;
use guesthouse_daemon::memory_files::MemoryFiles;
use guesthouse_daemon::slot::Slot;
use std::path::Path;

const RAM_WORD: &str = "ram";
const WINDOW_WORD: &str = "window";
const FILE_SUFFIX: &str = ".standalone";

/// The two files in `folder`, named for the slot. They are not the names an older version kept its
/// memory under, which the app clears out.
pub fn memory_files_in(folder: &Path, slot: Slot) -> MemoryFiles {
    let file = |word: &str| folder.join(format!("{}{FILE_SUFFIX}", slot.memory_name(word)));
    MemoryFiles { ram: file(RAM_WORD), window: file(WINDOW_WORD) }
}

impl VmPlan {
    /// This plan with the memory in `files`, no jail around QEMU, and sound left silent. The
    /// daemon puts its own jail on whatever it is started with.
    pub fn standalone(self, files: MemoryFiles) -> Self {
        Self { memory_by_path: Some(files), jail: None, jail_note: String::new(), sound: None, ..self }
    }
}
