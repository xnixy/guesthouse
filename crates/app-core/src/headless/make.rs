//! Making a machine, installing it, and taking it away: what the New Machine dialog, Start Over and
//! the machine's Delete do.

use super::args::NewOptions;
use super::{follow_up, run_task};
use crate::card_choice::{default_choice, windows_card_choices};
use crate::context::Context;
use crate::defaults::{NO_GPU, account_name, sizes_for};
use crate::failure::{Failure, Result};
use crate::gpu_support::{kind_offered, plain_reason, ubuntu_note};
use crate::host::{RamField, physical_cores};
use crate::iso::read_iso;
use crate::library::{NewMachine, create, delete, remove_entry};
use crate::machine::{Kind, MIN_RAM_GIB, Machine, load};
use crate::setup::checks::gpu_warning;
use crate::tasks::{State, status};
use std::io::Write;

fn kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::Windows => "Windows with GPU-PV",
        Kind::Plain => "a plain VM",
        Kind::Linux => "Ubuntu",
    }
}

fn read_kind(word: &str) -> Result<Kind> {
    match word.to_lowercase().as_str() {
        "windows" => Ok(Kind::Windows),
        "plain" => Ok(Kind::Plain),
        "ubuntu" => Ok(Kind::Linux),
        other => Err(Failure(format!("The kind is windows, ubuntu or plain, not {other}."))),
    }
}

fn whole_number(option: &str, text: &str) -> Result<u32> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    text.parse().ok().filter(|_| digits).ok_or_else(|| Failure(format!("{option} must be a whole number.")))
}

/// What the disc becomes here, and what the dialog says of it.
fn kind_of_disc(context: &Context, path: &str, wanted: Option<&str>, out: &mut dyn Write) -> Result<Kind> {
    let found = read_iso(path)?;
    let kind = kind_offered(context, found.kind);
    let reason = plain_reason(context, found.kind);
    let verdict = match kind {
        Kind::Windows => format!("{}: {}.", found.title, kind_text(kind)),
        Kind::Linux => format!("{}: {} {}", found.title, kind_text(kind), ubuntu_note(context)),
        Kind::Plain => {
            format!("{}: {}. You install it by hand. {NO_GPU} {reason}", found.title, kind_text(kind))
        }
    };
    writeln!(out, "{}", verdict.trim())?;
    if let Some(word) = wanted {
        let asked = read_kind(word)?;
        if asked != kind {
            return Err(Failure(format!(
                "This disc makes {} here, and not {}.",
                kind_text(kind),
                kind_text(asked)
            )));
        }
    }
    Ok(kind)
}

/// The card a Windows machine runs on, as the dialog would choose it unless one was named. Empty is
/// the NVIDIA card.
fn card_for(context: &Context, named: Option<&str>) -> Result<String> {
    let choices = windows_card_choices(context);
    let Some(pci) = named else {
        return Ok(choices
            .get(default_choice(context, &choices))
            .map(|choice| choice.pci.clone())
            .unwrap_or_default());
    };
    if choices.iter().any(|choice| !choice.pci.is_empty() && choice.pci == pci) {
        return Ok(pci.to_owned());
    }
    let cards: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
    Err(Failure(format!("There is no card at {pci} to use. The cards are {}.", cards.join(", "))))
}

pub(super) fn new_machine(
    context: &Context,
    name: &str,
    options: &NewOptions,
    out: &mut dyn Write,
) -> Result<()> {
    let iso = options.iso.ok_or_else(|| Failure::new("new needs --iso PATH, the disc to install from."))?;
    let kind = kind_of_disc(context, iso, options.kind, out)?;
    let windows = kind == Kind::Windows;
    if kind == Kind::Plain && options.user.is_some() {
        return Err(Failure::new("A plain VM has no account to name. It is installed by hand."));
    }
    if !windows && options.amd_card.is_some() {
        return Err(Failure::new("Only a Windows machine chooses its card here."));
    }
    let sizes = sizes_for(kind);
    let field = if windows {
        RamField::for_gpu(context, context.slot, MIN_RAM_GIB.into(), sizes.ram)
    } else {
        RamField::for_plain(context, 1, sizes.ram)
    };
    let number = |option: &str, text: Option<&str>, usual: u32| match text {
        Some(text) => whole_number(option, text),
        None => Ok(usual),
    };
    let new = NewMachine {
        name: name.to_owned(),
        iso: iso.to_owned(),
        kind,
        cores: number("--cores", options.cores, sizes.cores.min(physical_cores(&context.cpus)))?,
        ram_gib: number("--ram", options.ram, field.value as u32)?,
        disk_gib: number("--disk", options.disk, sizes.disk)?,
        user: account_name(kind, options.user.unwrap_or_default().trim()),
        password: String::new(),
        amd_gpu: windows.then(|| card_for(context, options.amd_card)).transpose()?,
    };
    if new.name.trim().is_empty() {
        return Err(Failure::new("The machine needs a name."));
    }
    let machine = create(context, &new)?;
    writeln!(out, "Made {} in {}.", machine.name, machine.folder(context).display())?;
    if machine.installing {
        let who = if windows { "Windows" } else { "Ubuntu" };
        writeln!(out, "{who} installs by itself. Run: guesthouse-vm install {}", machine.id)?;
    } else {
        writeln!(out, "Start it with: guesthouse-vm start {}", machine.id)?;
    }
    Ok(())
}

pub(super) fn install(context: &Context, machine: &Machine, wait: bool, out: &mut dyn Write) -> Result<()> {
    let name = &machine.name;
    if !machine.installing {
        return Err(Failure(format!("{name} has no install to run.")));
    }
    let before = status(context, machine);
    if before.state != State::Off {
        return Err(Failure(format!("{name} is {}.", before.state.name().to_lowercase())));
    }
    if machine.kind == Kind::Windows
        && let Some(warning) = gpu_warning(context)
    {
        writeln!(out, "Note: {warning}")?;
    }
    let Some(found) = run_task(context, machine, "install", wait, out)? else {
        return Ok(writeln!(out, "Installing {name}. status shows how far it is.")?);
    };
    if load(context, &machine.id)?.installing {
        let log = context.paths.log_file();
        return Err(follow_up(
            &found,
            format!("The install of {name} did not finish. The log has more: {}", log.display()),
        ));
    }
    if !found.error.is_empty() {
        writeln!(out, "Note: {}", found.error)?;
    }
    Ok(writeln!(out, "{name} is installed.")?)
}

pub(super) fn remove(context: &Context, machine: &Machine, yes: bool, out: &mut dyn Write) -> Result<()> {
    let owned = machine.owns_files(context);
    let what = if owned {
        format!(
            "This deletes {}, its disk and everything on it, in {}. It cannot be undone.",
            machine.name,
            machine.folder(context).display()
        )
    } else {
        format!(
            "Only the entry of {} goes. Its disk stays at {} and is not touched.",
            machine.name, machine.disk
        )
    };
    if !yes {
        return Err(Failure(format!("{what} Run it again with --yes to go on.")));
    }
    if owned { delete(context, machine) } else { remove_entry(context, machine) }?;
    Ok(writeln!(out, "Removed {}.", machine.name)?)
}
