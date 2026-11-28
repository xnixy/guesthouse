//! A stopped machine's snapshots, as a chain of disks. Taking one moves the machine's disk aside, to
//! stay as it is from then on, and gives the machine a new disk made on it, which holds only what's
//! written after. Going back to one gives the machine a new disk made on that one instead. The
//! firmware's state, with the TPM's, and the record beside the disk go into each snapshot as copies,
//! and come back with it.
//!
//! A snapshot that's deleted while a disk is still made on it is only hidden, and its file goes once
//! nothing is. When the machine's own disk is all that's made on it, the machine's disk is merged
//! into it, and it takes the machine's disk's place.
//!
//! The machine always has a disk. A new one is made under another name and renamed over the old, as
//! a rename replaces in one step. Taking a snapshot can't do that, as the disk has to move before
//! one can be made on it, so what it's doing is written in the index first, and `repair` finishes or
//! undoes it after a crash. The index is written last, through a file of its own renamed over it.
//!
//! The disk work is behind [`Disks`], so all of this is tested without Windows.

#![allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk

use crate::record::Record;
use guesthouse_snapshot_core::windows_host::{
    Entry, INDEX, Index, NEW_INDEX, chain_in, folder, index_text, links_in, read_index, shown, snapshot_disk,
};
pub use guesthouse_snapshot_core::windows_host::{
    LONGEST_LABEL, MOST, Snapshot, checked_label, count_refusal,
};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// What's asked of virtual disks.
pub trait Disks {
    /// Makes `child`, which must not be there yet, holding only what's written after and reading the
    /// rest from `parent`.
    fn differencing(&self, child: &Path, parent: &Path) -> Result<(), String>;
    /// Writes what `child` holds into its parent.
    fn merge_into_parent(&self, child: &Path) -> Result<(), String>;
    /// Makes `child`, a differencing disk, read the rest from `parent` in place of the disk it was
    /// made on. A disk stores where its parent is as a path, so one that's been copied still reads
    /// from the original until it's told.
    fn set_parent(&self, child: &Path, parent: &Path) -> Result<(), String>;
    /// Where `child`, a differencing disk, reads its parent from, as it finds it now. It's an error
    /// if it can't.
    fn parent_of(&self, child: &Path) -> Result<PathBuf, String>;
}

fn state_file(disk: &Path) -> PathBuf {
    disk.with_extension("vmgs")
}

fn load(disk: &Path) -> Result<Index, String> {
    let path = folder(disk).join(INDEX);
    match fs::read_to_string(&path) {
        Ok(text) => read_index(&text).map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Index::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn save(disk: &Path, index: &Index) -> Result<(), String> {
    let folder = folder(disk);
    fs::create_dir_all(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    let path = folder.join(INDEX);
    let new = folder.join(NEW_INDEX);
    fs::write(&new, index_text(index)).map_err(|error| format!("{}: {error}", new.display()))?;
    fs::rename(&new, &path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Copies `from` to `to` if it's there, and takes `to` away if it isn't, so a copy never outlives
/// its file.
fn copy_if_there(from: &Path, to: &Path) -> Result<(), String> {
    match fs::copy(from, to) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let _ = fs::remove_file(to);
            Ok(())
        }
        Err(error) => Err(format!("Copying {} to {}: {error}", from.display(), to.display())),
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to).map_err(|error| format!("Moving {} to {}: {error}", from.display(), to.display()))
}

/// The machine's snapshots, oldest first, without those deleted.
pub fn list(disk: &Path) -> Result<Vec<Snapshot>, String> {
    Ok(shown(&load(disk)?))
}

/// The disks the machine's disk is made on, nearest first, which a machine must be able to open as
/// it does its own.
pub fn chain(disk: &Path) -> Result<Vec<PathBuf>, String> {
    chain_in(&load(disk)?, disk)
}

/// Every disk of the machine that's made on another, each with the one it's made on: the machine's
/// own disk, and each snapshot's that has a parent. Deleted snapshots count, as their disks are
/// there while another is made on them, and those of another line of snapshots do too, which the
/// machine's chain doesn't pass through.
pub fn links(disk: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    Ok(links_in(&load(disk)?, disk))
}

/// Puts right what a machine's folder holds that says where the folder is, after the whole folder
/// has been copied from `old` to `new`, which are the machine's disks. The installer a record names
/// is the copy in the folder, and each differencing disk is told its parent's new place, then asked
/// where it reads its parent from, as one still reading the old copy would read as the new until the
/// old one is deleted. Nothing else may be using either.
pub fn relocate(old: &Path, new: &Path, disks: &dyn Disks) -> Result<(), String> {
    let old_folder = old.parent().unwrap_or(Path::new("."));
    let new_folder = new.parent().unwrap_or(Path::new("."));
    let index = load(new)?;
    let mut disk_files = vec![new.to_owned()];
    disk_files.extend(index.snapshots.iter().map(|entry| snapshot_disk(new, entry.id)));
    for file in disk_files.iter().filter(|file| Record::path(file).exists()) {
        let mut record = Record::load(file)?;
        if let Some(installer) = &record.installer
            && let Ok(inside) = installer.strip_prefix(old_folder)
        {
            record.installer = Some(new_folder.join(inside));
            record.save(file)?;
        }
    }
    let links = links(new)?;
    for (child, parent) in &links {
        disks.set_parent(child, parent)?;
    }
    for (child, parent) in &links {
        let reads = disks.parent_of(child)?;
        if !same_path(&reads, parent) {
            return Err(format!(
                "{} reads from {}, not {} where it was copied.",
                child.display(),
                reads.display(),
                parent.display()
            ));
        }
    }
    Ok(())
}

/// Whether two paths are the same file as Windows names them: the same whatever their case, and with
/// or without the `\\?\` Windows puts before a path it has resolved.
fn same_path(a: &Path, b: &Path) -> bool {
    let plain = |path: &Path| {
        let text = path.to_string_lossy().to_lowercase().replace('/', "\\");
        text.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(text)
    };
    plain(a) == plain(b)
}

/// Whether the machine's disk is made on another, deleted snapshots' included. Such a disk can't grow.
pub fn made_on_another(disk: &Path) -> Result<bool, String> {
    Ok(load(disk)?.current.is_some())
}

/// Finishes or undoes what a crash left half done. Nothing else may be using the machine's disks.
pub fn repair(disk: &Path) -> Result<(), String> {
    let mut index = load(disk)?;
    let mut changed = false;
    if let Some(entry) = index.taking.take() {
        changed = true;
        let moved = snapshot_disk(disk, entry.id);
        if !disk.exists() && moved.exists() {
            // Moved, and no new disk made on it: it goes back.
            rename(&moved, disk)?;
        } else if disk.exists() && moved.exists() {
            // Both made, and only the index not written: the snapshot was taken.
            index.current = Some(entry.id);
            index.snapshots.push(entry);
        }
        // Not taken, so only its copies may be left.
        if !moved.exists() {
            let _ = fs::remove_file(state_file(&moved));
            let _ = fs::remove_file(Record::path(&moved));
        }
    }
    // A collapse that renamed a hidden snapshot's disk over the machine's, with the index not yet
    // written, leaves the machine's disk made on that snapshot's parent.
    while let Some(id) = index.current {
        let Some(at) = index.snapshots.iter().position(|entry| entry.id == id) else { break };
        if snapshot_disk(disk, id).exists() || !index.snapshots[at].hidden {
            break;
        }
        index.current = index.snapshots.remove(at).parent;
        changed = true;
    }
    if changed { save(disk, &index) } else { Ok(()) }
}

/// Takes a snapshot of the stopped machine's disk, labelled `label`, at `now`. Returns its ID.
pub fn take(disk: &Path, label: &str, now: u64, disks: &dyn Disks) -> Result<u64, String> {
    let mut index = load(disk)?;
    let id = index.next_id.max(1);
    let entry = Entry { id, label: label.to_owned(), taken: now, parent: index.current, hidden: false };
    index.next_id = id + 1;
    index.taking = Some(entry.clone());
    save(disk, &index)?;

    let moved = snapshot_disk(disk, id);
    let made = (|| {
        copy_if_there(&state_file(disk), &state_file(&moved))?;
        copy_if_there(&Record::path(disk), &Record::path(&moved))?;
        rename(disk, &moved)?;
        if let Err(error) = disks.differencing(disk, &moved) {
            // The disk goes back where it was, as it is still.
            let _ = fs::remove_file(disk);
            rename(&moved, disk)?;
            return Err(error);
        }
        Ok(())
    })();
    index.taking = None;
    match made {
        Ok(()) => {
            index.snapshots.push(entry);
            index.current = Some(id);
            save(disk, &index)?;
            Ok(id)
        }
        Err(error) => {
            let _ = fs::remove_file(state_file(&moved));
            let _ = fs::remove_file(Record::path(&moved));
            save(disk, &index)?;
            Err(error)
        }
    }
}

/// Gives the stopped machine a new disk made on snapshot `id`, with that snapshot's firmware state
/// and record. What the machine's disk held since is gone.
pub fn revert(disk: &Path, id: u64, disks: &dyn Disks) -> Result<(), String> {
    let mut index = load(disk)?;
    if !index.snapshots.iter().any(|entry| entry.id == id && !entry.hidden) {
        return Err(format!("There's no snapshot {id}."));
    }
    let snapshot = snapshot_disk(disk, id);
    let new = disk.with_extension("new.vhdx");
    let _ = fs::remove_file(&new);
    disks.differencing(&new, &snapshot)?;
    // The new disk's state and record are put beside it under its own name, then each is renamed over
    // the machine's.
    let placed = (|| {
        copy_if_there(&state_file(&snapshot), &state_file(&new))?;
        copy_if_there(&Record::path(&snapshot), &Record::path(&new))?;
        rename(&new, disk)?;
        if state_file(&new).exists() {
            rename(&state_file(&new), &state_file(disk))?;
        }
        if Record::path(&new).exists() {
            rename(&Record::path(&new), &Record::path(disk))?;
        }
        Ok(())
    })();
    if let Err(error) = placed {
        let _ = fs::remove_file(&new);
        let _ = fs::remove_file(state_file(&new));
        let _ = fs::remove_file(Record::path(&new));
        return Err(error);
    }
    index.current = Some(id);
    save(disk, &index)?;
    collect(disk, &mut index, disks)
}

/// Deletes snapshot `id`. Its file goes now, or once nothing is made on it.
pub fn delete(disk: &Path, id: u64, disks: &dyn Disks) -> Result<(), String> {
    let mut index = load(disk)?;
    let Some(entry) = index.snapshots.iter_mut().find(|entry| entry.id == id && !entry.hidden) else {
        return Err(format!("There's no snapshot {id}."));
    };
    entry.hidden = true;
    save(disk, &index)?;
    collect(disk, &mut index, disks)
}

/// Takes away each deleted snapshot nothing is made on, and merges the machine's disk into a deleted
/// one only it is made on. Each can free another, so it goes on until neither does.
fn collect(disk: &Path, index: &mut Index, disks: &dyn Disks) -> Result<(), String> {
    loop {
        let mut changed = false;
        let hidden: Vec<Entry> = index.snapshots.iter().filter(|entry| entry.hidden).cloned().collect();
        for entry in hidden {
            let snapshots_on = index.snapshots.iter().any(|other| other.parent == Some(entry.id));
            let machine_on = index.current == Some(entry.id);
            let file = snapshot_disk(disk, entry.id);
            if snapshots_on {
                continue;
            }
            if machine_on {
                // The machine's disk into the snapshot's, which then takes its place. Nothing else is
                // made on the snapshot's, so the merge changes no other disk.
                disks.merge_into_parent(disk)?;
                rename(&file, disk)?;
                index.current = entry.parent;
            } else {
                fs::remove_file(&file).or_else(|error| {
                    if error.kind() == ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(format!("{}: {error}", file.display()))
                    }
                })?;
            }
            let _ = fs::remove_file(state_file(&file));
            let _ = fs::remove_file(Record::path(&file));
            index.snapshots.retain(|other| other.id != entry.id);
            save(disk, index)?;
            changed = true;
        }
        if !changed {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests;
