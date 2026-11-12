use super::*;
use guesthouse_daemon::guest_memory::window_base;

fn refused(context: &Context, words: &str) -> String {
    match said(context, words).1 {
        Err(Problem::Failed(failure)) => failure.0,
        other => panic!("{words:?} gave {other:?}"),
    }
}

/// Each word of QEMU's part of the output, which a person could paste into a script.
fn qemu_words(out: &str) -> Vec<String> {
    let after = out.split("\n# QEMU.\n").nth(1).expect("a QEMU part");
    let lines = after.lines().skip_while(|line| line.starts_with('#'));
    let lines = lines.take_while(|line| !line.is_empty() && !line.starts_with('#'));
    lines.flat_map(|line| line.trim_end_matches(" \\").split_whitespace().map(str::to_owned)).collect()
}

#[test]
fn a_windows_machine_gets_its_daemon_and_qemu_with_the_memory_in_files() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "win", "Win", Kind::Windows);
    let (out, result) = said(&context, "command win");
    assert_eq!(result, Ok(()));
    let ram = format!("{}/gpupv-ram.standalone", context.paths.shared_memory.display());
    let window = format!("{}/gpupv-window.standalone", context.paths.shared_memory.display());
    for wanted in [
        format!("GUESTHOUSE_RAM_FILE={ram}"),
        format!("GUESTHOUSE_WINDOW_FILE={window}"),
        "GUESTHOUSE_RAM=16G".to_owned(),
        "GUESTHOUSE_SLOT=0".to_owned(),
        format!("GUESTHOUSE_STORE={}", context.paths.store.display()),
        format!("rm -f {ram} {window}"),
    ] {
        assert!(out.lines().any(|line| line.trim_start().starts_with(&wanted)), "{wanted}\n{out}");
    }
    let daemon_at = out.find("guesthouse-daemon").unwrap();
    let wait_at = out.find("until [ -S").unwrap();
    let qemu_at = out.find("\n# QEMU.\n").unwrap();
    let stop_at = out.find("kill \"$daemon\"").unwrap();
    assert!(daemon_at < wait_at && wait_at < qemu_at && qemu_at < stop_at, "{out}");
}

/// Every word of the fixture that names no path is a word of the guest's machine, and must be there,
/// in the same order. The paths differ with the PC, and the two memory paths are what this changes.
#[test]
fn the_qemu_words_are_the_guests_machine_in_the_same_order() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "win", "Win", Kind::Windows);
    let words = qemu_words(&said(&context, "command win").0);
    let base = format!("{:#x}", window_base());
    let fixture = include_str!("../../vm_args/gpu-arguments.txt").lines();
    let device_words = fixture
        .filter(|word| !word.contains('/') && *word != "-L")
        .map(|word| word.replace("0x10000000000", &base));
    let mut remaining = words.iter();
    for wanted in device_words {
        assert!(remaining.any(|word| *word == wanted), "{wanted} is missing or out of order in {words:?}");
    }
    let shared = context.paths.shared_memory.display();
    for (id, name) in [("gpio", "window"), ("ram0", "ram")] {
        let wanted = format!("mem-path={shared}/gpupv-{name}.standalone,share=on");
        assert!(
            words
                .iter()
                .any(|word| word.starts_with(&format!("memory-backend-file,id={id},"))
                    && word.ends_with(&wanted)),
            "{wanted}"
        );
    }
    assert!(!words.iter().any(|word| word.contains("/proc/self/fd")), "nothing is handed over");
    assert!(!words.iter().any(|word| word.contains("guesthouse-jail") || word.contains("bwrap")));
    assert!(
        words
            .iter()
            .any(|word| word.starts_with("socket,id=gpv,path=")
                && word.ends_with("/vgpu.sock,reconnect-ms=500"))
    );
}

#[test]
fn a_tpm_is_a_step_of_its_own_before_the_daemon() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = Machine { tpm: true, ..make(&context, "win", "Win", Kind::Windows) };
    machine.save(&context).unwrap();
    let out = said(&context, "command win").0;
    let tpm_at = out.find("\nswtpm socket").unwrap();
    assert!(tpm_at < out.find("guesthouse-daemon").unwrap(), "{out}");
    assert!(out.contains("--tpm2") && out.contains("tpm.sock"), "{out}");
}

#[test]
fn a_plain_machine_prints_qemu_alone() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    let out = said(&context, "command work").0;
    assert!(out.contains("\n# QEMU.\n"), "{out}");
    for absent in ["guesthouse-daemon", "GUESTHOUSE_", "gpupv-", "until [ -S", "kill "] {
        assert!(!out.contains(absent), "{absent}\n{out}");
    }
    assert!(!qemu_words(&out).iter().any(|word| word.contains("gpupv-")));
}

#[test]
fn a_machine_that_cannot_be_started_this_way_says_why() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "ubu", "Ubu", Kind::Linux);
    let refusal = refused(&context, "command ubu");
    assert!(refusal.starts_with("Ubu is a Linux machine with a GPU, and command is for Windows machines"));
    Machine { installing: true, ..make(&context, "new", "New", Kind::Windows) }.save(&context).unwrap();
    assert!(refused(&context, "command new").contains("isn't installed yet"));
}

#[test]
fn nothing_is_started_or_made() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "win", "Win", Kind::Windows);
    let _ = said(&context, "command win");
    assert!(!context.paths.shared_memory.join("gpupv-ram.standalone").exists());
    assert!(!context.run_folder(None).exists());
}

#[test]
fn what_the_pc_lacks_is_noted_and_the_command_still_prints() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "win", "Win", Kind::Windows);
    let out = said(&context, "command win").0;
    assert!(out.lines().any(|line| line.starts_with("# Note: ")), "{out}");
    assert!(out.contains("guesthouse-daemon"));
}
