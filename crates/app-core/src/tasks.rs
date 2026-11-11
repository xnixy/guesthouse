//! Tasks as the app sees them: the small file each one writes, what is going on with a machine, and
//! starting one. Actions themselves are in [`crate::actions`].

use crate::context::{Context, SLOT_VARIABLE};
use crate::daemon::detached;
use crate::display::{read_state, ring_in, screen_text};
use crate::failure::{Failure, Result};
use crate::gpu_identity::read_state as read_identity;
use crate::gpu_maker::GpuMaker;
use crate::jailing;
use crate::linux::amd::{amd_mode, fallback_reason};
use crate::linux::{amd_says, guest_says};
use crate::log::open_for_appending;
use crate::machine::Machine;
use crate::process_stat::{self, Seen};
use crate::processes::{leftovers, memory_taken, running_machine, running_slots, still_running};
use crate::render_cards::render_card;
use crate::settings::write_json;
use crate::setup::cards::card_in_use_note;
use crate::slots;
use guesthouse_daemon::gpu_note::read_note;
use guesthouse_daemon::slot::{SLOTS, Slot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub const TASK_PROGRAM: &str = "guesthouse-task";
const TASK_FILE: &str = "guesthouse-task.json";
const STILL_BUSY: &str = "Something else is still in progress.";
const QEMU_PAUSED_WORDS: [&str; 2] = ["paused", "debug"];
const TASK_DIED: &str = "The task stopped before it finished. The log has more.";
/// How a machine is told it would be one too many.
/// Why there's no slot. Slots and their shared memory are the whole PC's, so another user's or another
/// install's VM really does take one, and it's only fair to say whose they are.
pub fn no_slot_text(context: &Context) -> String {
    let ours = running_slots(context).len();
    match (SLOTS as usize).saturating_sub(ours) {
        0 => format!("{SLOTS} VMs are running, which is as many as Guesthouse runs at once."),
        others => {
            format!("All {SLOTS} slots are taken, {others} of them by another user or install on this PC.")
        }
    }
}

/// Whether QEMU's word for the guest means it was left stopped.
pub fn is_paused(state: &str) -> bool {
    QEMU_PAUSED_WORDS.contains(&state)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub machine: String,
    /// Zero once it has finished, so a pid left here means it was killed and couldn't say so.
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Off,
    Starting,
    Installing,
    Running,
    Stopping,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Starting => "Starting",
            Self::Installing => "Installing",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// What is happening now.
    pub step: String,
    /// What went wrong last.
    pub error: String,
    /// A task for this machine is running.
    pub busy: bool,
    /// Which VM it is, if it's running or has a task.
    pub slot: Option<u8>,
    /// The VM's QEMU, while it's running.
    pub qemu: Option<Seen>,
    /// What's really on the guest's screen, in words, while it's up.
    pub screen: String,
    /// Why the screen isn't what was asked for.
    pub display_problem: String,
    /// What stands in the way of the card maker's vendor ID, or when it comes.
    pub identity_note: String,
    /// What the running machine's jail is missing, if anything.
    pub jail_note: String,
    /// Which card a GPU machine runs on, on a PC with more than one NVIDIA card.
    pub card_note: String,
    /// What a running Linux guest last said about its GPU.
    pub guest_gpu: String,
    /// The running Linux guest said its desktop is on the plain screen this boot.
    pub plain_desktop: bool,
}

pub fn task_path(context: &Context, slot: Option<Slot>) -> PathBuf {
    context.run_folder(slot).join(TASK_FILE)
}

pub fn read_task(context: &Context, slot: Option<Slot>) -> TaskRecord {
    let text = fs::read_to_string(task_path(context, slot)).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn write_task(context: &Context, slot: Option<Slot>, record: &TaskRecord) -> Result<()> {
    write_json(&task_path(context, slot), record)
}

pub fn task_alive(context: &Context, record: &TaskRecord) -> bool {
    let file = context.processes.join(record.pid.to_string()).join("cmdline");
    let words = fs::read(file).unwrap_or_default();
    words.windows(TASK_PROGRAM.len()).any(|part| part == TASK_PROGRAM.as_bytes())
}

/// Whether a task's process has really gone. `task_alive` can't say: `spawn` returns while the kernel
/// is still loading the task, and until that's done its name isn't there to find.
fn task_ended(context: &Context, record: &TaskRecord) -> bool {
    process_stat::has_ended(process_stat::read(context, record.pid).as_deref())
}

/// The slot this machine is running in or has a task in.
pub fn slot_of(context: &Context, machine: &Machine) -> Option<Slot> {
    running_machine(context, machine).map(|(number, _)| slots::number(number)).or_else(|| {
        slots::all().find(|slot| {
            let task = read_task(context, Some(*slot));
            task.machine == machine.id && task_alive(context, &task)
        })
    })
}

/// The lowest slot with no VM, no task, nothing left behind and no other VM on its memory.
pub fn free_slot(context: &Context) -> Option<Slot> {
    let running = running_slots(context);
    let is_free = |slot: &Slot| {
        !running.contains_key(&slot.number())
            && !task_alive(context, &read_task(context, Some(*slot)))
            && !memory_taken(context, Some(*slot))
            && leftovers(context, Some(*slot)).is_none()
    };
    // A window started with a slot named keeps to it, so it can stay clear of the others.
    if context.slot.number() != slots::first().number() {
        return Some(context.slot).filter(is_free);
    }
    slots::all().find(is_free)
}

/// What a guest on an AMD card said of its GPU, which is also kept for its next start: see
/// [`amd_says::learn`]. Before it says anything, a machine on Venus says why it is.
fn amd_guest_gpu(context: &Context, machine: &Machine, run_folder: Option<&Path>) -> String {
    let mode = amd_mode(context, machine);
    match run_folder.and_then(amd_says::read) {
        Some(says) => {
            amd_says::learn(context, machine, mode, &says);
            says.text(mode)
        }
        None => fallback_reason(context, machine)
            .map(|reason| format!("The guest is on Venus, as native context failed: {reason}"))
            .unwrap_or_default(),
    }
}

/// What is going on with a machine, and if its GPU has stopped, that comes first.
pub fn status(context: &Context, machine: &Machine) -> Status {
    status_since(context, machine, None)
}

/// The same, for a machine whose QEMU was seen in a slot at the last look. While that process is
/// going and has words the machine is running, whatever those words read as now.
pub fn status_since(context: &Context, machine: &Machine, before: Option<(u8, Seen)>) -> Status {
    let mut found = what_is_running(context, machine, before);
    let folder = found.slot.map(|number| context.run_folder(Some(slots::number(number))));
    if found.state == State::Running
        && let Some(note) = folder.as_ref().and_then(|folder| read_note(folder))
    {
        found.error = note;
    }
    if found.state == State::Running && machine.amd_card().is_some() {
        // A guest on an AMD card shows its desktop on QEMU's display, whatever its GPU does.
        found.plain_desktop = true;
        found.guest_gpu = amd_guest_gpu(context, machine, folder.as_deref());
    } else if found.state == State::Running && (machine.own_linux_gpu() || machine.wants_forwarded_gpu()) {
        let said = folder.as_deref().and_then(guest_says::read);
        found.guest_gpu = said.as_ref().map(guest_says::GuestSays::text).unwrap_or_default();
        found.plain_desktop = said.is_some_and(|said| said.plain_desktop());
    }
    if machine.has_gpu() {
        let shown = read_state(context, machine);
        found.display_problem = shown.problem(found.state == State::Running).to_owned();
        let identity = read_identity(context, machine);
        found.identity_note = identity.problem(found.state == State::Running).to_owned();
        found.card_note = match machine.windows_maker() {
            Some(maker @ (GpuMaker::Amd | GpuMaker::Intel)) => card_note(context, maker, &machine.amd_gpu),
            Some(GpuMaker::Nvidia) | None => card_in_use_note(context).unwrap_or_default(),
        };
        if found.state == State::Running {
            let slot = found.slot.map(slots::number);
            found.screen = screen_text(&shown, ring_in(context, slot));
        }
    }
    if found.state == State::Running {
        found.jail_note = folder.as_deref().and_then(jailing::read_note).unwrap_or_default();
    }
    found
}

/// Which card a Windows machine runs on, as a machine on a card other than NVIDIA's is the rarer case.
fn card_note(context: &Context, maker: GpuMaker, pci: &str) -> String {
    render_card(context, maker, pci).map_or_else(
        || format!("Runs on the {} card at {pci}.", maker.name()),
        |card| format!("Runs on the {} at {pci}.", card.name),
    )
}

/// Whether the task in `read` ended without finishing. A task's last act is to clear its pid, so this
/// looks again once it has ended: after that it can't write anything more.
fn task_died(context: &Context, number: u8, read: &TaskRecord) -> bool {
    read.pid != 0
        && task_ended(context, read)
        && read_task(context, Some(slots::number(number))).pid == read.pid
}

/// Looks at what is really running, so a VM started some other way counts like any other.
fn what_is_running(context: &Context, machine: &Machine, before: Option<(u8, Seen)>) -> Status {
    let found = running_machine(context, machine).map(|(number, qemu)| (number, qemu.seen));
    let running = found.or_else(|| before.filter(|(_, qemu)| still_running(context, *qemu)));
    let (slot, qemu) = running.unzip();
    let state = if slot.is_some() { State::Running } else { State::Off };
    let tasks: BTreeMap<u8, TaskRecord> =
        slots::all().map(|number| (number.number(), read_task(context, Some(number)))).collect();
    let mine: Vec<u8> =
        tasks.iter().filter(|(_, task)| task.machine == machine.id).map(|(number, _)| *number).collect();
    let Some(first) = mine.first() else { return Status { state, slot, qemu, ..Status::default() } };
    // A finished task's words stay in its slot, and the slot it's running in counts first.
    let number = slot.filter(|running| mine.contains(running)).unwrap_or(*first);
    let task = &tasks[&number];
    if !task_alive(context, task) {
        let died = if task_died(context, number, task) { TASK_DIED } else { "" };
        let error = if task.error.is_empty() { died.to_owned() } else { task.error.clone() };
        return Status { state, error, slot, qemu, ..Status::default() };
    }
    let state = match task.action.as_str() {
        "start" => State::Starting,
        "install" => State::Installing,
        "stop" | "force" | "clean_up" => State::Stopping,
        _ => state,
    };
    let (step, error) = (task.step.clone(), task.error.clone());
    Status { state, step, error, busy: true, slot: Some(number), qemu, ..Status::default() }
}

/// Starts a task that will outlive the app, and returns at once. It runs in the machine's slot, or
/// the one named, or a free one. Everything it does follows from that.
pub fn launch(
    context: &Context,
    action: &str,
    machine: &Machine,
    arguments: &[String],
    slot: Option<Slot>,
) -> Result<Child> {
    let slot = slot
        .or_else(|| slot_of(context, machine))
        .or_else(|| if context.second_instance() { Some(context.slot) } else { free_slot(context) })
        .ok_or_else(|| Failure(no_slot_text(context)))?;
    let task = read_task(context, Some(slot));
    let mut arguments = arguments.to_vec();
    if task_alive(context, &task) {
        if action != "force" {
            return Err(Failure::new(STILL_BUSY));
        }
        arguments = vec![task.pid.to_string()];
    }
    context.make_run_folder(Some(slot))?;
    let log = open_for_appending(context)?;
    let process = detached(
        Command::new(context.paths.binary(TASK_PROGRAM)).arg(action).arg(&machine.id).args(arguments),
    )
    .env(SLOT_VARIABLE, slot.number().to_string())
    .stdin(Stdio::null())
    .stdout(log.try_clone()?)
    .stderr(log)
    .spawn()?;
    let record = TaskRecord {
        action: action.to_owned(),
        machine: machine.id.clone(),
        pid: process.id(),
        step: "Getting ready".to_owned(),
        error: String::new(),
    };
    write_task(context, Some(slot), &record)?;
    Ok(process)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_stat::stand_in;
    use crate::processes::stand_in::fake_process;
    use crate::testing::{Scratch, scratch_context};
    use guesthouse_daemon::gpu_note::write_note;

    fn machine() -> Machine {
        Machine::new("sample", "Sample", "/d", "/v")
    }

    fn record(machine: &str, pid: u32, error: &str) -> TaskRecord {
        TaskRecord {
            action: "start".into(),
            machine: machine.into(),
            pid,
            step: "x".into(),
            error: error.into(),
        }
    }

    fn running_vm(context: &Context, vm: &Machine) {
        let qemu = context.paths.qemu.display().to_string();
        let monitor = format!("unix:{}/qmp.sock,server=on", context.run_folder(None).display());
        let disk = format!("file={},format=qcow2", vm.disk);
        fake_process(context, 40, &[&qemu, "-qmp", &monitor, &disk], &[], "");
    }

    #[test]
    fn the_card_note_names_the_card_by_its_maker_and_still_says_so_when_the_card_is_gone() {
        use crate::linux::amd::stand_in::fake_render_node;
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.drm_class = scratch.path("drm");
        const NAMES: &str = "1001  Other\n8086  Intel Corporation\n\t64a0  Lunar Lake [Intel Arc 140V]\n";
        context.pci_names = scratch.write("pci.ids", NAMES);
        fake_render_node(&scratch, 128, "0000:00:02.0", "0x8086", 0x64a0);
        assert_eq!(
            card_note(&context, GpuMaker::Intel, "0000:00:02.0"),
            "Runs on the Intel Arc 140V at 0000:00:02.0."
        );
        assert_eq!(
            card_note(&context, GpuMaker::Amd, "0000:00:02.0"),
            "Runs on the AMD card at 0000:00:02.0.",
            "an Intel card is not found as an AMD one"
        );
    }

    #[test]
    fn a_vm_whose_gpu_has_stopped_says_so_on_its_row() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        context.make_run_folder(None).unwrap();
        let vm = machine();
        running_vm(&context, &vm);
        assert_eq!(status(&context, &vm).error, "");
        write_note(&context.run_folder(None), true).unwrap();
        let found = status(&context, &vm);
        assert_eq!(found.state, State::Running);
        assert!(found.error.contains("GPU has stopped"), "{found:?}");
    }

    #[test]
    fn a_linux_guest_on_its_own_plain_screen_says_so_on_its_row() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        context.make_run_folder(None).unwrap();
        let vm = Machine { kind: crate::machine::Kind::Plain, linux_gpu: true, ..machine() };
        running_vm(&context, &vm);
        let serial = context.run_folder(None).join(guest_says::SERIAL_LOG);
        fs::write(&serial, "nvfwd-gpu on 615.78.08\nnvfwd-desktop gpu\n").unwrap();
        assert!(!status(&context, &vm).plain_desktop);
        fs::write(&serial, "nvfwd-gpu on 615.78.08\nnvfwd-desktop plain it never lit\n").unwrap();
        let found = status(&context, &vm);
        assert!(found.plain_desktop);
        assert!(found.guest_gpu.ends_with("plain screen: it never lit"), "{found:?}");
        assert!(!status(&context, &machine()).plain_desktop, "only a Linux machine's guest is heard");
    }

    #[test]
    fn a_guest_on_an_amd_card_is_heard_and_a_failure_is_kept_for_its_next_start() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        context.make_run_folder(None).unwrap();
        let vm = Machine { kind: crate::machine::Kind::Linux, amd_gpu: "0000:7c:00.0".into(), ..machine() };
        fs::create_dir_all(vm.folder(&context)).unwrap();
        running_vm(&context, &vm);
        let serial = context.run_folder(None).join(guest_says::SERIAL_LOG);
        fs::write(&serial, "guesthouse-amd building 5 10\n").unwrap();
        let found = status(&context, &vm);
        assert!(found.plain_desktop && found.guest_gpu.contains("5 of 10"), "{found:?}");
        fs::write(&serial, "guesthouse-amd failed eglinfo found llvmpipe\n").unwrap();
        assert!(status(&context, &vm).guest_gpu.ends_with("uses Venus."));
        assert_eq!(amd_mode(&context, &vm), crate::linux::amd::AmdMode::Venus);
        fs::write(&serial, "").unwrap();
        assert!(status(&context, &vm).guest_gpu.ends_with("failed: eglinfo found llvmpipe"));
    }

    #[test]
    fn a_vm_seen_running_stays_running_until_its_qemu_ends() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let vm = machine();
        running_vm(&context, &vm);
        let seen = status(&context, &vm);
        assert_eq!((seen.state, seen.slot), (State::Running, Some(0)));
        let before = seen.slot.zip(seen.qemu);
        fs::write(context.processes.join("40/cmdline"), "misread\0").unwrap();
        assert_eq!(status(&context, &vm).state, State::Off, "with nothing known, the words decide");
        let found = status_since(&context, &vm, before);
        assert_eq!((found.state, found.slot), (State::Running, Some(0)));
        stand_in::write_stat(&context, 40, 'Z', 40);
        assert_eq!(status_since(&context, &vm, before).state, State::Off);
    }

    #[test]
    fn a_vm_whose_qemu_is_ending_is_off_at_the_first_look() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let vm = machine();
        running_vm(&context, &vm);
        let seen = status(&context, &vm);
        // QEMU gives its words back first, and can take a while over the VM's memory after that.
        fs::write(context.processes.join("40/cmdline"), "").unwrap();
        assert_eq!(status_since(&context, &vm, seen.slot.zip(seen.qemu)).state, State::Off);
    }

    #[test]
    fn a_note_left_by_a_vm_that_has_gone_is_not_shown() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        context.make_run_folder(None).unwrap();
        write_note(&context.run_folder(None), true).unwrap();
        assert_eq!(status(&context, &machine()).error, "");
    }

    #[test]
    fn a_window_with_a_slot_named_keeps_to_it() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        assert_eq!(free_slot(&context).map(Slot::number), Some(0));
        context.slot = slots::number(3);
        assert_eq!(free_slot(&context).map(Slot::number), Some(3));
    }

    #[test]
    fn a_finished_task_leaves_its_error_for_its_own_machine() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        assert_eq!(status(&context, &machine()).error, "");
        write_task(&context, None, &record("sample", 0, "It broke.")).unwrap();
        let found = status(&context, &machine());
        assert!(found.error == "It broke." && !found.busy);
        let other = Machine { id: "other".into(), ..machine() };
        assert_eq!(status(&context, &other).error, "", "an error belongs to its own machine");
    }

    #[test]
    fn only_a_task_process_counts_as_a_task() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        fake_process(&context, 77, &["/bin/something"], &[], "");
        write_task(&context, None, &record("sample", 77, "")).unwrap();
        assert!(!status(&context, &machine()).busy);
    }

    #[test]
    fn a_task_that_dies_without_a_word_does_not_look_finished() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        write_task(&context, None, &record("sample", (1 << 22) + 1, "")).unwrap();
        let found = status(&context, &machine());
        assert!(!found.busy && found.error.contains("before it finished"));
        write_task(&context, None, &record("sample", 0, "")).unwrap();
        assert_eq!(status(&context, &machine()).error, "", "one that finished said all it had to");
    }

    #[test]
    fn a_task_still_being_loaded_has_not_died() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        write_task(&context, None, &record("sample", 79, "")).unwrap();
        for words in [&["/usr/bin/guesthouse"][..], &[""][..]] {
            fake_process(&context, 79, words, &[], "");
            fs::write(context.processes.join("79/stat"), "79 (a) b) R 1 79").unwrap();
            assert_eq!(status(&context, &machine()).error, "", "{words:?}");
        }
    }

    #[test]
    fn a_task_killed_and_not_yet_collected_has_died() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        write_task(&context, None, &record("sample", 79, "")).unwrap();
        fake_process(&context, 79, &[""], &[], "");
        fs::write(context.processes.join("79/stat"), "79 (guesthouse-task) Z 1 79").unwrap();
        assert_eq!(status(&context, &machine()).error, TASK_DIED);
    }

    #[test]
    fn a_task_that_finishes_just_after_its_record_is_read_has_not_died() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let read_while_running = record("sample", (1 << 22) + 1, "");
        write_task(&context, None, &record("sample", 0, "")).unwrap();
        assert!(!task_died(&context, 0, &read_while_running));
    }

    #[test]
    fn a_live_task_says_what_it_is_doing() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        fake_process(
            &context,
            78,
            &["/usr/libexec/guesthouse/guesthouse-task", "install", "sample"],
            &[],
            "",
        );
        let install = TaskRecord { action: "install".into(), ..record("sample", 78, "") };
        write_task(&context, Some(slots::number(1)), &install).unwrap();
        let found = status(&context, &machine());
        assert_eq!((found.state, found.busy, found.slot), (State::Installing, true, Some(1)));
        assert_eq!(found.step, "x");
        assert_eq!(slot_of(&context, &machine()), Some(slots::number(1)));
    }

    #[test]
    fn a_running_machine_is_found_in_its_slot() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let qemu = context.paths.qemu.display().to_string();
        let socket =
            format!("unix:{}/qmp.sock,server=on", context.paths.run_folder(slots::number(1)).display());
        fake_process(&context, 90, &[&qemu, "-qmp", &socket, "-drive", "file=/d,format=qcow2"], &[], "");
        assert_eq!(slot_of(&context, &machine()), Some(slots::number(1)));
        let found = status(&context, &machine());
        assert_eq!((found.state, found.slot), (State::Running, Some(1)));
        assert_eq!(free_slot(&context), Some(slots::first()), "the lowest one that is free");
    }

    #[test]
    fn a_slot_held_by_another_vm_is_not_free() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        assert_eq!(free_slot(&context), Some(slots::first()));
        let memory = context.paths.shared_file("ram", slots::first());
        fs::write(&memory, "").unwrap();
        fake_process(
            &context,
            201,
            &["/other/guesthouse-daemon", "/other"],
            &[],
            &memory.display().to_string(),
        );
        assert_eq!(free_slot(&context), Some(slots::number(1)));
    }

    #[test]
    fn a_busy_slot_refuses_a_second_task() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        fake_process(&context, 79, &["guesthouse-task", "start", "sample"], &[], "");
        write_task(&context, None, &record("sample", 79, "")).unwrap();
        let error = launch(&context, "start", &machine(), &[], Some(slots::first())).unwrap_err();
        assert_eq!(error.0, STILL_BUSY);
    }
}
