//! What the tests share: scratch folders, and contexts that can't reach the real PC.

use crate::context::Context;
use crate::firmware::Firmware;
use crate::paths::{Paths, Variables};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// So that no test depends on how much this PC has free. One about the limit sets its own.
const PLENTY_OF_GIB: u64 = 64;

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A folder that goes when the test ends.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let folder = std::env::temp_dir().join(format!("guesthouse-test-{}-{number}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        Self(folder)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    pub fn root(&self) -> &Path {
        &self.0
    }

    /// Writes a program that can be run. A child writes it and not this process: a file this process
    /// has open for writing can be held open for a moment by a fork in another test, and then a run of
    /// it fails.
    pub fn program(&self, name: &str, text: &str) -> PathBuf {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut writer = Command::new("sh")
            .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        writer.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        assert!(writer.wait().unwrap().success());
        path
    }

    /// Writes a file, making its folder.
    pub fn write(&self, name: &str, data: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, data).unwrap();
        path
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Paths that all lie in the scratch folder, so that no test can touch a real machine, a real run
/// folder or real shared memory.
pub fn scratch_paths(scratch: &Scratch) -> Paths {
    let folder = |name: &str| scratch.path(name).to_str().unwrap().to_owned();
    let (home, shm, program) = (folder("home"), folder("shm"), folder("program"));
    // As a real program folder does, this means GPU machines are possible.
    fs::create_dir_all(scratch.path("program/guest")).unwrap();
    let runtime = folder("xdg-run");
    let mut paths = Paths::new(
        &Variables::of(&[
            ("HOME", &home),
            ("XDG_RUNTIME_DIR", &runtime),
            ("GUESTHOUSE_SHARED_MEMORY", &shm),
            ("GUESTHOUSE_PROGRAM", &program),
        ]),
        None,
    );
    paths.firmware = Firmware {
        code: scratch.path("firmware/code"),
        vars: scratch.write("firmware/vars", "blank firmware settings"),
        code_secure: scratch.path("firmware/code-secure.qcow2"),
        vars_secure: scratch.write("firmware/vars-secure", "firmware settings with keys"),
    };
    fs::create_dir_all(&paths.shared_memory).unwrap();
    paths
}

pub fn scratch_context(scratch: &Scratch) -> Context {
    let mut context = Context::new(scratch_paths(scratch), crate::slots::first());
    context.processes = scratch.path("proc");
    fs::create_dir_all(&context.processes).unwrap();
    // Nor does it have drives, or use any, unless a test says so.
    context.block_devices = scratch.path("block");
    context.drive_ids = scratch.path("drive-ids");
    context.mount_table = scratch.path("mountinfo");
    context.swaps = scratch.path("swaps");
    context.udev_data = scratch.path("udev");
    // A test's PC has no card unless it makes one, whatever cards this PC has.
    context.drm_class = scratch.path("drm");
    context.reserve_memory = crate::guest_memory::GuestMemory::sized;
    // A test's pretend programs are shell scripts, which no jail would let run as they do.
    context.jail_offer = |_| crate::jailing::Offer::nothing();
    context.ram_limit = Some(PLENTY_OF_GIB);
    context
}
