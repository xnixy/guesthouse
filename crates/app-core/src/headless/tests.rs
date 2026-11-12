use super::*;
use crate::log::write_log;
use crate::processes::stand_in::fake_process;
use crate::tasks::{TaskRecord, write_task};
use crate::testing::{Scratch, scratch_context};
use std::fs;
use std::io::Write;

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

fn make(context: &Context, id: &str, name: &str, kind: Kind) -> Machine {
    let machine = Machine { kind, ..Machine::new(id, name, &format!("/d/{id}"), "/v") };
    fs::create_dir_all(machine.folder(context)).unwrap();
    machine.save(context).unwrap();
    machine
}

fn running(context: &Context, machine: &Machine, pid: u32) {
    let qemu = context.paths.qemu.display().to_string();
    let monitor = format!("unix:{}/qmp.sock,server=on", context.run_folder(None).display());
    let disk = format!("file={},format=qcow2", machine.disk);
    fake_process(context, pid, &[&qemu, "-qmp", &monitor, &disk], &[], "");
}

fn said(context: &Context, command: &str) -> (String, std::result::Result<(), Problem>) {
    let mut out = Vec::new();
    let result = run(context, &words(command), &mut out);
    (String::from_utf8(out).unwrap(), result)
}

fn usage_of(command: &str) -> String {
    let scratch = Scratch::new();
    match said(&scratch_context(&scratch), command).1 {
        Err(Problem::Usage(text)) => text,
        other => panic!("{command:?} gave {other:?}"),
    }
}

#[test]
fn the_words_typed_are_checked_before_anything_is_touched() {
    assert_eq!(usage_of(""), USAGE);
    assert!(usage_of("frobnicate").starts_with("guesthouse-vm: there is no command called frobnicate."));
    assert!(usage_of("start").contains("start needs a machine."));
    assert!(usage_of("stop a b").contains("stop takes only a machine."));
    assert!(usage_of("list a").contains("list takes no words, only options."));
    assert!(usage_of("stop a --no-wait").contains("stop has no option --no-wait."));
    assert!(usage_of("start a --wait").contains("start has no option --wait."));
}

#[test]
fn help_is_not_a_mistake() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    for word in ["--help", "-h", "help"] {
        let (out, result) = said(&context, word);
        assert_eq!((out, result), (format!("{USAGE}\n"), Ok(())));
    }
}

#[test]
fn a_machine_is_found_by_its_id_or_its_name_in_any_case() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    for (id, name) in [("work", "Office PC"), ("lab", "Lab"), ("lab-2", "LAB")] {
        make(&context, id, name, Kind::Plain);
    }
    assert_eq!(find(&context, "work").unwrap().name, "Office PC");
    assert_eq!(find(&context, "office pc").unwrap().id, "work");
    assert_eq!(find(&context, "lab").unwrap().id, "lab", "an id beats a name that reads the same");
    assert_eq!(find(&context, "lab-2").unwrap().name, "LAB");
}

#[test]
fn a_name_that_fits_two_machines_lists_them() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "one", "Lab", Kind::Plain);
    make(&context, "two", "LAB", Kind::Plain);
    let refusal = find(&context, "lab").unwrap_err().0;
    assert_eq!(refusal, "lab could be any of one (Lab), two (LAB). Use the id.");
}

#[test]
fn an_unknown_name_lists_the_machines() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    assert_eq!(find(&context, "x").unwrap_err().0, "There are no machines yet. They are made in the app.");
    make(&context, "work", "Office PC", Kind::Windows);
    make(&context, "lab", "Lab", Kind::Plain);
    assert_eq!(
        find(&context, "x").unwrap_err().0,
        "There is no machine called x. The machines are lab (Lab), work (Office PC)."
    );
}

#[test]
fn the_list_has_a_line_for_each_machine_in_columns() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let work = make(&context, "work", "Office PC", Kind::Plain);
    make(&context, "lab", "Lab", Kind::Linux);
    running(&context, &work, 40);
    let (out, result) = said(&context, "list");
    assert_eq!(result, Ok(()));
    assert_eq!(out, "Lab        lab   linux  Off\nOffice PC  work  plain  Running\n");
}

#[test]
fn an_empty_library_says_so() {
    let scratch = Scratch::new();
    let (out, result) = said(&scratch_context(&scratch), "list");
    assert_eq!((out.as_str(), result), ("There are no machines yet. They are made in the app.\n", Ok(())));
}

#[test]
fn a_file_that_cannot_be_read_is_named_after_the_list() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "lab", "Lab", Kind::Plain);
    scratch.write("home/.local/share/guesthouse/machines/broken/machine.json", "{");
    let (out, _) = said(&context, "list");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].starts_with("Lab  lab  plain  Off") && lines[1].starts_with("Could not read "));
}

#[test]
fn status_shows_the_state_and_what_is_wrong() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = make(&context, "work", "Office PC", Kind::Plain);
    let record = TaskRecord {
        action: "start".into(),
        machine: "work".into(),
        error: "It broke.".into(),
        ..Default::default()
    };
    write_task(&context, None, &record).unwrap();
    let (out, _) = said(&context, "status work");
    assert_eq!(out, "Name:  Office PC\nId:    work\nKind:  plain\nState: Off\nIt broke.\n");
    running(&context, &machine, 40);
    let mut out = Vec::new();
    run(&context, &["status".to_owned(), "OFFICE PC".to_owned()], &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("State: Running\nSlot:  0\n"), "{out}");
}

#[test]
fn every_step_is_said_even_one_that_comes_and_goes_between_two_looks() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = make(&context, "work", "Office PC", Kind::Plain);
    write_log(&context, "start from an earlier task").unwrap();
    let mut steps = Steps::from_end(&context, "start");
    let mut looks = 0;
    let mut out = Vec::new();
    follow(&context, &machine, &mut steps, &mut out, || {
        let lines: &[&str] = match looks {
            0 => &["start Checking the machine"],
            // Two steps and the end of a task, all in the time between two looks.
            1 => &[
                "start Starting the VM",
                "stop Shutting down",
                "app: a problem",
                "start Waiting for the desktop",
            ],
            _ => &["start finished"],
        };
        for line in lines {
            write_log(&context, line).unwrap();
        }
        looks += 1;
        looks == 3
    })
    .unwrap();
    let said = "Checking the machine
Starting the VM
Waiting for the desktop
";
    assert_eq!(String::from_utf8(out).unwrap(), said);
}

#[test]
fn a_step_still_being_written_waits_for_the_next_look() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let mut steps = Steps::from_end(&context, "start");
    let mut log = crate::log::open_for_appending(&context).unwrap();
    write!(log, "10:00:00 start Starting the V").unwrap();
    assert!(steps.new_steps().is_empty());
    writeln!(log, "M").unwrap();
    assert_eq!(steps.new_steps(), ["Starting the VM"]);
    assert!(steps.new_steps().is_empty());
}

#[test]
fn a_machine_already_in_the_wanted_state_is_left_alone() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = make(&context, "work", "Office PC", Kind::Plain);
    assert_eq!(said(&context, "stop work"), ("Office PC is already off.\n".to_owned(), Ok(())));
    assert_eq!(said(&context, "off work").0, "Office PC is already off.\n");
    running(&context, &machine, 40);
    assert_eq!(said(&context, "start work"), ("Office PC is already running.\n".to_owned(), Ok(())));
}

#[test]
fn a_machine_that_is_busy_is_not_started_or_asked_to_stop() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    fake_process(&context, 79, &["guesthouse-task", "start", "work"], &[], "");
    let record = TaskRecord { action: "start".into(), machine: "work".into(), pid: 79, ..Default::default() };
    write_task(&context, None, &record).unwrap();
    assert_eq!(said(&context, "start work").1, Err(Problem::Failed(Failure::new("Office PC is starting."))));
    assert_eq!(
        said(&context, "stop work").1,
        Err(Problem::Failed(Failure::new("Office PC is starting. off turns it off now.")))
    );
}

#[test]
fn an_install_that_did_not_finish_is_left_to_the_app() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = Machine { installing: true, ..make(&context, "work", "Office PC", Kind::Windows) };
    machine.save(&context).unwrap();
    let refusal = said(&context, "start work").1.unwrap_err().to_string();
    assert_eq!(refusal, "The install of Office PC did not finish. Start Over in the app runs it again.");
}

/// A task that is really a process, to go through `launch` as the app does. It says what went wrong a
/// moment after `launch` has written its own record, which would otherwise be the last word.
#[test]
fn a_task_that_fails_ends_the_command_with_its_error() {
    let scratch = Scratch::new();
    let mut context = scratch_context(&scratch);
    context.processes = "/proc".into();
    // A slot that is named is kept to, so a real VM on this PC in another can't be taken for ours.
    context.slot = crate::slots::number(3);
    make(&context, "work", "Office PC", Kind::Plain);
    context.make_run_folder(None).unwrap();
    let file = context.run_folder(None).join("guesthouse-task.json");
    let record = TaskRecord {
        action: "start".into(),
        machine: "work".into(),
        error: "It broke.".into(),
        ..Default::default()
    };
    scratch.write("record.json", serde_json::to_string(&record).unwrap());
    let script = format!(
        "#!/bin/sh\nsleep 1\necho '10:00:00 start Starting the VM'\ncat '{}' > '{}'\n",
        scratch.path("record.json").display(),
        file.display()
    );
    scratch.program("program/guesthouse-task", &script);
    let (out, result) = said(&context, "start work");
    assert_eq!(result, Err(Problem::Failed(Failure::new("It broke."))));
    assert_eq!(out, "Starting the VM\n");
}

#[test]
fn no_wait_returns_once_the_task_is_launched() {
    let scratch = Scratch::new();
    let mut context = scratch_context(&scratch);
    context.processes = "/proc".into();
    context.slot = crate::slots::number(3);
    make(&context, "work", "Office PC", Kind::Plain);
    scratch.program("program/guesthouse-task", "#!/bin/sh\nexit 0\n");
    let (out, result) = said(&context, "start work --no-wait");
    assert_eq!(result, Ok(()));
    assert_eq!(out, "Starting Office PC. status shows how far it is.\n");
}

#[test]
fn the_log_command_says_where_the_log_is_and_shows_its_end() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    let log = context.paths.log_file().display().to_string();
    assert_eq!(said(&context, "log work").0, format!("Log: {log}\nThe log is empty.\n"));
    for number in 0..LOG_LINES + 5 {
        crate::log::write_log(&context, &format!("line {number}")).unwrap();
    }
    let (out, _) = said(&context, "log work");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], format!("Log: {log}"));
    assert_eq!(lines.len(), 3 + LOG_LINES);
    assert!(lines[3].ends_with("line 5") && lines.last().unwrap().ends_with("line 44"), "{out}");
}

mod making_tests;
mod settings_tests;
mod standalone_tests;
