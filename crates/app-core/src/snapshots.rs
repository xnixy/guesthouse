//! Snapshots: the disk's own, with the firmware file and the TPM kept beside them.

use crate::actions::Task;
use crate::context::Context;
use crate::disk_image::disk_info;
use crate::failure::{Failure, Result};
use crate::machine::Machine;
use crate::processes::running_vms;
use crate::random::random_hex;
use crate::settings::with_suffix;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A name inside the qcow2 of ours, as qemu-img would read a name like 1 as a snapshot's number.
pub const SNAPSHOT_TAG: &str = "guesthouse-";
pub const MAX_SNAPSHOT_NAME: usize = 60;
const TAG_RANDOM_BYTES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub tag: String,
    pub name: String,
    /// Seconds since 1970.
    pub taken: i64,
}

pub fn snapshots_folder(context: &Context, machine: &Machine) -> PathBuf {
    machine.folder(context).join("snapshots")
}

pub fn snapshot_file(context: &Context, machine: &Machine, tag: &str, suffix: &str) -> PathBuf {
    snapshots_folder(context, machine).join(format!("{tag}{suffix}"))
}

/// Oldest first. Only what the disk holds and has its firmware file beside it: anything else was made
/// some other way, or is what a crash left of an unfinished snapshot.
pub fn list_snapshots(context: &Context, machine: &Machine) -> Vec<Snapshot> {
    if !snapshots_folder(context, machine).is_dir() {
        return Vec::new();
    }
    let Ok(info) = disk_info(&machine.disk) else { return Vec::new() };
    let empty = Vec::new();
    let entries = info["snapshots"].as_array().unwrap_or(&empty);
    let mut snapshots: Vec<Snapshot> = entries
        .iter()
        .filter_map(|entry| {
            let tag = entry["name"].as_str()?;
            let saved = fs::read_to_string(snapshot_file(context, machine, tag, ".json")).ok()?;
            let saved: Value = serde_json::from_str(&saved).ok()?;
            snapshot_file(context, machine, tag, ".fd").exists().then(|| Snapshot {
                tag: tag.to_owned(),
                name: saved["name"].as_str().unwrap_or_default().to_owned(),
                taken: entry["date-sec"].as_i64().unwrap_or(0),
            })
        })
        .collect();
    snapshots.sort_by_key(|snapshot| snapshot.taken);
    snapshots
}

fn find_snapshot(context: &Context, machine: &Machine, tag: &str) -> Result<Snapshot> {
    let found = list_snapshots(context, machine).into_iter().find(|snapshot| snapshot.tag == tag);
    found.ok_or_else(|| Failure::new("That snapshot is gone."))
}

/// Says why this machine's snapshots can't change now. qemu-img refuses a disk that is in use too,
/// which covers a VM this copy of the app can't see.
fn snapshot_problem(context: &Context, machine: &Machine) -> Option<Failure> {
    let running = running_vms(context).get(&context.slot.number()).is_some_and(|qemu| qemu.runs(machine));
    if running {
        return Some(Failure::new("Shut the VM down first."));
    }
    machine.installing.then(|| Failure::new("Windows has not finished installing here."))
}

fn change_snapshots(context: &Context, machine: &Machine, arguments: &[&str]) -> Result<()> {
    if let Some(problem) = snapshot_problem(context, machine) {
        return Err(problem);
    }
    let result = Command::new("qemu-img")
        .args(["snapshot", "-f", "qcow2"])
        .args(arguments)
        .arg(&machine.disk)
        .output()?;
    if !result.status.success() {
        let said = String::from_utf8_lossy(&result.stderr);
        return Err(Failure(format!("qemu-img refused: {}", said.trim())));
    }
    Ok(())
}

/// Copies a folder and what is in it.
pub fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The firmware file goes first: a snapshot only counts once the disk has it too.
pub fn take_snapshot(task: &mut Task, name: &str) -> Result<()> {
    let (context, machine) = (task.context, task.machine.clone());
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_SNAPSHOT_NAME {
        return Err(Failure(format!("A snapshot needs a name of up to {MAX_SNAPSHOT_NAME} characters.")));
    }
    if list_snapshots(context, &machine).iter().any(|snapshot| snapshot.name == name) {
        return Err(Failure::new("There is already a snapshot with that name."));
    }
    if let Some(problem) = snapshot_problem(context, &machine) {
        return Err(problem);
    }
    task.step(&format!("Taking the snapshot {name}"));
    let tag = format!("{SNAPSHOT_TAG}{}", random_hex::<TAG_RANDOM_BYTES>());
    fs::create_dir_all(snapshots_folder(context, &machine))?;
    fs::copy(&machine.vars, snapshot_file(context, &machine, &tag, ".fd"))?;
    let tpm = machine.tpm_folder(context);
    if tpm.is_dir() {
        copy_tree(&tpm, &snapshot_file(context, &machine, &tag, ".tpm"))?;
    }
    fs::write(snapshot_file(context, &machine, &tag, ".json"), json!({"name": name}).to_string())?;
    if let Err(failure) = change_snapshots(context, &machine, &["-c", &tag]) {
        remove_snapshot_files(context, &machine, &tag)?;
        return Err(failure);
    }
    Ok(())
}

fn remove_snapshot_files(context: &Context, machine: &Machine, tag: &str) -> Result<()> {
    for suffix in [".fd", ".json"] {
        fs::remove_file(snapshot_file(context, machine, tag, suffix))?;
    }
    let _ = fs::remove_dir_all(snapshot_file(context, machine, tag, ".tpm"));
    Ok(())
}

/// Everything that can fail comes before the disk changes, and the firmware file then goes across in
/// one step. The TPM goes with them: Windows may have sealed its disk's key in it.
pub fn go_back(task: &mut Task, tag: &str) -> Result<()> {
    let (context, machine) = (task.context, task.machine.clone());
    let snapshot = find_snapshot(context, &machine, tag)?;
    task.step(&format!("Going back to {}", snapshot.name));
    let new = with_suffix(Path::new(&machine.vars), ".new");
    let tpm = machine.tpm_folder(context);
    let new_tpm = with_suffix(&tpm, ".new");
    let saved_tpm = snapshot_file(context, &machine, tag, ".tpm");
    fs::copy(snapshot_file(context, &machine, tag, ".fd"), &new)?;
    let _ = fs::remove_dir_all(&new_tpm);
    if saved_tpm.is_dir() {
        copy_tree(&saved_tpm, &new_tpm)?;
    }
    if let Err(failure) = change_snapshots(context, &machine, &["-a", tag]) {
        fs::remove_file(&new)?;
        let _ = fs::remove_dir_all(&new_tpm);
        return Err(failure);
    }
    fs::rename(&new, &machine.vars)?;
    if new_tpm.is_dir() {
        let _ = fs::remove_dir_all(&tpm);
        fs::rename(new_tpm, tpm)?;
    }
    Ok(())
}

pub fn delete_snapshot(task: &mut Task, tag: &str) -> Result<()> {
    let (context, machine) = (task.context, task.machine.clone());
    let snapshot = find_snapshot(context, &machine, tag)?;
    task.step(&format!("Deleting the snapshot {}", snapshot.name));
    change_snapshots(context, &machine, &["-d", tag])?;
    remove_snapshot_files(context, &machine, tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Task, grow};
    use crate::disk_image::disk_size_gib;
    use crate::library::{NewMachine, create, delete, remove_entry};
    use crate::machine::Kind;
    use crate::processes::stand_in::fake_process;
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};
    use crate::vm_host::stand_in::RecordingHost;
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    fn fill(disk: &str, byte: u8) {
        let write = format!("write -P {byte} 0 64k");
        let done = Command::new("qemu-io").args(["-f", "qcow2", "-c", &write, disk]).output().unwrap();
        assert!(done.status.success());
    }

    fn holds(disk: &str, byte: u8) -> bool {
        let read = format!("read -P {byte} 0 64k");
        Command::new("qemu-io")
            .args(["-f", "qcow2", "-r", "-c", &read, disk])
            .output()
            .unwrap()
            .status
            .success()
    }

    fn make(context: &Context, scratch: &Scratch) -> Machine {
        let iso = scratch.write("linux.iso", "disc").display().to_string();
        let new = NewMachine {
            name: "Snaps".into(),
            iso,
            kind: Kind::Plain,
            cores: 1,
            ram_gib: 1,
            disk_gib: 1,
            user: String::new(),
            password: String::new(),
            amd_gpu: None,
            gpu_maker: None,
        };
        create(context, &new).unwrap()
    }

    fn firmware(machine: &Machine) -> String {
        fs::read_to_string(&machine.vars).unwrap()
    }

    #[test]
    fn going_back_is_complete() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = make(&context, &scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "take_snapshot", machine.clone());
        assert!(list_snapshots(&context, &machine).is_empty());
        fill(&machine.disk, 0x11);
        fs::write(&machine.vars, "first").unwrap();
        take_snapshot(&mut task, " 1 ").unwrap();
        fill(&machine.disk, 0x22);
        fs::write(&machine.vars, "second").unwrap();
        take_snapshot(&mut task, "Ünï, cødé; $(rm) -a").unwrap();
        fill(&machine.disk, 0x33);
        fs::write(&machine.vars, "third").unwrap();
        let found = list_snapshots(&context, &machine);
        let [first, second] = &found[..] else { panic!("{found:?}") };
        assert_eq!((first.name.as_str(), second.name.as_str()), ("1", "Ünï, cødé; $(rm) -a"));
        assert!(holds(&machine.disk, 0x33) && !holds(&machine.disk, 0x11), "the check itself must work");
        assert!(first.tag.starts_with(SNAPSHOT_TAG) && first.tag != second.tag);
        for (name, text) in [("1", "already"), (" ", "needs a name"), (&"x".repeat(61), "needs a name")] {
            assert!(take_snapshot(&mut task, name).unwrap_err().0.contains(text));
        }
        assert_eq!(list_snapshots(&context, &machine).len(), 2, "a refused snapshot leaves nothing");
        go_back(&mut task, &first.tag).unwrap();
        assert!(holds(&machine.disk, 0x11) && firmware(&machine) == "first", "going back must be complete");
        assert!(!with_suffix(Path::new(&machine.vars), ".new").exists());
        assert_eq!(list_snapshots(&context, &machine).len(), 2, "other snapshots stay");
        go_back(&mut task, &second.tag).unwrap();
        assert!(holds(&machine.disk, 0x22) && firmware(&machine) == "second");
        grow(&mut task, 2).unwrap();
        assert_eq!(disk_size_gib(&machine.disk).unwrap(), 2.0);
        go_back(&mut task, &first.tag).unwrap();
        assert_eq!(disk_size_gib(&machine.disk).unwrap(), 1.0, "the disk's size goes back too");
        assert!(holds(&machine.disk, 0x11));
        delete_snapshot(&mut task, &second.tag).unwrap();
        let left: Vec<String> =
            list_snapshots(&context, &machine).into_iter().map(|found| found.tag).collect();
        assert_eq!(left, std::slice::from_ref(&first.tag));
        let names = crate::machine::sorted_names(&snapshots_folder(&context, &machine));
        assert!(!names.iter().any(|name| name.contains(&second.tag)));
        assert!(go_back(&mut task, &second.tag).unwrap_err().0.contains("gone"));
        assert!(delete_snapshot(&mut task, &second.tag).unwrap_err().0.contains("gone"));
        assert!(Command::new("qemu-img").args(["check", "-q", &machine.disk]).status().unwrap().success());
    }

    #[test]
    fn the_tpm_goes_back_with_the_disk() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = make(&context, &scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "take_snapshot", machine.clone());
        let state = machine.tpm_folder(&context).join("state");
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        fs::write(&state, "saved").unwrap();
        take_snapshot(&mut task, "with a TPM").unwrap();
        let tag = list_snapshots(&context, &machine)[0].tag.clone();
        fs::write(&state, "later").unwrap();
        go_back(&mut task, &tag).unwrap();
        assert_eq!(fs::read_to_string(&state).unwrap(), "saved");
        delete_snapshot(&mut task, &tag).unwrap();
        let names = crate::machine::sorted_names(&snapshots_folder(&context, &machine));
        assert!(names.is_empty());
    }

    #[test]
    fn a_disk_something_else_has_open_is_refused_and_nothing_changes() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = make(&context, &scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "take_snapshot", machine.clone());
        take_snapshot(&mut task, "before").unwrap();
        let tag = list_snapshots(&context, &machine)[0].tag.clone();
        fs::write(&machine.vars, "now").unwrap();
        fill(&machine.disk, 0x44);
        let mut holder = Command::new("qemu-io")
            .args(["-f", "qcow2", &machine.disk])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = holder.stdin.take().unwrap();
        writeln!(input, "read 0 512").unwrap();
        let mut line = String::new();
        BufReader::new(holder.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert!(take_snapshot(&mut task, "open").unwrap_err().0.contains("refused"));
        assert!(go_back(&mut task, &tag).unwrap_err().0.contains("refused"));
        assert!(delete_snapshot(&mut task, &tag).unwrap_err().0.contains("refused"));
        drop(input);
        holder.wait().unwrap();
        assert!(
            firmware(&machine) == "now" && holds(&machine.disk, 0x44),
            "a refused go back changes nothing"
        );
        assert!(!with_suffix(Path::new(&machine.vars), ".new").exists());
        assert_eq!(crate::machine::sorted_names(&snapshots_folder(&context, &machine)).len(), 2);
    }

    #[test]
    fn what_a_crash_leaves_and_what_was_made_by_hand_is_not_listed() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = make(&context, &scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "take_snapshot", machine.clone());
        take_snapshot(&mut task, "real").unwrap();
        let tag = list_snapshots(&context, &machine)[0].tag.clone();
        assert!(
            Command::new("qemu-img")
                .args(["snapshot", "-c", "by-hand", &machine.disk])
                .status()
                .unwrap()
                .success()
        );
        fs::write(snapshot_file(&context, &machine, "guesthouse-half", ".json"), r#"{"name": "half"}"#)
            .unwrap();
        let tags: Vec<String> =
            list_snapshots(&context, &machine).into_iter().map(|found| found.tag).collect();
        assert_eq!(tags, [tag]);
    }

    #[test]
    fn a_running_or_installing_machine_keeps_its_snapshots_as_they_are() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = make(&context, &scratch);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "take_snapshot", machine.clone());
        take_snapshot(&mut task, "kept").unwrap();
        let tag = list_snapshots(&context, &machine)[0].tag.clone();
        let qemu = context.paths.qemu.display().to_string();
        let socket =
            format!("unix:{}/qmp.sock,server=on", context.paths.run_folder(slots::first()).display());
        let drive = format!("file={},format=qcow2", machine.disk);
        fake_process(&context, 61, &[&qemu, "-qmp", &socket, "-drive", &drive], &[], "");
        assert!(go_back(&mut task, &tag).unwrap_err().0.contains("Shut the VM down"));
        assert!(take_snapshot(&mut task, "on").unwrap_err().0.contains("Shut the VM down"));
        fs::remove_dir_all(context.processes.join("61")).unwrap();
        task.machine.installing = true;
        assert!(take_snapshot(&mut task, "early").unwrap_err().0.contains("finished installing"));
        task.machine.installing = false;
        delete(&context, &machine).unwrap();
        assert!(!machine.folder(&context).exists(), "a machine's snapshots go with it");
    }

    #[test]
    fn an_entrys_disk_stays_so_its_snapshots_must_go_first() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let outside = scratch.path("outside.qcow2").display().to_string();
        assert!(
            Command::new("qemu-img")
                .args(["create", "-q", "-f", "qcow2", &outside, "1G"])
                .status()
                .unwrap()
                .success()
        );
        let firmware = scratch.write("outside.fd", "firmware").display().to_string();
        let entry = Machine { kind: Kind::Plain, ..Machine::new("entry", "Entry", &outside, &firmware) };
        entry.save(&context).unwrap();
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "take_snapshot", entry.clone());
        take_snapshot(&mut task, "kept").unwrap();
        assert!(remove_entry(&context, &entry).unwrap_err().0.contains("snapshots first"));
        let tag = list_snapshots(&context, &entry)[0].tag.clone();
        delete_snapshot(&mut task, &tag).unwrap();
        remove_entry(&context, &entry).unwrap();
        assert!(!entry.folder(&context).exists());
    }
}
