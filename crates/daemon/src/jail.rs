//! The daemon's jail, which it puts itself in. A guest that takes the daemon over gets what's here
//! and nothing more: the card, this VM's memory and run folder, and the system's libraries. The
//! memory comes open from the app, or is opened by the daemon first, from the files it is told of, so no
//! rule names it.
//!
//! It goes on in two steps. The files come first, before any thread exists, as Landlock covers only
//! the thread that applies it. The calls come once the card is open and the sockets are bound, as
//! the filter leaves no way to make a socket, and it covers every thread.

use guesthouse_jail::seccomp::{self, Threads, trap};
use guesthouse_jail::{Grant, Plan, Support, filters, landlock, only_thread};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// `off` starts the daemon with no jail. The app only sets it for a machine the user has allowed to
/// start without one. Anything else, or nothing, means a jail or no start.
pub const VARIABLE: &str = "GUESTHOUSE_JAIL";
pub const OFF: &str = "off";

const DEVICES: &str = "/dev";
const RENDER_NODES: &str = "/dev/dri";
const RENDER_NODE_PREFIX: &str = "renderD";
/// The control device, each card's, and the unified memory devices.
const NVIDIA_PREFIX: &str = "nvidia";
/// Display and partitioning nodes, which nothing here uses.
const NVIDIA_LEFT_OUT: [&str; 3] = ["nvidia-modeset", "nvidia-caps", "nvidia-nvswitchctl"];
const OWNER_ONLY: u32 = 0o600;

/// The system's libraries, which the management library is loaded from when this daemon's turn to
/// read the card comes, and `nvidia-smi`, which reads the card's rated clocks at the start. `/proc`
/// and `/sys` are read for the driver's version and the card's place, and reading `/proc` gives
/// nothing away, as Landlock doesn't let a jailed process look into another. A program it runs gets
/// `/dev/null` for its input.
const SYSTEM: [(Grant, &str); 5] = [
    (Grant::Run, "/usr"),
    (Grant::Read, "/etc/ld.so.cache"),
    (Grant::Read, "/sys"),
    (Grant::Read, "/proc"),
    (Grant::Write, "/dev/null"),
];

/// What this daemon touches, which the jail is made from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Needs {
    pub run_folder: PathBuf,
    /// Files it reads and writes but doesn't make, as the sensors' snapshot.
    pub shared_files: Vec<PathBuf>,
    /// The GPU's device files, which the stand-in has no use for.
    pub devices: Vec<PathBuf>,
    /// The guest's driver, from setup.
    pub driver_folder: Option<PathBuf>,
}

impl Needs {
    pub fn plan(&self) -> Plan {
        let mut plan = Plan::default();
        for (grant, path) in SYSTEM {
            plan.grant(grant, path);
        }
        for device in &self.devices {
            plan.grant(Grant::Device, device);
        }
        for file in &self.shared_files {
            plan.grant(Grant::Write, file);
        }
        if let Some(folder) = &self.driver_folder {
            plan.grant(Grant::Read, folder);
        }
        plan.grant(Grant::Own, &self.run_folder);
        plan
    }
}

fn names_in(folder: &Path) -> Vec<String> {
    let entries = fs::read_dir(folder).into_iter().flatten().flatten();
    let mut names: Vec<String> =
        entries.map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

/// The NVIDIA device files under `devices`, and the render nodes under `render_nodes`.
pub fn gpu_devices(devices: &Path, render_nodes: &Path) -> Vec<PathBuf> {
    let nvidia = names_in(devices).into_iter().filter(|name| {
        name.starts_with(NVIDIA_PREFIX) && !NVIDIA_LEFT_OUT.iter().any(|left| name.starts_with(left))
    });
    let mut found: Vec<PathBuf> = nvidia.map(|name| devices.join(name)).collect();
    let render = names_in(render_nodes).into_iter().filter(|name| name.starts_with(RENDER_NODE_PREFIX));
    found.extend(render.map(|name| render_nodes.join(name)));
    found
}

#[cfg(feature = "amd")]
impl Needs {
    /// What the AMD daemon of this slot needs. Only the one render node is granted, so the other
    /// cards' device files are not reachable from this process.
    pub fn of_amd_card(
        slot: crate::slot::Slot,
        project: &Path,
        driver_folder: &Path,
        render_node: PathBuf,
    ) -> Self {
        Self {
            run_folder: slot.run_folder(project),
            shared_files: Vec::new(),
            devices: vec![render_node],
            driver_folder: Some(driver_folder.to_owned()),
        }
    }
}

#[cfg(feature = "nvidia")]
impl Needs {
    /// What the real daemon of this slot needs.
    pub fn of_card(slot: crate::slot::Slot, project: &Path, driver_folder: &Path) -> Self {
        let snapshot = guesthouse_sensors::snapshot_path();
        Self {
            run_folder: slot.run_folder(project),
            shared_files: vec![snapshot],
            devices: host_gpu_devices(),
            driver_folder: Some(driver_folder.to_owned()),
        }
    }
}

impl Needs {
    /// The jail names each file, so each must be there before it: see [`make_if_missing`].
    pub fn prepare(&self) -> io::Result<()> {
        fs::create_dir_all(&self.run_folder)?;
        self.shared_files.iter().try_for_each(|file| make_if_missing(file))
    }
}

/// The GPU's device files on this PC.
pub fn host_gpu_devices() -> Vec<PathBuf> {
    gpu_devices(Path::new(DEVICES), Path::new(RENDER_NODES))
}

/// A rule can only name a file that's there, so one another process fills later is made here, empty,
/// if it isn't yet.
pub fn make_if_missing(path: &Path) -> io::Result<()> {
    let made = OpenOptions::new().write(true).create_new(true).mode(OWNER_ONLY).open(path);
    match made {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

fn wanted() -> bool {
    std::env::var(VARIABLE).as_deref() != Ok(OFF)
}

/// Why the daemon won't start. It says how to start it anyway, as only the user can choose that.
fn refusal(reason: &str) -> String {
    format!(
        "The daemon can't be jailed here: {reason} A machine can be allowed to start without a jail in its \
         settings."
    )
}

/// Whether the jail is on, which [`lock_calls`] needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locked(bool);

/// The first step: the files. Call it before anything starts a thread.
pub fn lock_files(needs: &Needs) -> Result<Locked, String> {
    if !wanted() {
        log::warn!("not jailed, as {VARIABLE} is {OFF}");
        return Ok(Locked(false));
    }
    if let Some(reason) = Support::probe().refusal() {
        return Err(refusal(reason));
    }
    if !only_thread().map_err(|error| refusal(&error.to_string()))? {
        return Err(refusal("it already has threads, which Landlock wouldn't cover."));
    }
    let applied = landlock::apply(&needs.plan()).map_err(|error| refusal(&format!("{error}.")))?;
    let missing = applied.missing();
    if missing.is_empty() {
        log::info!("jailed, with Landlock {}", applied.landlock_abi);
    } else {
        log::info!("jailed, with Landlock {}, which leaves out {}", applied.landlock_abi, missing.join(", "));
    }
    Ok(Locked(true))
}

/// The second step: the calls, for every thread. Call it once everything the daemon serves is open.
pub fn lock_calls(locked: Locked) -> Result<(), String> {
    if !locked.0 {
        return Ok(());
    }
    trap::report_refusals().map_err(|error| refusal(&format!("{error}.")))?;
    let filter = filters::daemon(std::process::id() as i32);
    seccomp::install(&filter, Threads::All).map_err(|error| refusal(&format!("{error}.")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gh-daemon-jail-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn the_card_and_its_render_node_and_nothing_else() {
        let folder = scratch("devices");
        let dri = folder.join("dri");
        fs::create_dir_all(&dri).unwrap();
        for name in ["nvidia0", "nvidiactl", "nvidia-uvm", "nvidia-modeset", "nvidia-caps", "sda", "kvm"] {
            fs::write(folder.join(name), "").unwrap();
        }
        for name in ["card0", "renderD128"] {
            fs::write(dri.join(name), "").unwrap();
        }
        let found = gpu_devices(&folder, &dri);
        let names: Vec<_> =
            found.iter().map(|path| path.strip_prefix(&folder).unwrap().display().to_string()).collect();
        assert_eq!(names, ["nvidia-uvm", "nvidia0", "nvidiactl", "dri/renderD128"]);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn its_own_folder_is_the_only_one_it_can_change() {
        let needs = Needs {
            run_folder: "/run/user/1000/guesthouse/run-2".into(),
            shared_files: vec!["/dev/shm/gpupv-sensors-1000-2".into()],
            devices: vec!["/dev/nvidiactl".into()],
            driver_folder: Some("/store/driver".into()),
        };
        let plan = needs.plan();
        let owned: Vec<_> = plan.paths.iter().filter(|(grant, _)| *grant == Grant::Own).collect();
        assert_eq!(owned, [&(Grant::Own, PathBuf::from("/run/user/1000/guesthouse/run-2"))]);
        assert!(
            plan.paths.contains(&(Grant::Write, "/dev/shm/gpupv-sensors-1000-2".into())),
            "the file, not /dev/shm"
        );
        assert!(!plan.paths.iter().any(|(_, path)| path == Path::new("/dev/shm")));
        assert!(plan.paths.contains(&(Grant::Read, "/store/driver".into())));
        assert_eq!(plan.network, guesthouse_jail::Network::none());
    }

    #[test]
    fn a_missing_file_is_made_and_one_that_is_there_is_kept() {
        let folder = scratch("snapshot");
        let path = folder.join("gpupv-sensors-1000-3");
        make_if_missing(&path).unwrap();
        fs::write(&path, "held").unwrap();
        make_if_missing(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "held");
        fs::remove_dir_all(folder).unwrap();
    }
}
