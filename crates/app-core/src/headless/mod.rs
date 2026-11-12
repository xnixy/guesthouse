//! `guesthouse-vm`: the machines from a terminal, with no window. It does what the app's buttons do, by
//! the same calls, so a machine started here is the app's to stop and the other way round.

mod args;
mod change;
mod make;
mod pick;
mod setup;
mod standalone;
mod steps;
#[cfg(test)]
mod tests;

use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::log::read_log;
use crate::machine::{Kind, Machine, load_all};
use crate::setup::checks::gpu_warning;
use crate::slots;
use crate::tasks::{State, Status, launch, status};
use args::{Command, parse};
use pick::find;
use std::fmt;
use std::io::{self, Write};
use std::time::Duration;
use steps::Steps;

/// How often a running task is looked at.
const POLL: Duration = Duration::from_millis(300);
const LOG_LINES: usize = 40;

pub const USAGE: &str = "usage: guesthouse-vm COMMAND [MACHINE]

  list                       each machine and what it is doing
  status MACHINE             one machine in full
  start MACHINE [--no-wait]  start it, saying each step until it is ready
  stop MACHINE               ask the guest to shut down, and wait for it
  off MACHINE                cut the power, as Force Off does
  log MACHINE                where the log is, and the end of it
  command MACHINE            what would start it beside your own QEMU or libvirt, printed

  new NAME --iso PATH [--kind windows|ubuntu|plain] [--disk GIB] [--cores N] [--ram GIB]
                             [--user NAME] [--amd-card PCI]
  install MACHINE [--no-wait]
                             run the install of a Windows or Ubuntu machine
  remove MACHINE [--yes]     delete it, or forget it if its disk is elsewhere

  show MACHINE               its settings
  set MACHINE SETTING VALUE  cores, ram, vram, screen, refresh, network, passt, bridge, sound, share
  forward MACHINE add|remove tcp|udp HOST GUEST
                             a port on this PC that reaches a port in the guest

  setup [--driver PATH] [--card PCI]
                             the first-run setup for GPU machines

MACHINE is a machine's id, or its name in any case.";

/// Why a command did not happen. A mistake in the words typed is told apart, as it ends with the usage.
#[derive(Debug, PartialEq, Eq)]
pub enum Problem {
    Usage(String),
    Failed(Failure),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(text) => f.write_str(text),
            Self::Failed(failure) => failure.fmt(f),
        }
    }
}

impl From<Failure> for Problem {
    fn from(failure: Failure) -> Self {
        Self::Failed(failure)
    }
}

impl From<io::Error> for Problem {
    fn from(error: io::Error) -> Self {
        Self::Failed(error.into())
    }
}

/// Runs what the words ask, writing what a person should read to `out`.
pub fn run(context: &Context, arguments: &[String], out: &mut dyn Write) -> std::result::Result<(), Problem> {
    match parse(arguments)? {
        Command::Help => writeln!(out, "{USAGE}")?,
        Command::List => list(context, out)?,
        Command::Status(name) => show_status(context, &find(context, name)?, out)?,
        Command::Show(name) => change::show(context, &find(context, name)?, out)?,
        Command::Start { name, wait } => start(context, &find(context, name)?, wait, out)?,
        Command::Install { name, wait } => make::install(context, &find(context, name)?, wait, out)?,
        Command::Stop(name) => stop(context, &find(context, name)?, "stop", out)?,
        Command::Off(name) => stop(context, &find(context, name)?, "force", out)?,
        Command::Log(name) => show_log(context, &find(context, name)?, out)?,
        Command::Standalone(name) => standalone::command(context, &find(context, name)?, out)?,
        Command::New { name, options } => make::new_machine(context, name, &options, out)?,
        Command::Set { name, key, value } => change::set(context, find(context, name)?, key, value, out)?,
        Command::Forward { name, change } => change::forward(context, find(context, name)?, &change, out)?,
        Command::Setup { driver, card } => setup::setup(context, driver, card, out)?,
        Command::Remove { name, yes } => make::remove(context, &find(context, name)?, yes, out)?,
    }
    Ok(())
}

fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Windows => "windows",
        Kind::Plain => "plain",
        Kind::Linux => "linux",
    }
}

/// The state as the app's row opens its subtitle, with the step while there is one.
fn state_text(found: &Status) -> String {
    if found.step.is_empty() {
        found.state.name().to_owned()
    } else {
        format!("{}: {}", found.state.name(), found.step)
    }
}

fn list(context: &Context, out: &mut dyn Write) -> Result<()> {
    let (machines, unreadable) = load_all(context);
    let rows: Vec<[String; 4]> = machines
        .iter()
        .map(|machine| {
            let state = state_text(&status(context, machine));
            [machine.name.clone(), machine.id.clone(), kind_word(machine.kind).to_owned(), state]
        })
        .collect();
    let width = |column: usize| rows.iter().map(|row| row[column].chars().count()).max().unwrap_or(0);
    let (names, ids, kinds) = (width(0), width(1), width(2));
    for [name, id, kind, state] in &rows {
        writeln!(out, "{name:names$}  {id:ids$}  {kind:kinds$}  {state}")?;
    }
    if machines.is_empty() && unreadable.is_empty() {
        writeln!(out, "There are no machines yet. They are made in the app.")?;
    }
    for problem in unreadable {
        writeln!(out, "{problem}")?;
    }
    Ok(())
}

fn show_status(context: &Context, machine: &Machine, out: &mut dyn Write) -> Result<()> {
    let found = status(context, machine);
    writeln!(out, "Name:  {}", machine.name)?;
    writeln!(out, "Id:    {}", machine.id)?;
    writeln!(out, "Kind:  {}", kind_word(machine.kind))?;
    writeln!(out, "State: {}", state_text(&found))?;
    if let Some(slot) = found.slot {
        writeln!(out, "Slot:  {slot}")?;
    }
    let notes = [
        &found.error,
        &found.display_problem,
        &found.identity_note,
        &found.jail_note,
        &found.card_note,
        &found.guest_gpu,
    ];
    for note in notes.into_iter().filter(|note| !note.is_empty()) {
        writeln!(out, "{note}")?;
    }
    Ok(())
}

/// Says each step of the task as the log gets it, until `ended` says it has. A step that comes and goes
/// between two looks is in the log, so it is said all the same.
fn follow(
    context: &Context,
    machine: &Machine,
    steps: &mut Steps,
    out: &mut dyn Write,
    mut ended: impl FnMut() -> bool,
) -> Result<Status> {
    loop {
        let done = ended();
        for step in steps.new_steps() {
            writeln!(out, "{step}")?;
        }
        if done {
            return Ok(status(context, machine));
        }
        context.clock.sleep(POLL);
    }
}

/// Starts the task and, unless told not to, follows it to its end.
fn run_task(
    context: &Context,
    machine: &Machine,
    action: &str,
    wait: bool,
    out: &mut dyn Write,
) -> Result<Option<Status>> {
    let mut steps = Steps::from_end(context, action);
    let mut task = launch(context, action, machine, &[], None)?;
    if !wait {
        return Ok(None);
    }
    let ended = || !matches!(task.try_wait(), Ok(None));
    follow(context, machine, &mut steps, out, ended).map(Some)
}

/// What a task that ended without a word of its own still owes the person who waited for it.
fn follow_up(found: &Status, said_nothing: String) -> Failure {
    Failure(if found.error.is_empty() { said_nothing } else { found.error.clone() })
}

fn start(context: &Context, machine: &Machine, wait: bool, out: &mut dyn Write) -> Result<()> {
    let name = &machine.name;
    if machine.installing {
        let text = format!("The install of {name} did not finish. Start Over in the app runs it again.");
        return Err(Failure(text));
    }
    let before = status(context, machine);
    match before.state {
        State::Running => return Ok(writeln!(out, "{name} is already running.")?),
        State::Off => {}
        _ => {
            return Err(Failure(format!("{name} is {}.", before.state.name().to_lowercase())));
        }
    }
    if machine.kind == Kind::Windows
        && let Some(warning) = gpu_warning(context)
    {
        writeln!(out, "Note: {warning}")?;
    }
    let Some(found) = run_task(context, machine, "start", wait, out)? else {
        return Ok(writeln!(out, "Starting {name}. status shows how far it is.")?);
    };
    if found.state != State::Running {
        let log = context.paths.log_file();
        return Err(follow_up(&found, format!("{name} did not start. The log has more: {}", log.display())));
    }
    if !found.error.is_empty() {
        writeln!(out, "Note: {}", found.error)?;
    }
    Ok(writeln!(out, "{name} is running.")?)
}

/// `stop` asks the guest, and `force` cuts the power. Both leave the machine off.
fn stop(context: &Context, machine: &Machine, action: &str, out: &mut dyn Write) -> Result<()> {
    let name = &machine.name;
    let before = status(context, machine);
    match (before.state, action) {
        (State::Off, _) => return Ok(writeln!(out, "{name} is already off.")?),
        (State::Running, _) | (_, "force") => {}
        _ => {
            let text = format!("{name} is {}. off turns it off now.", before.state.name().to_lowercase());
            return Err(Failure(text));
        }
    }
    let found = run_task(context, machine, action, true, out)?.unwrap_or_default();
    if found.state != State::Off {
        return Err(follow_up(&found, format!("{name} is still running.")));
    }
    if !found.error.is_empty() {
        writeln!(out, "Note: {}", found.error)?;
    }
    Ok(writeln!(out, "{name} is off.")?)
}

fn show_log(context: &Context, machine: &Machine, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "Log: {}", context.paths.log_file().display())?;
    if let Some(slot) = status(context, machine).slot {
        let folder = context.run_folder(Some(slots::number(slot)));
        writeln!(out, "Run folder: {}", folder.display())?;
    }
    let text = read_log(context);
    if text.is_empty() {
        return Ok(writeln!(out, "The log is empty.")?);
    }
    writeln!(out, "The log has every machine's lines. The end of it:\n")?;
    let lines: Vec<&str> = text.lines().collect();
    for line in &lines[lines.len().saturating_sub(LOG_LINES)..] {
        writeln!(out, "{line}")?;
    }
    Ok(())
}
