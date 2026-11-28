//! Where an Intel machine's desktop is drawn on its next start, and the way back to the software
//! desktop when the card's didn't come up.
//!
//! A person's word in [`DESKTOP_VARIABLE`](guesthouse_backend_intel::stage::DESKTOP_VARIABLE)
//! decides, unless that same word already failed on this build. Without it a machine whose hardware
//! desktop failed on this build keeps the software one, a recorded card keeps the software one, and
//! any other gets the stage's. A new build, or a person asking for the hardware desktop after the
//! stage's own try failed, tries it again.
//!
//! Once its daemon is up, a start goes by what the daemon gave: a hardware desktop asked at a stage
//! where nothing the compositor sends runs is the software desktop, with the daemon's reason in the
//! run folder. See [`guesthouse_daemon::desktop_note`].

use crate::context::Context;
use crate::gpu_maker::GpuMaker;
use crate::machine::Machine;
use crate::updates::VERSION;
use guesthouse_backend_intel::stage::{
    Desktop, LAB_DEVICE_ON_RECORDED_CARD_VARIABLE, LISTENING_NOTE_START, RUN_VARIABLE, Stage,
    lab_keeps_device_on_recorded_card,
};
use guesthouse_daemon::desktop_note::read_note;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const FALLBACK_FILE: &str = "intel-software-desktop";
const HARDWARE_LOG_FILE: &str = "intel-hardware-desktop-daemon.out";
/// How much of a big log's start, warnings and end [`bounded_log`] keeps: at most 12 MiB in all.
const LOG_START_BYTES: u64 = 2 << 20;
const LOG_WARNING_BYTES: u64 = 2 << 20;
const LOG_END_BYTES: u64 = 8 << 20;
/// Kept while the app has the Intel render device switched off in the guest, with why.
const DEVICE_OFF_FILE: &str = "intel-device-off";
/// The second line of the file: whether a person had asked for the hardware desktop that failed.
const ASKED: &str = "asked";
const BY_DEFAULT: &str = "default";

/// Why a machine is back on the software desktop, and the build that put it there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback {
    pub build: String,
    /// A person had asked for the hardware desktop that failed.
    pub asked: bool,
    pub reason: String,
}

/// Where the fallback is kept: the build, then [`ASKED`] or [`BY_DEFAULT`], then why.
pub fn fallback_file(context: &Context, machine: &Machine) -> PathBuf {
    machine.folder(context).join(FALLBACK_FILE)
}

pub fn fallback(context: &Context, machine: &Machine) -> Option<Fallback> {
    let text = fs::read_to_string(fallback_file(context, machine)).ok()?;
    let mut lines = text.lines();
    let build = lines.next()?.trim().to_owned();
    let asked = lines.next().is_some_and(|line| line.trim() == ASKED);
    let reason = lines.collect::<Vec<_>>().join(" ").trim().to_owned();
    Some(Fallback { build, asked, reason })
}

/// The next starts go to the software desktop, for this reason, until there's another build.
pub fn fall_back(context: &Context, machine: &Machine, reason: &str) -> std::io::Result<()> {
    let how = if Desktop::asked_in_environment() == Some(Desktop::Hardware) { ASKED } else { BY_DEFAULT };
    fs::write(fallback_file(context, machine), format!("{VERSION}\n{how}\n{reason}\n"))
}

/// The desktop for this start, with what a person asked for, if they did.
pub fn chosen(context: &Context, machine: &Machine, asked: Option<Desktop>) -> Desktop {
    let default = Desktop::default_for(context.recorded_intel_card.is_some());
    let on_this_build = fallback(context, machine).filter(|fallback| fallback.build == VERSION);
    pick(asked, default, on_this_build.map(|fallback| fallback.asked))
}

/// A person's word, then a fallback on this build, then the default. `fell_back` says whether this
/// build fell back, and if so whether a person had asked for what failed. The same word failing
/// again is not tried again, which also keeps a fallback within one start from going round.
fn pick(asked: Option<Desktop>, default: Desktop, fell_back: Option<bool>) -> Desktop {
    match (asked, fell_back) {
        (Some(Desktop::Hardware), Some(true)) => Desktop::Software,
        (Some(desktop), _) => desktop,
        (None, Some(_)) => Desktop::Software,
        (None, None) => default,
    }
}

/// Where the app notes that it switched the Intel render device off in the guest.
pub fn device_off_file(context: &Context, machine: &Machine) -> PathBuf {
    machine.folder(context).join(DEVICE_OFF_FILE)
}

/// Whether the Intel render device is switched off in the guest on `desktop`. On the software
/// desktop the compositor takes it for what it draws, and refused Intel's driver it keeps asking
/// and draws nothing. With the device off, it draws in software on the basic display. A person who
/// asks for work on a real card keeps the device, for that work, and the screen stays black: by
/// asking for the software desktop, or for compute work in [`RUN_VARIABLE`], which also holds on a
/// machine that fell back from the hardware desktop. Nothing runs, and the device goes off, on a
/// recorded card, at the listening stage, or where the daemon's note (`daemon_note`, see
/// [`Desktop::note`]) says the stage is listening.
pub fn switches_device_off(
    desktop: Desktop,
    asked: Option<Desktop>,
    recorded_card: bool,
    run: Option<&OsStr>,
    daemon_note: Option<&str>,
) -> bool {
    if desktop.has_display_driver() {
        return false;
    }
    let (stage, _) = Stage::asked(run);
    let daemon_listens = daemon_note.is_some_and(|note| note.starts_with(LISTENING_NOTE_START));
    let work_runs = !recorded_card && stage.is_real() && !daemon_listens;
    let work_asked_for = asked == Some(Desktop::Software) || stage == Stage::Compute;
    !(work_runs && work_asked_for)
}

/// Whether the device step counts this start's card as recorded: a recorded card is, unless a lab
/// asked for [`LAB_DEVICE_ON_RECORDED_CARD_VARIABLE`]'s exception (`lab`, its value), for a daemon
/// that runs the lab card.
fn recorded_for_the_device(recorded_card: bool, lab: Option<&OsStr>) -> bool {
    recorded_card && !lab_keeps_device_on_recorded_card(lab, recorded_card)
}

/// Whether a lab has this start's recorded card treated as a real one by the device step.
pub fn lab_device_on_recorded_card(context: &Context) -> bool {
    let recorded = context.recorded_intel_card.is_some();
    recorded
        && !recorded_for_the_device(
            recorded,
            std::env::var_os(LAB_DEVICE_ON_RECORDED_CARD_VARIABLE).as_deref(),
        )
}

/// [`switches_device_off`] for this start of an Intel machine.
pub fn device_off_for_start(context: &Context, machine: &Machine) -> bool {
    machine.windows_maker() == Some(GpuMaker::Intel)
        && switches_device_off(
            for_start(context, machine),
            Desktop::asked_in_environment(),
            context.recorded_intel_card.is_some() && !lab_device_on_recorded_card(context),
            std::env::var_os(RUN_VARIABLE).as_deref(),
            given_in_software(context).as_deref(),
        )
}

/// The desktop for this start in this process: the software desktop if the daemon gave it, and
/// otherwise the one chosen.
pub fn for_start(context: &Context, machine: &Machine) -> Desktop {
    if given_in_software(context).is_some() {
        return Desktop::Software;
    }
    chosen(context, machine, Desktop::asked_in_environment())
}

/// Where the daemon's log of the last hardware desktop that failed is kept, beside the fallback. The
/// run folder's log is replaced at each start and goes at each reboot, and a report needs the failed
/// start's: its self-test, its stage, the first layer that refused and its last submissions.
pub fn hardware_log_file(context: &Context, machine: &Machine) -> PathBuf {
    machine.folder(context).join(HARDWARE_LOG_FILE)
}

/// Keeps this start's daemon log as the failed hardware desktop's, cut to [`bounded_log`]'s size.
pub fn keep_hardware_log(context: &Context, machine: &Machine) -> std::io::Result<()> {
    let log = bounded_log(&context.run_folder(None).join(crate::daemon::OUTPUT_FILE))?;
    fs::write(hardware_log_file(context, machine), log)
}

/// The log whole if it's small. A bigger one, as a verbose log is, keeps its start, which has the
/// self-test and the stage, every warning and error line after that (the layers that refused), and
/// its end, which has the last submissions, with what was left out said where it was.
#[allow(clippy::disallowed_methods)] // the daemon's own log file, read to its end
fn bounded_log(path: &Path) -> std::io::Result<Vec<u8>> {
    let size = fs::metadata(path)?.len();
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut kept = Vec::new();
    if size <= LOG_START_BYTES + LOG_END_BYTES {
        reader.read_to_end(&mut kept)?;
        return Ok(kept);
    }
    let end_starts = size - LOG_END_BYTES;
    let (mut at, mut warnings, mut left_out) = (0u64, 0u64, 0u64);
    let mut line = Vec::new();
    while at < end_starts {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)? as u64;
        if read == 0 {
            break;
        }
        if at < LOG_START_BYTES {
            kept.extend_from_slice(&line);
        } else if worth_keeping(&line) && warnings + read <= LOG_WARNING_BYTES {
            if warnings == 0 {
                kept.extend_from_slice(
                    b"[... from here only warnings and errors, up to the log's end ...]\n",
                );
            }
            warnings += read;
            kept.extend_from_slice(&line);
        } else {
            left_out += read;
        }
        at += read;
    }
    kept.extend_from_slice(
        format!("[... {left_out} bytes of other lines left out; the log's end follows ...]\n").as_bytes(),
    );
    reader.read_to_end(&mut kept)?;
    Ok(kept)
}

fn worth_keeping(line: &[u8]) -> bool {
    line.starts_with(b"WARN")
        || line.starts_with(b"ERROR")
        || line.windows(12).any(|bytes| bytes == b"intel stage=")
}

/// Why this start's daemon gave the software desktop though the hardware one was asked, if it did.
pub fn given_in_software(context: &Context) -> Option<String> {
    read_note(&context.run_folder(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Scratch, scratch_context};
    use guesthouse_backend_intel::stage::STAGE_DESKTOP;

    fn machine(context: &Context) -> Machine {
        let machine = Machine::new("arc", "Arc", "/m/disk.qcow2", "");
        fs::create_dir_all(machine.folder(context)).unwrap();
        machine
    }

    #[test]
    fn with_either_default_a_fallback_gives_software_until_a_new_word() {
        for default in [Desktop::Software, Desktop::Hardware] {
            assert_eq!(pick(None, default, None), default);
            assert_eq!(pick(None, default, Some(false)), Desktop::Software);
            assert_eq!(pick(None, default, Some(true)), Desktop::Software);
            assert_eq!(
                pick(Some(Desktop::Hardware), default, Some(false)),
                Desktop::Hardware,
                "asked after the stage's try"
            );
            assert_eq!(
                pick(Some(Desktop::Hardware), default, Some(true)),
                Desktop::Software,
                "the same word failed"
            );
            assert_eq!(pick(Some(Desktop::Hardware), default, None), Desktop::Hardware);
            assert_eq!(pick(Some(Desktop::Software), default, None), Desktop::Software);
        }
    }

    #[test]
    fn nobody_asking_gives_the_stages_desktop_and_a_recorded_card_the_software_one() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        let machine = machine(&context);
        assert_eq!(chosen(&context, &machine, None), STAGE_DESKTOP);
        context.recorded_intel_card = Some(scratch.path("b580.card"));
        assert_eq!(chosen(&context, &machine, None), Desktop::Software);
        assert_eq!(chosen(&context, &machine, Some(Desktop::Hardware)), Desktop::Hardware);
    }

    #[test]
    fn a_fallback_is_read_back_and_lasts_for_its_build_only() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = machine(&context);
        fs::write(
            fallback_file(&context, &machine),
            format!("{VERSION}\n{ASKED}\nWindows stopped the card\n"),
        )
        .unwrap();
        let kept = fallback(&context, &machine).unwrap();
        let expected = Fallback {
            build: VERSION.to_owned(),
            asked: true,
            reason: "Windows stopped the card".to_owned(),
        };
        assert_eq!(kept, expected);
        assert_eq!(chosen(&context, &machine, Some(Desktop::Hardware)), Desktop::Software);
        fs::write(fallback_file(&context, &machine), format!("0.0.1\n{ASKED}\nan older build's\n")).unwrap();
        assert_eq!(
            chosen(&context, &machine, Some(Desktop::Hardware)),
            Desktop::Hardware,
            "a new build tries again"
        );
        assert_eq!(chosen(&context, &machine, None), STAGE_DESKTOP);
    }

    #[test]
    fn a_hardware_desktop_the_daemon_gave_in_software_is_the_software_desktop() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = machine(&context);
        fs::create_dir_all(context.run_folder(None)).unwrap();
        let note = |why| guesthouse_daemon::desktop_note::write_note(&context.run_folder(None), why).unwrap();
        note(Some("stage=Listen layer=card"));
        assert_eq!(given_in_software(&context).as_deref(), Some("stage=Listen layer=card"));
        assert_eq!(for_start(&context, &machine), Desktop::Software);
        assert!(!for_start(&context, &machine).has_display_driver());
        note(None);
        assert_eq!(
            for_start(&context, &machine),
            chosen(&context, &machine, Desktop::asked_in_environment())
        );
    }

    #[test]
    fn falling_back_writes_this_build() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = machine(&context);
        fall_back(&context, &machine, "the compositor kept crashing").unwrap();
        let kept = fallback(&context, &machine).unwrap();
        assert_eq!((kept.build.as_str(), kept.reason.as_str()), (VERSION, "the compositor kept crashing"));
    }

    #[test]
    fn the_device_goes_off_on_the_software_desktop_unless_a_person_asked_for_work_on_a_card() {
        let (soft, hard) = (Desktop::Software, Desktop::Hardware);
        let word = |word| Some(OsStr::new(word));
        let compute = word("compute");
        assert!(!switches_device_off(hard, Some(hard), false, None, None), "the hardware desktop keeps it");
        assert!(switches_device_off(soft, None, false, None, None), "fallen back or by default");
        assert!(
            switches_device_off(soft, Some(hard), true, None, None),
            "the daemon gave the software desktop"
        );
        assert!(!switches_device_off(soft, Some(soft), false, None, None), "asked, with work on the card");
        assert!(switches_device_off(soft, Some(soft), true, None, None), "a recorded card runs nothing");
        for listening in ["listen", "0", "LISTEN", " listen ", "yes"] {
            assert!(switches_device_off(soft, Some(soft), false, word(listening), None), "{listening:?}");
        }
        assert!(!switches_device_off(soft, None, false, compute, None), "compute asked keeps it");
        assert!(!switches_device_off(soft, None, false, word("Compute"), None));
        assert!(switches_device_off(soft, None, true, compute, None), "but not on a recorded card");
        let listens = Some("stage=Listen layer=host-self-test: 1 of its steps failed");
        assert!(switches_device_off(soft, Some(soft), false, None, listens), "the daemon runs nothing");
        assert!(switches_device_off(soft, None, false, compute, listens));
        let compute_note = Some("stage=Compute: GUESTHOUSE_INTEL_RUN_ON_CARD=compute runs compute work only");
        assert!(!switches_device_off(soft, None, false, compute, compute_note));
    }

    #[test]
    fn a_recorded_card_keeps_the_device_off_unless_a_lab_asks_and_its_daemon_runs_work() {
        let (soft, compute) = (Desktop::Software, Some(OsStr::new("compute")));
        let one = Some(OsStr::new("1"));
        assert!(recorded_for_the_device(true, None), "a packaged build on a recorded card");
        assert!(recorded_for_the_device(true, Some(OsStr::new("yes"))));
        assert!(!recorded_for_the_device(false, one), "a real card is a real card");
        assert!(!recorded_for_the_device(true, one));
        let off = |lab| switches_device_off(soft, None, recorded_for_the_device(true, lab), compute, None);
        assert!(off(None), "as today");
        assert!(!off(one), "a lab's compute ask keeps it, as on a real card");
        let listens = Some("stage=Listen layer=card: it is a recording");
        assert!(
            switches_device_off(soft, None, recorded_for_the_device(true, one), compute, listens),
            "a daemon without the lab card listens, and the device goes off"
        );
    }

    #[test]
    fn a_failed_hardware_desktops_log_is_kept_whole_or_cut_to_its_start_warnings_and_end() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = machine(&context);
        fs::create_dir_all(context.run_folder(None)).unwrap();
        let log = context.run_folder(None).join(crate::daemon::OUTPUT_FILE);
        let kept = || fs::read_to_string(hardware_log_file(&context, &machine)).unwrap();
        let small = "INFO  [    0.000] intel stage=Everything: the guest's work runs on the card\n";
        fs::write(&log, small).unwrap();
        keep_hardware_log(&context, &machine).unwrap();
        assert_eq!(kept(), small);

        let filler = "INFO  [    1.000] intel SubmitToHardwareQueue queue=0x40021a40 answer=started\n";
        let mut text = String::from("INFO  [    0.000] intel self-test step 1 of 13: PASS\n");
        text.push_str(small);
        while (text.len() as u64) < LOG_START_BYTES + 1 {
            text.push_str(filler);
        }
        let refused =
            "WARN  [    7.814] intel work: queue 0x40014700 stops: its batch neither ends nor parks\n";
        text.push_str(refused);
        while (text.len() as u64) < LOG_START_BYTES + 2 * LOG_END_BYTES {
            text.push_str(filler);
        }
        let last = "INFO  [  277.000] intel SubmitToHardwareQueue queue=0x4000a1c0 answer=started\n";
        text.push_str(last);
        fs::write(&log, &text).unwrap();
        keep_hardware_log(&context, &machine).unwrap();
        let kept = kept();
        assert!(
            kept.len() as u64 <= LOG_START_BYTES + LOG_WARNING_BYTES + LOG_END_BYTES + 4096,
            "{}",
            kept.len()
        );
        assert!(kept.starts_with("INFO  [    0.000] intel self-test step 1 of 13: PASS\n"));
        assert!(kept.contains(small) && kept.contains(refused), "the stage and the layer that refused");
        assert!(kept.contains("bytes of other lines left out"));
        assert!(kept.ends_with(last), "the last submissions");
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Card {
        /// A real card whose self-test passes.
        Real,
        /// A real card the daemon leaves listening: its self-test failed, or it isn't on xe.
        Listening,
        Recorded,
    }

    /// One start, from the two switches, a fallback on this build (whether a person had asked for
    /// what failed) and the card, through the app's choice, the daemon's stage and note and the
    /// app's device step: the desktop given, whether the Intel device is on in the guest, and
    /// whether work runs on the card.
    fn start(
        run: Option<&str>,
        desktop: Option<&str>,
        fell_back: Option<bool>,
        card: Card,
    ) -> (Desktop, bool, bool) {
        use guesthouse_backend_intel::stage::Fallback as Refused;
        let run = run.map(OsStr::new);
        let asked = Desktop::asked(desktop.map(OsStr::new));
        let recorded = card == Card::Recorded;
        let chosen = pick(asked, Desktop::default_for(recorded), fell_back);
        let (stage, listening) = match card {
            Card::Real => Stage::asked(run),
            Card::Listening => (Stage::Listen, Some(Refused::SelfTestFailed(vec!["binds".into()]))),
            Card::Recorded => (Stage::Listen, Some(Refused::RecordedCard)),
        };
        let (given, why) = chosen.at_stage(stage, listening.as_ref(), false);
        let note = Desktop::note(given, why, stage, listening.as_ref());
        let for_start = if note.is_some() { Desktop::Software } else { chosen };
        assert_eq!(for_start, given, "the app and the daemon agree on the desktop");
        let on = !switches_device_off(given, asked, recorded, run, note.as_deref());
        (given, on, on && stage.is_real())
    }

    #[test]
    fn every_switch_fallback_and_card_gives_the_desktop_device_and_work_of_the_docs_table() {
        use Card::{Listening, Real, Recorded};
        let (hard, soft) = (Desktop::Hardware, Desktop::Software);
        let (by_default, asked) = (Some(false), Some(true));
        // (stage word, desktop word, fell back, card) => (desktop, device on, work on the card)
        let rows = [
            ((None, None, None, Real), (hard, true, true)),
            ((None, None, by_default, Real), (soft, false, false)),
            ((None, None, asked, Real), (soft, false, false)),
            ((None, Some("0"), None, Real), (hard, true, true)),
            ((None, Some("0"), by_default, Real), (hard, true, true)),
            ((None, Some("0"), asked, Real), (soft, false, false)),
            ((None, Some("1"), None, Real), (soft, true, true)),
            ((None, Some("1"), by_default, Real), (soft, true, true)),
            ((None, Some("1"), asked, Real), (soft, true, true)),
            ((Some("1"), None, None, Real), (hard, true, true)),
            ((Some("everything"), None, by_default, Real), (soft, false, false)),
            ((Some("compute"), None, None, Real), (soft, true, true)),
            ((Some("compute"), None, by_default, Real), (soft, true, true)),
            ((Some("compute"), None, asked, Real), (soft, true, true)),
            ((Some("compute"), Some("0"), None, Real), (soft, true, true)),
            ((Some("compute"), Some("0"), asked, Real), (soft, true, true)),
            ((Some("compute"), Some("1"), by_default, Real), (soft, true, true)),
        ];
        for ((run, desktop, fell_back, card), expected) in rows {
            assert_eq!(
                start(run, desktop, fell_back, card),
                expected,
                "{run:?} {desktop:?} {fell_back:?} {card:?}"
            );
        }
        let any_run = [None, Some("1"), Some("compute"), Some("0"), Some("listen"), Some("yes")];
        let nothing_runs = [Some("0"), Some("listen"), Some("LISTEN"), Some("yes")];
        for desktop in [None, Some("0"), Some("1")] {
            for fell_back in [None, by_default, asked] {
                for run in nothing_runs {
                    assert_eq!(
                        start(run, desktop, fell_back, Real),
                        (soft, false, false),
                        "{run:?} {desktop:?}"
                    );
                }
                for run in any_run {
                    for card in [Listening, Recorded] {
                        let got = start(run, desktop, fell_back, card);
                        assert_eq!(got, (soft, false, false), "{run:?} {desktop:?} {fell_back:?} {card:?}");
                    }
                }
            }
        }
    }
}
