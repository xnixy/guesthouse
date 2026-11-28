//! What the macOS part's download does today, held before its plain half moves: its files' names at
//! every stage, its steps and sentences, curl's words, and what is trusted from one run to the next.
//! Nothing here reaches a network: archives come from `file://` addresses or from the stand-in curl
//! of the downloads' pins.

use super::*;
use crate::downloads::pins::{
    asked, go, go_first, names_in, ready, run_with_path, stand_in_curl, stand_in_here,
};
use crate::testing::{Scratch, scratch_paths};
use std::cell::RefCell;

fn packed(scratch: &Scratch, files: &[&str]) -> (String, String) {
    let top = scratch.path("made").join("guesthouse-macos-part-2");
    for name in files {
        let file = top.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, name).unwrap();
    }
    let archive = scratch.path("part.tar.xz");
    let made = Command::new("tar")
        .args(["--create", "--xz", "--file"])
        .arg(&archive)
        .arg("--directory")
        .arg(scratch.path("made"))
        .arg("guesthouse-macos-part-2")
        .status()
        .unwrap();
    assert!(made.success());
    (format!("file://{}", archive.display()), sha256(&archive).unwrap())
}

const WHOLE: [&str; 4] = ["reims-util", "qemu/bin/qemu-system-x86_64", "ReimsGraphics.rom", "kexts/set.toml"];

#[test]
fn the_size_said_before_it_is_in_mib_rounded_up() {
    assert_eq!(download_size(), "22 MiB");
}

/// The archive comes as `guesthouse-macos-part-2.part` beside the part's folder and is unpacked into
/// `guesthouse-macos-part-2.new`, which is emptied first and becomes the folder; neither is left.
#[test]
fn the_archive_and_the_unpacked_folder_have_names_beside_the_part() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let (url, hash) = packed(&scratch, &WHOLE);
    let into = paths.parts.join("guesthouse-macos-part-2");
    // A folder an earlier try left half unpacked.
    fs::create_dir_all(paths.parts.join("guesthouse-macos-part-2.new")).unwrap();
    fs::write(paths.parts.join("guesthouse-macos-part-2.new/left-over"), "").unwrap();
    let said = RefCell::new(Vec::new());
    fetch(&url, &hash, &into, &|step| said.borrow_mut().push(step.to_owned())).unwrap();
    assert_eq!(names_in(&paths.parts), ["guesthouse-macos-part-2"]);
    assert!(!into.join("left-over").exists());
    let said = said.into_inner();
    assert_eq!(said.first().map(String::as_str), Some("Downloading macOS support"));
    assert_eq!(said.last().map(String::as_str), Some("Unpacking macOS support"));
}

/// A half archive left by a stopped try is gone on from.
#[test]
fn a_half_archive_is_gone_on_from() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let (url, hash) = packed(&scratch, &WHOLE);
    let whole = fs::read(scratch.path("part.tar.xz")).unwrap();
    fs::create_dir_all(&paths.parts).unwrap();
    fs::write(paths.parts.join("guesthouse-macos-part-2.part"), &whole[..whole.len() / 2]).unwrap();
    fetch(&url, &hash, &paths.parts.join("guesthouse-macos-part-2"), &|_| {}).unwrap();
    assert!(is_here(&paths));
}

/// A wrong archive is thrown away with its sentence; a stopped one is kept with curl's words.
#[test]
fn a_wrong_archive_goes_and_a_stopped_one_stays() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let (url, _) = packed(&scratch, &WHOLE);
    let into = paths.parts.join("guesthouse-macos-part-2");
    let error = fetch(&url, &"0".repeat(64), &into, &|_| {}).unwrap_err();
    assert_eq!(error.0, "The macOS support that came is not the file it should be. Try again.");
    assert_eq!(names_in(&paths.parts), Vec::<String>::new());
    fs::write(paths.parts.join("guesthouse-macos-part-2.part"), "half").unwrap();
    let nowhere = format!("file://{}", scratch.path("nowhere").display());
    let error = fetch(&nowhere, &"0".repeat(64), &into, &|_| {}).unwrap_err();
    assert!(error.0.starts_with("macOS support could not be downloaded: curl: (37) "), "{error}");
    assert_eq!(fs::read_to_string(paths.parts.join("guesthouse-macos-part-2.part")).unwrap(), "half");
}

/// An archive whose sum is right but which isn't whole is refused with tar's words; the archive is
/// kept, and the half-unpacked folder goes.
#[test]
fn an_archive_that_is_not_whole_is_kept_and_its_folder_goes() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let (url, hash) = packed(&scratch, &WHOLE[..3]);
    let into = paths.parts.join("guesthouse-macos-part-2");
    let error = fetch(&url, &hash, &into, &|_| {}).unwrap_err();
    assert_eq!(error.0, "macOS support could not be unpacked. ");
    assert_eq!(names_in(&paths.parts), ["guesthouse-macos-part-2.part"]);
}

/// A part's folder with its four files is trusted as it is, with nothing read or fetched; one in the
/// program folder comes first.
#[test]
fn a_whole_folder_is_trusted_by_its_files_being_there() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let into = paths.parts.join("guesthouse-macos-part-2");
    for name in WHOLE {
        let file = into.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "not what the release has").unwrap();
    }
    assert!(is_here(&paths));
    ensure(&paths, &|_| panic!("nothing is fetched")).unwrap();
    assert_eq!(folder(&paths), into);
    fs::create_dir_all(paths.helper("macos")).unwrap();
    assert_eq!(folder(&paths), paths.helper("macos"));
    assert!(!is_here(&paths), "the program folder's own, with nothing in it, is the one asked");
}

#[test]
fn with_a_stand_in_curl() {
    let scratch = Scratch::new();
    let bin = stand_in_curl(&scratch);
    run_with_path("macos_part::pins::stand_in_curl_pins", &bin, true);
}

/// Its steps as it comes, against the release's size, and curl's words.
#[test]
#[ignore = "run by with_a_stand_in_curl, with the stand-in curl first on PATH"]
fn stand_in_curl_pins() {
    let Some(bin) = stand_in_here() else { return };
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let half = vec![0u8; 22_772_560 / 2];
    ready(&bin, "", &half, &half);
    let into = paths.parts.join("guesthouse-macos-part-2");
    let said = RefCell::new(Vec::<String>::new());
    let during = RefCell::new(Vec::<String>::new());
    let error = fetch("https://example.invalid/part.tar.xz", &"0".repeat(64), &into, &|step| {
        said.borrow_mut().push(step.to_owned());
        if step == "Downloading macOS support: 0%" {
            go_first(&bin);
        }
        if step == "Downloading macOS support: 50%" {
            *during.borrow_mut() = names_in(&paths.parts);
            go(&bin);
        }
    })
    .unwrap_err();
    assert_eq!(error.0, "The macOS support that came is not the file it should be. Try again.");
    let mut said = said.into_inner();
    said.retain(|step| step != "Downloading macOS support: 100%");
    assert_eq!(
        said,
        ["Downloading macOS support", "Downloading macOS support: 0%", "Downloading macOS support: 50%"]
    );
    assert_eq!(during.into_inner(), ["guesthouse-macos-part-2.part"]);
    let partial = paths.parts.join("guesthouse-macos-part-2.part");
    assert_eq!(
        asked(&bin),
        [format!(
            "--fail --location --silent --show-error --proto-redir =https --retry 3 --continue-at - --output {} \
             https://example.invalid/part.tar.xz",
            partial.display()
        )]
    );
}
