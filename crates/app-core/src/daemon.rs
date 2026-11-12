//! How the daemon is started and recognised. Its start-up is changing, so this is the one place that
//! knows it: `guesthouse-daemon BASE`, with the slot in `GUESTHOUSE_SLOT`, serves `BASE/run[-N]`.

use crate::context::{Context, SLOT_VARIABLE};
use crate::failure::Result;
use crate::guest_memory::GuestMemory;
use guesthouse_daemon::memory_files::{MemoryFiles, RAM_VARIABLE, WINDOW_VARIABLE};
use guesthouse_daemon::slot::Slot;
use guesthouse_daemon::{card_choice, jail};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub const DAEMON_PROGRAM: &str = "guesthouse-daemon";
/// The sockets QEMU connects to, which the daemon makes once it's ready.
pub const SOCKETS: [&str; 2] = ["vgpu.sock", "global.sock"];
pub const OUTPUT_FILE: &str = "daemon.out";

/// What it takes to start the daemon for one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStart {
    pub program: PathBuf,
    pub base: PathBuf,
    pub environment: BTreeMap<String, String>,
}

impl DaemonStart {
    /// `ram` and `vram` are the daemon's words for the guest's memory, as `16G`.
    pub fn new(context: &Context, slot: Slot, ram: &str, vram: Option<&str>) -> Self {
        let mut environment = BTreeMap::from([
            (SLOT_VARIABLE.to_owned(), slot.number().to_string()),
            ("GUESTHOUSE_RAM".to_owned(), ram.to_owned()),
        ]);
        if let Some(vram) = vram {
            environment.insert("GUESTHOUSE_VRAM".into(), vram.to_owned());
        }
        Self {
            program: context.paths.binary(DAEMON_PROGRAM),
            base: context.paths.runtime.clone(),
            environment,
        }
    }

    /// For a guest on an AMD card, which the daemon then opens instead of the NVIDIA card.
    pub fn on_amd_card(mut self, pci: &str) -> Self {
        self.environment.insert(card_choice::AMD_CARD_VARIABLE.to_owned(), pci.to_owned());
        self
    }

    /// For a machine allowed to start with no jail, on a PC that can't give it one.
    pub fn without_jail(mut self) -> Self {
        self.environment.insert(jail::VARIABLE.to_owned(), jail::OFF.to_owned());
        self
    }

    /// For a daemon that is not started with the memory open, but told the two files to make and
    /// hold: see [`MemoryFiles`].
    pub fn with_memory_files(mut self, files: &MemoryFiles) -> Self {
        self.environment.insert(RAM_VARIABLE.to_owned(), files.ram.display().to_string());
        self.environment.insert(WINDOW_VARIABLE.to_owned(), files.window.display().to_string());
        self
    }

    /// Starts it with the guest's memory open, where it looks for it.
    pub fn spawn(&self, run_folder: &Path, memory: &GuestMemory) -> Result<Child> {
        let output = File::create(run_folder.join(OUTPUT_FILE))?;
        let mut command = Command::new(&self.program);
        memory.hand_to(&mut command);
        let spawned = detached(command.arg(&self.base))
            .envs(&self.environment)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?;
        Ok(spawned)
    }
}

/// A process group of its own, so that what it starts outlives the task, and the app can close at any
/// moment. A task can then also be stopped with everything it started.
pub fn detached(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    command.process_group(0)
}

fn words_of(file: &Path) -> Vec<Vec<u8>> {
    let data = fs::read(file).unwrap_or_default();
    data.split(|byte| *byte == 0).filter(|word| !word.is_empty()).map(<[u8]>::to_vec).collect()
}

/// Whether this process is this install's daemon for this slot. Its slot isn't in its arguments, so
/// that comes from its environment.
pub fn is_own_daemon(context: &Context, pid: &str, slot: Slot) -> bool {
    let folder = context.processes.join(pid);
    let words = words_of(&folder.join("cmdline"));
    let [program, base, ..] = &words[..] else { return false };
    let is_daemon = Path::new(OsStr::new(&*String::from_utf8_lossy(program))).ends_with(DAEMON_PROGRAM);
    let wanted = format!("{SLOT_VARIABLE}={}", slot.number());
    let names_slot = words_of(&folder.join("environ")).iter().any(|word| word == wanted.as_bytes());
    is_daemon && *base == context.paths.runtime.as_os_str().as_encoded_bytes() && names_slot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};

    #[test]
    fn the_daemon_gets_the_guests_memory_and_its_slot() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let start = DaemonStart::new(&context, slots::number(2), "16G", Some("8G"));
        assert_eq!(start.environment["GUESTHOUSE_SLOT"], "2");
        assert_eq!(
            (start.environment["GUESTHOUSE_RAM"].as_str(), start.environment["GUESTHOUSE_VRAM"].as_str()),
            ("16G", "8G")
        );
        assert_eq!(start.base, context.paths.runtime);
        assert_eq!(context.paths.run_folder(slots::number(2)), start.base.join("run-2"));
    }

    #[test]
    fn a_guest_on_an_amd_card_names_it_and_one_on_nvidia_names_none() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let start = DaemonStart::new(&context, slots::first(), "16G", None);
        assert!(!start.environment.contains_key("GUESTHOUSE_AMD_CARD"));
        let start = start.on_amd_card("0000:7c:00.0");
        assert_eq!(start.environment["GUESTHOUSE_AMD_CARD"], "0000:7c:00.0");
    }

    #[test]
    fn with_no_cap_the_daemon_is_told_none() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let start = DaemonStart::new(&context, slots::first(), "16G", None);
        assert!(!start.environment.contains_key("GUESTHOUSE_VRAM"));
    }
}
