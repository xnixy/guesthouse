// SPDX-License-Identifier: GPL-2.0-or-later
//! `guesthouse-report`, the report tool for when the app will not open: the Linux host's tool by
//! name, options and output as far as they apply, built from the same functions the app uses. It
//! shows what the report holds before anything is sent, and sends only after a typed `yes`.

use super::bundle::{self, Prepared};
use super::collect::{self, Places};
use super::{ProblemKind, Request, before_sending};
use crate::store;
use guesthouse_report_core::contract::KEPT_DAYS;
use guesthouse_report_core::issue::issue_link;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = "\
guesthouse-report: sends a bug report about Guesthouse to its developer.

It gathers the app's log, one machine's logs from its last runs, its settings, and facts about this
Mac, takes your user name, host name and home folder (and passwords, pairing keys, device
fingerprints, pairing codes and serial numbers) out of them where it finds them, and writes them
into one file here. It shows what is in the file and asks before sending anything: you type yes.

  --machine NAME      the machine the report is about. Without it, the one that ran last.
  --kind KIND         what went wrong: wont-start, crashed, picture, slow, install-failed or other.
  --description TEXT  what went wrong, in your words. Without it, you're asked.
  --contact TEXT      how the developer can reach you, if you'd like them to. It isn't made public.
  --no-guest-serial   leaves out what the guest wrote to its serial port.
  --save-only         writes the file and stops, with nothing sent.
  --help              this.
";

/// What the options asked for.
#[derive(Debug, Default, PartialEq)]
pub struct Options {
    pub machine: Option<String>,
    pub kind: ProblemKind,
    pub description: Option<String>,
    pub contact: String,
    pub no_guest_serial: bool,
    pub save_only: bool,
}

/// The options, or the text to say instead (the help, or what was wrong).
pub fn read_options(arguments: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or_else(|| format!("{argument} needs a value after it."));
        match argument.as_str() {
            "--machine" => options.machine = Some(value()?),
            "--kind" => {
                let word = value()?;
                options.kind = ProblemKind::from_word(&word)
                    .ok_or_else(|| format!("{word} isn't a kind. {}", kinds()))?;
            }
            "--description" => options.description = Some(value()?),
            "--contact" => options.contact = value()?,
            "--no-guest-serial" => options.no_guest_serial = true,
            "--save-only" => options.save_only = true,
            "--help" | "-h" => return Err(HELP.to_owned()),
            other => return Err(format!("{other} isn't an option here.\n\n{HELP}")),
        }
    }
    Ok(options)
}

fn kinds() -> String {
    let words: Vec<&str> = ProblemKind::ALL.iter().map(|kind| kind.word()).collect();
    format!("The kinds are {}.", words.join(", "))
}

/// Where the tool reads its questions and writes its answers; a test gives it its own.
pub trait Terminal {
    /// The answer to a question, or nothing when nobody is there to answer.
    fn ask(&mut self, question: &str) -> String;
    fn say(&mut self, text: &str);
}

struct RealTerminal;

impl Terminal for RealTerminal {
    fn ask(&mut self, question: &str) -> String {
        if !io::stdin().is_terminal() {
            return String::new();
        }
        print!("{question}");
        let _ = io::stdout().flush();
        let mut line = String::new();
        #[allow(clippy::disallowed_methods)] // a line the person types at the terminal
        let _ = io::stdin().lock().read_line(&mut line);
        line.trim().to_owned()
    }

    fn say(&mut self, text: &str) {
        println!("{text}");
    }
}

/// How a report is sent: the app's `send::send` for real, and a stand-in for a test.
pub type Sender<'a> = &'a dyn Fn(&Path, &str) -> Result<String, String>;

/// A machine's folder name and the name its person gave it.
fn machines(support: &Path) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = fs::read_dir(support.join("Machines"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_string_lossy().into_owned();
            let text = fs::read_to_string(entry.path().join("machine.json")).ok()?;
            let name = serde_json::from_str::<serde_json::Value>(&text).ok()?["name"].as_str()?.to_owned();
            Some((id, name))
        })
        .collect();
    found.sort();
    found
}

/// The machine called `wanted` (by name or folder name, whatever its case), or the one that ran
/// last when none is named.
fn choose(support: &Path, wanted: Option<&str>) -> Result<Option<(String, String)>, String> {
    let all = machines(support);
    match wanted {
        Some(wanted) => all
            .iter()
            .find(|(id, name)| id.eq_ignore_ascii_case(wanted) || name.eq_ignore_ascii_case(wanted))
            .cloned()
            .map(Some)
            .ok_or_else(|| {
                let names: Vec<&str> = all.iter().map(|(_, name)| name.as_str()).collect();
                format!("There's no machine called {wanted}. The machines are: {}.", names.join(", "))
            }),
        None => Ok(all
            .into_iter()
            .max_by_key(|(id, _)| fs::metadata(support.join("Run").join(id)).and_then(|m| m.modified()).ok())
            .filter(|(id, _)| support.join("Run").join(id).is_dir())),
    }
}

/// Here, if this folder can be written to, and the temp folder if not.
fn output_folder() -> PathBuf {
    std::env::current_dir()
        .ok()
        .filter(|folder| fs::metadata(folder).is_ok_and(|m| !m.permissions().readonly()))
        .unwrap_or_else(std::env::temp_dir)
}

fn show_contents(term: &mut dyn Terminal, prepared: &Prepared) {
    term.say("The report holds:");
    for (name, bytes) in &prepared.files {
        term.say(&format!("  {name} ({bytes} bytes)"));
    }
    if !prepared.missing.is_empty() {
        term.say("\nNot in it:");
        for (name, reason) in &prepared.missing {
            term.say(&format!("  {name}: {reason}"));
        }
    }
    for note in &prepared.notes {
        term.say(&format!("  {note}"));
    }
}

/// Runs the tool: gathers, shows, writes, and sends only after a typed `yes`.
pub fn run(
    options: Options,
    places: &Places,
    app_version: &str,
    folder: &Path,
    term: &mut dyn Terminal,
    send: Sender,
) -> ExitCode {
    let machine = match choose(&places.support, options.machine.as_deref()) {
        Ok(machine) => machine,
        Err(text) => {
            eprintln!("{text}");
            return ExitCode::FAILURE;
        }
    };
    match &machine {
        Some((_, name)) => term.say(&format!("The report is about {name}.")),
        None => {
            term.say("No machine has run, so the report is about the app and this Mac. --machine picks one.")
        }
    }
    term.say("Gathering...");
    let description = match options.description {
        Some(text) => text,
        None => term.ask("What went wrong? One line, or Enter to skip: "),
    };
    let request = Request {
        machine: machine.map(|(id, _)| id),
        kind: options.kind.word().to_owned(),
        description: description.clone(),
        contact: options.contact,
        include_serial: !options.no_guest_serial,
        // Not asked of the agent from here: the tool works with the app closed.
        machine_running: false,
        app_version: app_version.to_owned(),
        driver_version: None,
    };
    let found = collect::collect(&request, places);
    let prepared = match bundle::prepare(&request, &found, places) {
        Ok(prepared) => prepared,
        Err(text) => {
            eprintln!("The report couldn't be written: {text}");
            return ExitCode::FAILURE;
        }
    };
    show_contents(term, &prepared);
    let saved = folder.join(prepared.archive.file_name().unwrap_or_default());
    if fs::copy(&prepared.archive, &saved).is_err() {
        term.say(&format!("It couldn't be copied here, so it is in {}", prepared.archive.display()));
    } else {
        term.say(&format!("\nWritten to {}", saved.display()));
    }
    term.say(&format!("The files are unpacked in {}, to read first.", prepared.folder.display()));
    if options.save_only {
        return ExitCode::SUCCESS;
    }
    term.say(&format!(
        "\nSending gives this file to Guesthouse's developer. It's read by the developer and their automation \
         tools, and deleted after {KEPT_DAYS} days. Your user name, host name and home folder were taken out \
         where they were found, but logs are free text, so look through it first if that matters to you."
    ));
    let thin = before_sending(request.machine.is_some(), false, found.has_run_logs, options.kind);
    let question = match thin {
        Some(text) => {
            term.say(&format!("\n{text}"));
            "Send anyway? Type yes to send: "
        }
        None => "Send? Type yes to send: ",
    };
    if term.ask(question) != "yes" {
        term.say("Nothing was sent.");
        return ExitCode::SUCCESS;
    }
    term.say("Sending...");
    match send(&prepared.archive, &prepared.summary_header) {
        Ok(id) => {
            term.say(&format!(
                "Sent. The report's ID is {id}. Mention it wherever you talk about this problem."
            ));
            let link = issue_link(&id, &prepared.summary);
            term.say(&format!(
                "\nIf you'd like to open a GitHub issue too, this link fills one in for you to check first:\n{link}"
            ));
            ExitCode::SUCCESS
        }
        Err(reason) => {
            term.say(&format!("It wasn't sent. {reason}"));
            term.say(&format!(
                "The report is still in {}. You can attach it to an issue at https://github.com/ruxnlabs/guesthouse/issues, or run this again later.",
                saved.display()
            ));
            ExitCode::FAILURE
        }
    }
}

pub fn main() -> ExitCode {
    let options = match read_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(text) if text == HELP => {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(text) => {
            eprintln!("{text}");
            return ExitCode::FAILURE;
        }
    };
    let (Some(support), Some(home)) = (store::support_folder(), std::env::var_os("HOME")) else {
        eprintln!("This Mac has no home folder, so there is nowhere to look.");
        return ExitCode::FAILURE;
    };
    let places = Places { support, home: PathBuf::from(home) };
    run(
        options,
        &places,
        env!("CARGO_PKG_VERSION"),
        &output_folder(),
        &mut RealTerminal,
        &|archive, header| super::send::send(archive, header),
    )
}

#[cfg(test)]
mod tests;
