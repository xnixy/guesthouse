// SPDX-License-Identifier: GPL-2.0-or-later
//! The frame export against several readers at once, with the device brought up by a QEMU and a
//! guest's driver played by hand (`played/`): no virtual machine runs. The readers are rmframes'
//! own reading side (`reader/`), the file the Guesthouse app and rmhost read through; one runs in
//! a second process, so it can be killed.
#![cfg(target_os = "macos")]
#![allow(clippy::disallowed_methods)] // a test's own sockets

mod played;
mod reader;

use played::{COLOUR, Guest, HEIGHT, PATIENCE, WIDTH, ask};
use reader::{COLOUR_BITS, Reader};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// As many slots as the program makes: all a SLOTS message carries.
const SLOTS: usize = 8;
/// As many readers as rmframes serves.
const READERS: u64 = 4;
/// How many swaps readers that hold every slot are given before a frame must have been skipped.
const MOST_SWAPS: usize = 40;
/// Pictures shown while two readers keep up.
const FRAMES: usize = 30;
/// As `src/export.rs` has it: how often the program tries again to hand out a picture that
/// found no free slot.
const LOOK_EVERY: Duration = Duration::from_millis(20);
/// How long readers' counts must stand still to count as settled.
const SETTLED: Duration = Duration::from_millis(150);
/// The reader in a second process: the test it runs, and the variable that names the publisher.
const CHILD_TEST: &str = "a_reader_in_another_process";
const CHILD_NAME: &str = "PVG_EXPORT_CHILD";
const CHILD_READY: &str = "reader ready";
/// How long the soak publishes for outside readers, in seconds, and under which name.
const SOAK_SECONDS: &str = "PVG_EXPORT_SOAK";
const SOAK_NAME: &str = "PVG_EXPORT_NAME";
const SOAK_HERTZ: u64 = 60;

/// A colour of its own for each frame of a run, so a frame shows which swap it was.
fn colour(index: usize) -> u32 {
    0xFF00_0000 | (index as u32 + 1).wrapping_mul(0x0011_0907) & COLOUR_BITS
}

fn number(status: &str, word: &str) -> u64 {
    let mut words = status.split(' ');
    words.find(|found| *found == word).and_then(|_| words.next()).and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Waits until a count of the status line is `value`. The program's counts follow rmframes'
/// own a look later.
fn wait_for_count(control: &Path, word: &str, value: u64) -> String {
    let since = Instant::now();
    loop {
        let status = ask(control, "status");
        if number(&status, word) == value {
            return status;
        }
        assert!(since.elapsed() < PATIENCE, "{word} never came to {value}: {status}");
        std::thread::sleep(LOOK_EVERY);
    }
}

/// Waits until no reader has had a frame for a while.
fn settle(readers: &[&Reader]) {
    let mut counts: Vec<usize> = readers.iter().map(|reader| reader.count()).collect();
    let mut still = Instant::now();
    let since = Instant::now();
    while still.elapsed() < SETTLED {
        assert!(since.elapsed() < PATIENCE, "the readers never settled");
        std::thread::sleep(LOOK_EVERY);
        let now: Vec<usize> = readers.iter().map(|reader| reader.count()).collect();
        if now != counts {
            counts = now;
            still = Instant::now();
        }
    }
}

/// Swaps new colours until a frame has been skipped once more, and returns the colour of the
/// last one and the next colour's index.
fn swap_until_skipped(guest: &mut Guest, first_colour: usize) -> (u32, usize) {
    let control = guest.control.clone();
    let skipped = number(&ask(&control, "status"), "skipped");
    let mut index = first_colour;
    while number(&ask(&control, "status"), "skipped") == skipped {
        assert!(index - first_colour < MOST_SWAPS, "no frame was skipped with every slot held");
        guest.paint(colour(index));
        guest.swap();
        index += 1;
    }
    (colour(index - 1) & COLOUR_BITS, index)
}

/// Starts this test binary again as a reader in a process of its own, which holds every frame
/// it gets, and waits until it has its slots.
fn reader_in_another_process(name: &str) -> Child {
    let mut child = Command::new(std::env::current_exe().expect("this test's program"))
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD_NAME, name)
        .stdout(Stdio::piped())
        .spawn()
        .expect("a second process");
    let mut lines = BufReader::new(child.stdout.take().expect("its output")).lines();
    assert!(
        lines.any(|line| line.is_ok_and(|line| line.contains(CHILD_READY))),
        "the reader in the second process never came up"
    );
    child
}

#[test]
fn several_readers_share_the_slots_and_each_gets_every_frame() {
    if !played::runnable() {
        return;
    }
    let name = format!("pvg-host-test.{}", std::process::id());
    let mut guest = played::start(&["--frames", &name]);
    let control = guest.control.clone();

    // A reader there before the first picture gets slots of its size and the picture itself.
    let first = Reader::open(&name, true);
    guest.swap();
    let shown = first.frame_after(0, PATIENCE);
    assert_eq!(first.heard(|heard| (heard.size, heard.surfaces_len())), ((WIDTH, HEIGHT), SLOTS));
    assert_eq!(first.colour(shown), COLOUR & COLOUR_BITS, "the guest's picture is in the slot");

    // A second that comes after it gets the same picture at once, and the first is sent nothing.
    let before = first.count();
    let since = Instant::now();
    let second = Reader::open(&name, true);
    let late = second.frame_after(0, PATIENCE);
    let waited = since.elapsed();
    assert_eq!(late.sequence, shown.sequence, "a late reader gets the newest picture");
    assert!(waited < LOOK_EVERY * 5, "a late reader waited {waited:?}");
    settle(&[&first, &second]);
    assert_eq!(first.count(), before, "the first reader was not sent it again");
    eprintln!("a late second reader waited {waited:?}");

    // Two readers that keep up both get every frame.
    let (a, b) = (first.count(), second.count());
    for index in 0..FRAMES {
        guest.paint(colour(index));
        guest.swap();
    }
    settle(&[&first, &second]);
    let (got_a, got_b) = (first.sequences_after(a), second.sequences_after(b));
    assert_eq!(got_a, got_b, "both readers got the same frames");
    assert!(got_a.len() >= FRAMES / 2, "{} frames of {FRAMES} pictures", got_a.len());
    assert_eq!(number(&ask(&control, "status"), "skipped"), 0);
    eprintln!("two readers, {FRAMES} pictures: {} frames each", got_a.len());

    // One that holds all it gets makes frames skip for the other only once it holds every slot.
    second.hold();
    let (a, b) = (first.count(), second.count());
    let (newest, next) = swap_until_skipped(&mut guest, FRAMES);
    settle(&[&first, &second]);
    assert_eq!(second.heard(|heard| heard.held.len()), SLOTS, "the slow reader holds every slot");
    assert!(!second.heard(|heard| heard.handed_twice), "a held slot was handed out again");
    assert_eq!(first.sequences_after(a), second.sequences_after(b), "the other got every frame published");
    eprintln!("a slow reader held {SLOTS} slots after {} frames", second.count() - b);

    // Letting go brings the newest picture to both, though the guest shows nothing new.
    let (a, b) = (first.count(), second.count());
    second.let_go();
    let owed = first.frame_after(a, PATIENCE);
    assert_eq!(first.colour(owed), newest, "the picture owed is the newest");
    assert_eq!(second.frame_after(b, PATIENCE).sequence, owed.sequence);

    // A reader in another process that holds every slot, killed, frees them; the others go on.
    let mut third = reader_in_another_process(&name);
    wait_for_count(&control, "readers", 3);
    let (newest, next) = swap_until_skipped(&mut guest, next);
    settle(&[&first, &second]);
    let a = first.count();
    third.kill().expect("the second process is killed");
    let _ = third.wait();
    wait_for_count(&control, "readers", 2);
    let owed = first.frame_after(a, PATIENCE);
    assert_eq!(first.colour(owed), newest, "the frame skipped while it held everything comes after");
    let skipped = number(&ask(&control, "status"), "skipped");
    let a = first.count();
    for index in next..next + FRAMES {
        guest.paint(colour(index));
        guest.swap();
    }
    settle(&[&first, &second]);
    assert!(first.count() - a >= FRAMES / 2, "the first reader went on");
    assert_eq!(number(&ask(&control, "status"), "skipped"), skipped, "nothing held after the kill");

    // A third and a fourth join and get the picture; a fifth is turned away with a reason.
    let shown_last = first.heard(|heard| *heard.frames.last().expect("a frame"));
    let third = Reader::open(&name, true);
    let fourth = Reader::open(&name, true);
    for reader in [&third, &fourth] {
        assert_eq!(reader.frame_after(0, PATIENCE).sequence, shown_last.sequence);
    }
    wait_for_count(&control, "readers", READERS);
    let fifth = Reader::open(&name, true);
    let reason = fifth.wait("a refusal", PATIENCE, |heard| heard.refused.clone());
    assert!(reason.contains("already has 4 readers"), "{reason}");
    eprintln!("a fifth reader: {reason}");
    fifth.close();
    let status = wait_for_count(&control, "readers", READERS);
    eprintln!("{status}");
    assert_eq!(number(&status, "failed"), 0, "{status}");

    // QEMU goes, the program with it, and every reader hears its publisher has gone.
    drop(guest.qemu);
    let since = Instant::now();
    let status = loop {
        if let Some(status) = guest.program.child.try_wait().expect("the program's end") {
            break status;
        }
        assert!(since.elapsed() < PATIENCE, "the program outlived QEMU");
        std::thread::sleep(Duration::from_millis(10));
    };
    // The export's thread lets the display go before the device does, or the display's
    // unplugging raises an interrupt into a device that has gone.
    assert!(status.success(), "{status}");
    for reader in [&first, &second, &third, &fourth] {
        reader.wait("word that the publisher went", PATIENCE, |heard| heard.gone.then_some(()));
    }
}

/// The reader the test above starts in a process of its own; nothing by itself.
#[test]
#[ignore = "a reader for the test above, in a second process"]
fn a_reader_in_another_process() {
    let Ok(name) = std::env::var(CHILD_NAME) else {
        return;
    };
    let reader = Reader::open(&name, false);
    reader.wait("slots", PATIENCE, |heard| (heard.surfaces_len() > 0).then_some(()));
    println!("{CHILD_READY}");
    // Holds whatever comes until it is killed.
    std::thread::sleep(PATIENCE * 12);
    reader.close();
}

/// Publishes a changing picture for as long as `PVG_EXPORT_SOAK` says, under `PVG_EXPORT_NAME`,
/// for readers outside the test such as `rmhost stats` or `serve`:
///
///     PVG_EXPORT_SOAK=60 PVG_EXPORT_NAME=pvg-host.soak cargo test -p guesthouse-pvg-host \
///         --test export -- --ignored --nocapture a_picture_that_changes
#[test]
#[ignore = "runs for as long as PVG_EXPORT_SOAK says, for readers outside the test"]
fn a_picture_that_changes_is_published_for_outside_readers() {
    if std::env::var(SOAK_SECONDS).is_err() || !played::runnable() {
        return;
    }
    let seconds: u64 = std::env::var(SOAK_SECONDS).ok().and_then(|text| text.parse().ok()).unwrap_or(10);
    let name = std::env::var(SOAK_NAME).unwrap_or_else(|_| format!("pvg-host-test.{}", std::process::id()));
    let mut guest = played::start(&["--frames", &name]);
    eprintln!("publishing as {name} for {seconds} s");
    let since = Instant::now();
    let period = Duration::from_nanos(1_000_000_000 / SOAK_HERTZ);
    let mut index = 0;
    while since.elapsed() < Duration::from_secs(seconds) {
        guest.paint(colour(index));
        guest.swap();
        index += 1;
        let due = period * index as u32;
        if let Some(wait) = due.checked_sub(since.elapsed()) {
            std::thread::sleep(wait);
        }
    }
    eprintln!("{index} pictures shown; {}", ask(&guest.control, "status"));
}

/// Reads a running program's frames under `PVG_EXPORT_READ` for `PVG_EXPORT_READ_SECONDS` (10),
/// giving each back at once, and says each second how many came and of what size: for a real
/// guest's frames, such as a machine whose graphics process runs with `--frames NAME`.
///
///     PVG_EXPORT_READ=guesthouse.test PVG_EXPORT_READ_SECONDS=20 cargo test -p guesthouse-pvg-host \
///         --test export -- --ignored --nocapture a_reader_outside
#[test]
#[ignore = "reads a program outside the test, named by PVG_EXPORT_READ"]
fn a_reader_outside_counts_the_frames_it_gets() {
    let Ok(name) = std::env::var("PVG_EXPORT_READ") else {
        return;
    };
    let seconds: u64 =
        std::env::var("PVG_EXPORT_READ_SECONDS").ok().and_then(|text| text.parse().ok()).unwrap_or(10);
    let reader = Reader::open(&name, true);
    let said = |heard: &reader::Heard| {
        format!(
            "{} frames, slots {} of {}x{}, generation {}, last sequence {}, gone {}, refused {:?}",
            heard.frames.len(),
            heard.surfaces_len(),
            heard.size.0,
            heard.size.1,
            heard.generation,
            heard.frames.last().map_or(0, |frame| frame.sequence),
            heard.gone,
            heard.refused,
        )
    };
    let since = Instant::now();
    for second in 1..=seconds {
        std::thread::sleep((Duration::from_secs(second)).saturating_sub(since.elapsed()));
        println!("reader {second} s: {}", reader.heard(said));
        if reader.heard(|heard| heard.gone || heard.refused.is_some()) {
            break;
        }
    }
    println!("reader end: {}", reader.heard(said));
    reader.close();
}
