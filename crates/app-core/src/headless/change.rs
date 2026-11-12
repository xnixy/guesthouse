//! A machine's settings, changed by the calls the settings pages make. A page greys out what a running
//! machine can't have changed, and here the same is refused in words.

use super::args::Forwarding;
use super::run_task;
use crate::context::Context;
use crate::display::{forget_note, read_state};
use crate::failure::{Failure, Result};
use crate::limits::{cores_range, ram_field};
use crate::machine::{Kind, Machine, SCREENS, read_refresh};
use crate::network::{Forward, Mode, Protocol, read_port};
use crate::network_settings::{add_forward, read_forward, remove_forward, set_bridge, set_network};
use crate::settings::max_vram_gib;
use crate::start_checks::share_problem;
use crate::tasks::{State, status};
use crate::vm_args::passt_installed;
use std::fmt::Display;
use std::io::Write;

const MUST_BE_OFF: &str = "The VM must be off to change this.";
const SETTINGS: [&str; 10] =
    ["cores", "ram", "vram", "screen", "refresh", "network", "passt", "bridge", "sound", "share"];
const FIXED_NETWORK: &str = "An Ubuntu machine's network is fixed, with its SSH port forwarded.";

fn whole_number(key: &str, text: &str) -> Result<u32> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    text.parse().ok().filter(|_| digits).ok_or_else(|| Failure(format!("{key} must be a whole number.")))
}

fn on_or_off(key: &str, text: &str) -> Result<bool> {
    match text.to_lowercase().as_str() {
        "on" | "yes" | "true" => Ok(true),
        "off" | "no" | "false" => Ok(false),
        _ => Err(Failure(format!("{key} is on or off."))),
    }
}

fn only_for_windows(machine: &Machine, key: &str) -> Result<()> {
    match machine.kind {
        Kind::Windows => Ok(()),
        _ => Err(Failure(format!("{key} is only for a Windows machine with GPU-PV."))),
    }
}

fn not_for_ubuntu(machine: &Machine) -> Result<()> {
    match machine.kind {
        Kind::Linux => Err(Failure::new(FIXED_NETWORK)),
        _ => Ok(()),
    }
}

fn must_be_off(context: &Context, machine: &Machine) -> Result<()> {
    let found = status(context, machine);
    if found.state == State::Off && !found.busy { Ok(()) } else { Err(Failure::new(MUST_BE_OFF)) }
}

fn in_range(what: &str, wanted: u32, least: u64, most: u64) -> Result<()> {
    if (least..=most).contains(&u64::from(wanted)) {
        Ok(())
    } else {
        Err(Failure(format!("{what} must be from {least} to {most} on this PC.")))
    }
}

/// A screen or rate that changed reaches Windows at once if it can take it, as the display page does.
fn apply_display(context: &Context, machine: &Machine, out: &mut dyn Write) -> Result<()> {
    forget_note(context, machine)?;
    let found = status(context, machine);
    let on_driver = read_state(context, machine).running.is_some_and(|screen| screen.on_driver);
    if found.state == State::Running && !found.busy && on_driver {
        run_task(context, machine, "apply_display", true, out)?;
    }
    Ok(())
}

fn said(out: &mut dyn Write, key: &str, value: &str) -> Result<()> {
    Ok(writeln!(out, "{key} is now {value}.")?)
}

pub(super) fn set(
    context: &Context,
    mut machine: Machine,
    key: &str,
    value: &str,
    out: &mut dyn Write,
) -> Result<()> {
    match key {
        "cores" => {
            must_be_off(context, &machine)?;
            let (least, most) = cores_range(context, &machine);
            let cores = whole_number(key, value)?;
            in_range("Cores", cores, least.into(), most.into())?;
            machine.cores = cores;
        }
        "ram" => {
            must_be_off(context, &machine)?;
            let field = ram_field(context, &machine);
            let ram = whole_number(key, value)?;
            in_range("RAM in GiB", ram, field.lower, field.upper.max(machine.ram_gib.into()))?;
            machine.ram_gib = ram;
        }
        "vram" => {
            only_for_windows(&machine, key)?;
            must_be_off(context, &machine)?;
            let cap = whole_number(key, value)?;
            in_range("The video memory cap in GiB", cap, 0, max_vram_gib(context).into())?;
            machine.vram_gib = cap;
        }
        "sound" => {
            only_for_windows(&machine, key)?;
            must_be_off(context, &machine)?;
            machine.sound = on_or_off(key, value)?;
        }
        "screen" => {
            only_for_windows(&machine, key)?;
            if !SCREENS.contains(&value) {
                return Err(Failure(format!("The screen size must be one of {}.", SCREENS.join(", "))));
            }
            value.clone_into(&mut machine.screen);
        }
        "refresh" => {
            only_for_windows(&machine, key)?;
            machine.refresh_hz = read_refresh(value)?;
        }
        "network" => {
            not_for_ubuntu(&machine)?;
            must_be_off(context, &machine)?;
            let mode = Mode::ALL.iter().find(|mode| mode.name().eq_ignore_ascii_case(value));
            let mode = mode.ok_or_else(|| Failure::new("The network is nat, none or bridged."))?;
            set_network(context, &mut machine, *mode)?;
            return said(out, key, mode.name());
        }
        "passt" => {
            not_for_ubuntu(&machine)?;
            must_be_off(context, &machine)?;
            if machine.network_mode != Mode::Nat {
                return Err(Failure::new("passt is for the NAT network."));
            }
            let wanted = on_or_off(key, value)?;
            if wanted && !passt_installed() {
                return Err(Failure::new("passt is not installed."));
            }
            machine.passt = wanted;
        }
        "bridge" => {
            not_for_ubuntu(&machine)?;
            must_be_off(context, &machine)?;
            set_bridge(context, &mut machine, value)?;
            return said(out, key, value);
        }
        "share" => {
            not_for_ubuntu(&machine)?;
            must_be_off(context, &machine)?;
            let folder = if value.eq_ignore_ascii_case("none") { "" } else { value };
            if let Some(problem) = share_problem(context, folder) {
                return Err(Failure(problem));
            }
            folder.clone_into(&mut machine.share);
        }
        other => {
            let known = SETTINGS.join(", ");
            return Err(Failure(format!("There is no setting called {other}. They are {known}.")));
        }
    }
    machine.save(context)?;
    if matches!(key, "screen" | "refresh") {
        apply_display(context, &machine, out)?;
    }
    said(out, key, value)
}

fn protocol_of(word: &str) -> Result<Protocol> {
    let found = Protocol::ALL.iter().find(|protocol| protocol.word().eq_ignore_ascii_case(word));
    found.copied().ok_or_else(|| Failure::new("The protocol is tcp or udp."))
}

pub(super) fn forward(
    context: &Context,
    mut machine: Machine,
    change: &Forwarding,
    out: &mut dyn Write,
) -> Result<()> {
    not_for_ubuntu(&machine)?;
    must_be_off(context, &machine)?;
    match change {
        Forwarding::Add { protocol, host, guest } => {
            let item = read_forward(context, &machine, protocol_of(protocol)?, host, guest)?;
            add_forward(context, &mut machine, item)?;
            Ok(writeln!(out, "Forwarded {}.", item.text())?)
        }
        Forwarding::Remove { protocol, host, guest } => {
            let (Some(host), Some(guest)) = (read_port(host), read_port(guest)) else {
                return Err(Failure::new("Ports must be whole numbers from 1 to 65535."));
            };
            let item = Forward::new(protocol_of(protocol)?, host, guest);
            if !machine.forwards.contains(&item) {
                return Err(Failure(format!("{} is not forwarded.", item.text())));
            }
            remove_forward(context, &mut machine, &item)?;
            Ok(writeln!(out, "Removed the forward {}.", item.text())?)
        }
    }
}

fn line(out: &mut dyn Write, key: &str, value: &dyn Display) -> Result<()> {
    Ok(writeln!(out, "{key:<9} {value}")?)
}

fn switch(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

pub(super) fn show(context: &Context, machine: &Machine, out: &mut dyn Write) -> Result<()> {
    line(out, "name", &machine.name)?;
    line(out, "id", &machine.id)?;
    line(out, "folder", &machine.folder(context).display())?;
    line(out, "cores", &machine.cores)?;
    line(out, "ram", &format!("{} GiB", machine.ram_gib))?;
    if machine.kind == Kind::Windows {
        let cap =
            if machine.vram_gib == 0 { "no cap".to_owned() } else { format!("{} GiB", machine.vram_gib) };
        line(out, "vram", &cap)?;
        line(out, "screen", &machine.screen)?;
        line(out, "refresh", &format!("{} Hz", machine.refresh_hz))?;
        line(out, "sound", &switch(machine.sound))?;
    }
    if machine.kind != Kind::Linux {
        line(out, "network", &machine.network_mode.name())?;
        if machine.network_mode == Mode::Nat {
            line(out, "passt", &switch(machine.passt))?;
        }
        if machine.network_mode == Mode::Bridged {
            line(out, "bridge", &machine.bridge)?;
        }
        let share = if machine.share.is_empty() { "none" } else { &machine.share };
        line(out, "share", &share)?;
        let forwards: Vec<String> = machine.forwards.iter().map(Forward::text).collect();
        line(out, "forwards", &if forwards.is_empty() { "none".to_owned() } else { forwards.join(", ") })?;
    }
    if !machine.iso.is_empty() {
        line(out, "iso", &machine.iso)?;
    }
    if machine.installing {
        writeln!(out, "Its install has not finished.")?;
    }
    Ok(())
}
