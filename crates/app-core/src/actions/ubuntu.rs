//! Installing and starting an Ubuntu guest. If the host can't honestly offer its driver, the guest
//! boots without the GPU and is told why.

use super::amd_start::{Waited, stop_for_venus, wait_for_guest};
use super::{KEY_POLL, Task, plan_for, start_if_there_is_room};
use crate::disk_image::disk_size_gib;
use crate::failure::{Failure, Result};
use crate::iso::kernel_package;
use crate::jailing::{decide, gpu_refusal};
use crate::library::make_disk;
use crate::linux::amd::{amd_card, amd_mode};
use crate::linux::amd_image::amd_setup_image;
use crate::linux::card::forwarded_card;
use crate::linux::driver_host::LoaderHost;
use crate::linux::driver_image::{Outcome, prepare};
use crate::linux::forwarding::{amd_problem, forwarding_problem};
use crate::linux::gpu_off::{
    GUEST_NO_AMD_CARD, GUEST_NO_CARD, GUEST_NO_IMAGE, GUEST_NO_JAIL, GUEST_TURNED_OFF, GpuOff,
};
use crate::linux::layout::ALLOW_LIST;
use crate::linux::seed::make_seed;
use crate::linux::{guest_sources, sensor_daemon};
use crate::network::{Protocol, port_taken};
use crate::start_checks::start_problems;
use crate::vm_args::{AmdBoot, Boot, INSTALL_LOG, UbuntuBoot, UbuntuInstall, VmPlan, ssh_port};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Only a finished install writes this line. The VM powers off either way.
const INSTALL_FINISHED: &[u8] = b"nvfwd-install-finished";
const INSTALL_HOURS: u64 = 2;
const REASON_FILE: &str = "gpu-off-reason";
const NO_CARD_DETAIL: &str = "The NVIDIA driver doesn't list the card setup chose, so it has no device file to open. \
                              Run Setup Again from the menu if the card has changed.";

fn isoinfo_files(iso: &str) -> Result<String> {
    let listing = Command::new("isoinfo").args(["-R", "-f", "-i", iso]).output()?;
    Ok(String::from_utf8_lossy(&listing.stdout).into_owned())
}

fn extract(iso: &str, name: &str, to: &Path) -> Result<()> {
    let data = Command::new("isoinfo").args(["-R", "-i", iso, "-x", name]).output()?.stdout;
    if data.is_empty() {
        return Err(Failure(format!("{iso} has no {name}, so it is not an Ubuntu server ISO.")));
    }
    fs::write(to, data)?;
    Ok(())
}

fn installed(serial: &[u8]) -> bool {
    serial.split(|byte| *byte == b'\n').any(|line| line.starts_with(INSTALL_FINISHED))
}

/// Takes a new machine from its ISO to its first boot. Each run starts again from nothing.
pub fn install(task: &mut Task) -> Result<()> {
    let context = task.context;
    if !task.machine.owns_files(context) {
        return Err(Failure::new(
            "This machine's disk is not in its folder, so nothing will install over it.",
        ));
    }
    let mut found = start_problems(context, &task.machine)?;
    found.retain(|problem| !problem.contains("is missing"));
    if !Path::new(&task.machine.iso).exists() {
        found.push(format!("{} is missing.", task.machine.iso));
    }
    if let Some(problem) = found.into_iter().next() {
        return Err(Failure(problem));
    }
    task.step("Preparing the install");
    let iso = task.machine.iso.clone();
    let listing = isoinfo_files(&iso)?;
    let package = listing.lines().find_map(kernel_package).map(str::to_owned);
    let package = package.ok_or_else(|| {
        Failure(format!("{iso} holds no generic kernel in its pool, so it is not an Ubuntu server ISO."))
    })?;
    let seed = make_seed(context, &task.machine, &package)?;
    let boot = task.machine.folder(context).join("boot");
    fs::create_dir_all(&boot)?;
    let (kernel, initrd) = (boot.join("vmlinuz"), boot.join("initrd"));
    extract(&iso, "/casper/vmlinuz", &kernel)?;
    extract(&iso, "/casper/initrd", &initrd)?;
    let size = disk_size_gib(&task.machine.disk)? as u32;
    fs::remove_file(&task.machine.disk)?;
    make_disk(context, &task.machine, size)?;
    task.step("Installing Ubuntu");
    let install = UbuntuInstall { iso, seed, kernel, initrd };
    let plan = plan_for(context, &task.machine, Boot::UbuntuInstall(install));
    task.host.start(&plan, &task.machine)?;
    wait_for_poweroff(task)?;
    let serial = fs::read(task.run_file(INSTALL_LOG)).unwrap_or_default();
    if !installed(&serial) {
        return Err(Failure(format!(
            "The install failed. See the end of {}",
            task.run_file(INSTALL_LOG).display()
        )));
    }
    let _ = fs::remove_dir_all(boot);
    task.machine.installing = false;
    task.machine.iso.clear();
    task.machine.save(context)?;
    start_if_there_is_room(task, "Ubuntu")
}

fn wait_for_poweroff(task: &Task) -> Result<()> {
    let end = task.now() + Duration::from_secs(INSTALL_HOURS * 3600);
    while task.pid().is_some() {
        if task.now() >= end {
            return Err(Failure(format!(
                "The install did not finish in {INSTALL_HOURS} hours. Show Screen says how far it got."
            )));
        }
        task.sleep(KEY_POLL);
    }
    Ok(())
}

/// A boot with no GPU, whose reason reaches the guest.
pub(super) fn refused(task: &Task, off: &GpuOff) -> Result<UbuntuBoot> {
    let reason_file = task.run_file(REASON_FILE);
    task.context.make_run_folder(None)?;
    fs::write(&reason_file, off.guest_text())?;
    Ok(UbuntuBoot::NoGpu { reason_file })
}

/// The GPU if the host can offer it, for Ubuntu and for a Linux the user installed alike.
pub(super) fn gpu_boot(task: &mut Task) -> Result<UbuntuBoot> {
    let context = task.context;
    let refused = |task: &mut Task, off: GpuOff| refused(task, &off);
    if !task.machine.gpu_preview {
        return refused(task, GpuOff::new(GUEST_TURNED_OFF, GUEST_TURNED_OFF));
    }
    if let Some(pci) = task.machine.amd_card().map(str::to_owned) {
        return match amd_boot(task, &pci) {
            Ok(amd) => Ok(UbuntuBoot::Amd(amd)),
            Err(off) => {
                task.error = format!("Starting without the GPU: {}", off.detail);
                refused(task, off)
            }
        };
    }
    let sources = guest_sources(context);
    let allow_list = sources.join(ALLOW_LIST);
    if let Some(off) = forwarding_problem(&context.paths.qemu, &allow_list) {
        task.error = format!("Starting without the GPU: {}", off.detail);
        return refused(task, off);
    }
    // It's fine if this isn't there: the device goes on without sensors, and logs why.
    let sensor_daemon = sensor_daemon(context);
    task.step("Getting the driver image ready");
    let store = &context.paths.store;
    let tree = env_folder("NVIDIA_SOURCE").unwrap_or_else(|| store.join("src/open-gpu-kernel-modules"));
    let host = LoaderHost::of_this_pc(context.driver_version_file.clone());
    match or_refused(prepare(&host, &sources, &tree, &store.join("linux-driver"))) {
        Outcome::Refused(off) => {
            task.error = format!("Starting without the GPU: {}", off.detail);
            refused(task, off)
        }
        Outcome::Ready { image, warning } => {
            let Some(card) = forwarded_card(context) else {
                let off = GpuOff::new(GUEST_NO_CARD, NO_CARD_DETAIL);
                task.error = format!("Starting without the GPU: {}", off.detail);
                return refused(task, off);
            };
            task.error = warning.unwrap_or_default();
            Ok(UbuntuBoot::WithGpu { image, allow_list, sensor_daemon, card })
        }
    }
}

/// The AMD card chosen for the machine, on native context unless the guest said that failed. A Linux
/// the user installed gets the scripts it sets itself up from.
fn amd_boot(task: &Task, pci: &str) -> std::result::Result<AmdBoot, GpuOff> {
    let context = task.context;
    if let Some(off) = amd_problem(&context.paths.qemu) {
        return Err(off);
    }
    let Some(card) = amd_card(context, pci) else {
        let detail = format!(
            "The AMD card at {pci} isn't in this PC now. Choose the GPU again in the machine's settings."
        );
        return Err(GpuOff::new(GUEST_NO_AMD_CARD, detail));
    };
    let setup_image = match task.machine.own_linux_gpu() {
        true => Some(setup_image(context).map_err(|failure| {
            GpuOff::new(GUEST_NO_IMAGE, format!("The setup image could not be made: {}", failure.0))
        })?),
        false => None,
    };
    Ok(AmdBoot { card, mode: amd_mode(context, &task.machine), setup_image })
}

fn setup_image(context: &crate::context::Context) -> Result<PathBuf> {
    let folder = context.paths.store.join("linux-driver");
    fs::create_dir_all(&folder)?;
    amd_setup_image(&guest_sources(context), &folder)
}

/// Anything that goes wrong making the image is a reason to lose the GPU, and never the VM.
fn or_refused(prepared: Result<Outcome>) -> Outcome {
    prepared.unwrap_or_else(|failure| {
        let detail = format!("The driver image could not be made: {}", failure.0);
        Outcome::Refused(GpuOff::new(GUEST_NO_IMAGE, detail))
    })
}

fn env_folder(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Boots it, with the GPU if that can be offered. The guest does the rest on its own.
pub fn start(task: &mut Task) -> Result<()> {
    if let Some(problem) = start_problems(task.context, &task.machine)?.into_iter().next() {
        return Err(Failure(problem));
    }
    // QEMU's own message wouldn't say what to do about it.
    let port = ssh_port(task.context.slot);
    if port_taken(Protocol::Tcp, port) {
        return Err(Failure(format!(
            "Port {port} on this PC is in use by another program, and Ubuntu's SSH needs it. Set SSH_PORT to another one."
        )));
    }
    let plan = start_vm(task)?;
    // On an AMD card the guest may have Mesa to build, and if native context fails it goes to Venus.
    if let Waited::OnVenusNext(reason) = wait_for_guest(task, &plan) {
        stop_for_venus(task, &reason)?;
        let plan = start_vm(task)?;
        wait_for_guest(task, &plan);
    }
    Ok(())
}

fn start_vm(task: &mut Task) -> Result<VmPlan> {
    let boot = gpu_boot(task)?;
    let plan = jailed(task, plan_for(task.context, &task.machine, Boot::Ubuntu(boot)))?;
    task.step("Starting the VM");
    task.host.start(&plan, &task.machine)?;
    Ok(plan)
}

/// The forwarded GPU only goes to a jailed QEMU, as on Windows. A PC that can't jail it starts the
/// guest without the GPU and says why, unless the machine may start without a jail.
pub(super) fn jailed(task: &mut Task, plan: VmPlan) -> Result<VmPlan> {
    if !plan.has_forwarded_gpu() && !plan.has_amd_gpu() {
        return Ok(plan);
    }
    match decide(task.context, &task.machine, &plan, &(task.context.jail_offer)(&plan)) {
        Ok(choice) => {
            let jail = choice.qemu.map(|jail| plan.with_backing_files(jail)).transpose()?;
            Ok(VmPlan { jail, jail_note: choice.note, ..plan })
        }
        Err(reason) => {
            let detail = gpu_refusal(reason);
            task.error = format!("Starting without the GPU: {detail}");
            let gpu = refused(task, &GpuOff::new(GUEST_NO_JAIL, detail))?;
            let boot = match plan.boot {
                Boot::OwnLinux { iso, .. } => Boot::OwnLinux { iso, gpu },
                _ => Boot::Ubuntu(gpu),
            };
            Ok(VmPlan { boot, ..plan })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::card::ForwardedCard;
    use crate::machine::{Kind, Machine};
    use crate::testing::{Scratch, scratch_context};
    use crate::vm_host::stand_in::RecordingHost;

    fn ubuntu(context: &crate::context::Context) -> Machine {
        let folder = context.paths.machines.join("ubuntu");
        fs::create_dir_all(&folder).unwrap();
        let disk = folder.join("disk.qcow2");
        fs::write(&disk, "").unwrap();
        let machine = Machine {
            kind: Kind::Linux,
            cores: 2,
            ram_gib: 2,
            ..Machine::new("ubuntu", "Ubuntu", &disk.to_string_lossy(), "")
        };
        machine.save(context).unwrap();
        machine
    }

    fn off_reason(task: &mut Task) -> String {
        let boot = gpu_boot(task).unwrap();
        let UbuntuBoot::NoGpu { reason_file } = boot else { panic!("it should have no GPU") };
        fs::read_to_string(reason_file).unwrap()
    }

    #[test]
    fn only_a_line_of_its_own_counts_as_finished() {
        assert!(installed(b"booting\nnvfwd-install-finished\npowering off\n"));
        assert!(!installed(b"echo nvfwd-install-finished > /dev/ttyS0\n"));
        assert!(!installed(b""));
    }

    #[test]
    fn a_machine_with_its_gpu_turned_off_tells_the_guest_why() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let vm = Machine { gpu_preview: false, ..ubuntu(&context) };
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", vm);
        assert!(off_reason(&mut task).starts_with(GUEST_TURNED_OFF));
    }

    #[test]
    fn a_qemu_with_no_device_boots_with_no_gpu() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", ubuntu(&context));
        assert!(off_reason(&mut task).contains("no nvfwd device"));
    }

    /// A folder where QEMU has the device and bwrap works, whatever this PC has.
    fn qemu_with_the_device(scratch: &Scratch, context: &mut crate::context::Context) {
        context.paths.qemu = scratch.program("qemu/qemu-system-x86_64", "#!/bin/sh\necho 'nvfwd options:'\n");
    }

    #[test]
    fn a_missing_sensor_daemon_is_not_what_turns_the_gpu_off() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        qemu_with_the_device(&scratch, &mut context);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", ubuntu(&context));
        assert!(!off_reason(&mut task).contains("no nvfwd device"));
    }

    #[test]
    fn host_paths_stay_with_the_host() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        qemu_with_the_device(&scratch, &mut context);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", ubuntu(&context));
        let said = off_reason(&mut task);
        assert!(!said.contains('/'), "{said}");
        assert!(task.error.contains("allow.txt"));
    }

    #[test]
    fn a_failure_making_the_image_only_turns_the_gpu_off() {
        let Outcome::Refused(off) =
            or_refused(Err(Failure::new("/home/me/store: gcc found no such header.")))
        else {
            panic!("it should have been refused")
        };
        assert!(off.detail.contains("gcc found no such header"));
        assert!(!off.guest_text().contains("gcc") && !off.guest_text().contains("/home"));
    }

    #[test]
    fn a_port_in_use_is_named_before_qemu_can_fail_on_it() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.cpus = crate::host::stand_in::fake_cpus(&scratch, "cpus", &[32768; 8]);
        context.meminfo =
            scratch.write("meminfo", "MemTotal:       100000000 kB\nMemAvailable:   100000000 kB\n");
        let listener = std::net::TcpListener::bind(("127.0.0.1", ssh_port(context.slot)));
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", ubuntu(&context));
        let found = start(&mut task);
        // Whoever holds the port, this test or another program, the words are the same. Another
        // program can let go of it first, as a second run of these tests does, and then it can start.
        if listener.is_ok() || found.is_err() {
            assert!(found.unwrap_err().0.contains("Set SSH_PORT"));
        }
    }

    fn with_the_gpu(task: &Task) -> VmPlan {
        let boot = UbuntuBoot::WithGpu {
            image: "/store/driver.iso".into(),
            allow_list: "/p/allow.txt".into(),
            sensor_daemon: "/p/nvfwd-sensord".into(),
            card: ForwardedCard { pci: "0000:02:00.0".into(), minor: 0, render_node: None },
        };
        plan_for(task.context, &task.machine, Boot::Ubuntu(boot))
    }

    fn a_kernel_with_landlock(_: &VmPlan) -> crate::jailing::Offer {
        crate::jailing::Offer {
            support: guesthouse_jail::Support { landlock_abi: 10, seccomp: true },
            ..crate::jailing::Offer::nothing()
        }
    }

    #[test]
    fn the_gpu_goes_to_a_jailed_qemu() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.jail_offer = a_kernel_with_landlock;
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", ubuntu(&context));
        let wanted = with_the_gpu(&task);
        let plan = jailed(&mut task, wanted).unwrap();
        assert!(plan.has_forwarded_gpu());
        let jail = plan.jail.as_ref().unwrap();
        assert!(!jail.namespaces && jail.launcher.ends_with("guesthouse-jail"));
        assert!(plan.jail_note.starts_with("Jailed, without namespaces"), "{}", plan.jail_note);
        assert_eq!(task.error, "");
    }

    #[test]
    fn a_pc_that_cant_jail_qemu_only_costs_the_gpu_unless_allowed() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", ubuntu(&context));
        let wanted = with_the_gpu(&task);
        let plan = jailed(&mut task, wanted).unwrap();
        let Boot::Ubuntu(UbuntuBoot::NoGpu { reason_file }) = &plan.boot else { panic!("{:?}", plan.boot) };
        assert!(fs::read_to_string(reason_file).unwrap().starts_with(GUEST_NO_JAIL));
        assert!(plan.jail.is_none());
        assert!(
            task.error.contains("Landlock") && task.error.contains("Start without a jail"),
            "{}",
            task.error
        );
        let allowed = Machine { start_without_jail: true, ..ubuntu(&context) };
        let mut task = Task::new(&context, &host, "start", allowed);
        let wanted = with_the_gpu(&task);
        let plan = jailed(&mut task, wanted).unwrap();
        assert!(plan.has_forwarded_gpu() && plan.jail.is_none());
        assert!(plan.jail_note.starts_with("Not jailed: "), "{}", plan.jail_note);
    }

    #[test]
    fn a_guest_with_no_gpu_needs_no_jail() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "start", ubuntu(&context));
        let boot = Boot::Ubuntu(UbuntuBoot::NoGpu { reason_file: "/r/reason".into() });
        let wanted = plan_for(&context, &task.machine, boot.clone());
        let plan = jailed(&mut task, wanted).unwrap();
        assert_eq!((plan.boot, plan.jail, plan.jail_note), (boot, None, String::new()));
    }

    #[test]
    fn an_iso_with_no_kernel_says_it_is_not_the_server_iso() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.cpus = crate::host::stand_in::fake_cpus(&scratch, "cpus", &[32768; 8]);
        context.meminfo =
            scratch.write("meminfo", "MemTotal:       100000000 kB\nMemAvailable:   100000000 kB\n");
        let iso = crate::iso::stand_in::make_iso(&scratch, "other", &[("boot/grub.cfg", b"linux")]);
        let vm = Machine { installing: true, iso, ..ubuntu(&context) };
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "install", vm);
        assert!(install(&mut task).unwrap_err().0.contains("not an Ubuntu server ISO"));
    }
}
