//! The Windows host's snapshot index as it is today, held before it moves: `snapshots/index.json`
//! byte for byte after each kind of change, what is read from older or odd indexes, and the rules'
//! numbers. The kept copies are in `tests/snapshot-golden/`;
//! `GUESTHOUSE_WRITE_GOLDEN=1 cargo test -p guesthouse-host-windows --test snapshot_index` writes them
//! afresh, which is only for a change meant to alter the index.

#![allow(clippy::disallowed_methods)] // a test's own folders

use guesthouse_host_windows::snapshots::{
    self, Disks, LONGEST_LABEL, MOST, Snapshot, chain, links, list, made_on_another, repair,
};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

const WRITE: &str = "GUESTHOUSE_WRITE_GOLDEN";

fn golden(name: &str, made: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshot-golden").join(name);
    if std::env::var_os(WRITE).is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, made).unwrap();
        return;
    }
    let kept = fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert!(kept == made.as_bytes(), "{name} is no longer what the snapshot code writes:\n{made}");
}

/// Disks as small files that name the disk they're made on. Each index the take sees as it makes a
/// disk is kept, as that is the index a crash would leave.
#[derive(Default)]
struct Files {
    seen: RefCell<Vec<String>>,
}

impl Disks for Files {
    fn differencing(&self, child: &Path, parent: &Path) -> Result<(), String> {
        let index = parent.parent().unwrap().join("index.json");
        let other = child.parent().unwrap().join("snapshots").join("index.json");
        for path in [index, other] {
            if let Ok(text) = fs::read_to_string(path) {
                self.seen.borrow_mut().push(text);
                break;
            }
        }
        fs::write(child, format!("on {}", parent.file_name().unwrap().to_string_lossy()))
            .map_err(|error| error.to_string())
    }

    fn merge_into_parent(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }

    fn set_parent(&self, child: &Path, parent: &Path) -> Result<(), String> {
        fs::write(child, format!("on {}", parent.file_name().unwrap().to_string_lossy()))
            .map_err(|error| error.to_string())
    }

    fn parent_of(&self, child: &Path) -> Result<PathBuf, String> {
        let text = fs::read_to_string(child).map_err(|error| error.to_string())?;
        let name = text.strip_prefix("on ").ok_or("it has no parent")?;
        Ok(child.parent().unwrap().join(name))
    }
}

struct Machine {
    root: PathBuf,
    disk: PathBuf,
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn machine(test: &str) -> Machine {
    let root = std::env::temp_dir().join(format!("gh-snapshot-index-{test}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let disk = root.join("win.vhdx");
    fs::write(&disk, "installed").unwrap();
    Machine { root, disk }
}

fn index_path(disk: &Path) -> PathBuf {
    disk.parent().unwrap().join("snapshots").join("index.json")
}

fn index(disk: &Path) -> String {
    fs::read_to_string(index_path(disk)).unwrap()
}

fn shown(id: u64, label: &str, taken: u64, current: bool) -> Snapshot {
    Snapshot { id, label: label.to_owned(), taken, current }
}

#[test]
fn the_rules_kept_today() {
    assert_eq!((MOST, LONGEST_LABEL), (32, 60));
}

#[test]
fn each_change_writes_the_index_in_these_bytes() {
    let machine = machine("changes");
    let disk = &machine.disk;
    let disks = Files::default();
    assert_eq!(snapshots::take(disk, "first", 1_000, &disks), Ok(1));
    golden("01-taken.json", &index(disk));
    assert_eq!(snapshots::take(disk, "Ünï \"quoted\" \\ tab\t", 2_000, &disks), Ok(2));
    golden("02-taking.json", &disks.seen.borrow()[1]);
    golden("03-two.json", &index(disk));
    snapshots::revert(disk, 1, &disks).unwrap();
    golden("04-gone-back.json", &index(disk));
    assert_eq!(snapshots::take(disk, "third", 3_000, &disks), Ok(3));
    snapshots::delete(disk, 1, &disks).unwrap();
    golden("05-hidden.json", &index(disk));
    assert_eq!(
        list(disk).unwrap(),
        [shown(2, "Ünï \"quoted\" \\ tab\t", 2_000, false), shown(3, "third", 3_000, true)]
    );
    let in_folder = |name: &str| disk.parent().unwrap().join("snapshots").join(name);
    assert_eq!(chain(disk).unwrap(), [in_folder("3.vhdx"), in_folder("1.vhdx")]);
    assert_eq!(
        links(disk).unwrap(),
        [
            (in_folder("2.vhdx"), in_folder("1.vhdx")),
            (in_folder("3.vhdx"), in_folder("1.vhdx")),
            (disk.clone(), in_folder("3.vhdx")),
        ]
    );
    assert!(made_on_another(disk).unwrap());
    snapshots::delete(disk, 2, &disks).unwrap();
    golden("06-one-left.json", &index(disk));
    snapshots::delete(disk, 3, &disks).unwrap();
    golden("07-none-left.json", &index(disk));
    assert!(!made_on_another(disk).unwrap());
    assert_eq!(snapshots::take(disk, "again", 4_000, &disks), Ok(4), "an ID is never given twice");
}

#[test]
fn refusals_say_these_words_and_leave_the_index_as_it_was() {
    let machine = machine("refusals");
    let disk = &machine.disk;
    let disks = Files::default();
    snapshots::take(disk, "only", 1_000, &disks).unwrap();
    let before = index(disk);
    assert_eq!(snapshots::revert(disk, 9, &disks), Err("There's no snapshot 9.".to_owned()));
    assert_eq!(snapshots::delete(disk, 0, &disks), Err("There's no snapshot 0.".to_owned()));
    assert_eq!(index(disk), before);
}

#[test]
fn older_and_odd_indexes_read_so() {
    let machine = machine("older");
    let disk = &machine.disk;
    let disks = Files::default();
    assert_eq!(list(disk).unwrap(), [], "no index is no snapshots");
    assert_eq!(chain(disk).unwrap(), Vec::<PathBuf>::new());
    fs::create_dir_all(index_path(disk).parent().unwrap()).unwrap();

    fs::write(index_path(disk), "{}").unwrap();
    assert_eq!((list(disk).unwrap(), made_on_another(disk).unwrap()), (vec![], false));

    // No hidden, no next_id and a field it doesn't know: read, and the field is gone at the next save.
    let older = r#"{"snapshots": [{"id": 1, "label": "old", "taken": 5, "parent": null}], "current": 1, "future": [1]}"#;
    fs::write(index_path(disk), older).unwrap();
    assert_eq!(list(disk).unwrap(), [shown(1, "old", 5, true)]);
    fs::write(disk.parent().unwrap().join("snapshots").join("1.vhdx"), "installed").unwrap();
    assert_eq!(snapshots::take(disk, "next", 6, &disks), Ok(1), "with no next_id the first ID is 1 again");
    golden("08-older-then-taken.json", &index(disk));

    fs::write(index_path(disk), "not json").unwrap();
    let said = list(disk).unwrap_err();
    assert_eq!(said, format!("{}: expected ident at line 1 column 2", index_path(disk).display()));

    fs::write(index_path(disk), r#"{"current": 4}"#).unwrap();
    assert_eq!(chain(disk), Err("The snapshot index names a snapshot 4 it doesn't have.".to_owned()));
    let circle = r#"{"snapshots": [{"id": 1, "label": "a", "taken": 0, "parent": 2}, {"id": 2, "label": "b", "taken": 0, "parent": 1}], "current": 1}"#;
    fs::write(index_path(disk), circle).unwrap();
    assert_eq!(chain(disk), Err("The snapshot index goes round in a circle.".to_owned()));
}

#[test]
fn a_take_a_crash_left_is_finished_or_undone_in_these_bytes() {
    let machine = machine("repair");
    let disk = &machine.disk;
    let folder = disk.parent().unwrap().join("snapshots");
    fs::create_dir_all(&folder).unwrap();
    let taking = r#"{"snapshots": [], "current": null, "next_id": 2, "taking": {"id": 1, "label": "half", "taken": 7, "parent": null}}"#;

    // Both disks made, only the index not written: it was taken.
    fs::write(index_path(disk), taking).unwrap();
    fs::write(folder.join("1.vhdx"), "installed").unwrap();
    repair(disk).unwrap();
    golden("09-repaired-taken.json", &index(disk));

    // Moved, and no new disk made on it: it goes back.
    fs::write(index_path(disk), taking).unwrap();
    fs::remove_file(disk).unwrap();
    repair(disk).unwrap();
    golden("10-repaired-undone.json", &index(disk));
    assert_eq!(fs::read_to_string(disk).unwrap(), "installed");

    // Nothing to put right: the index isn't written again.
    let before = index(disk);
    repair(disk).unwrap();
    assert_eq!(index(disk), before);
}
