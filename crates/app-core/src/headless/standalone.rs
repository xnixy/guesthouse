//! `command`: what would run to start a machine beside someone's own tools, written so that it can be
//! pasted into a script. It starts nothing and changes nothing. The words are the ones a start from the
//! app runs, from the same plan: see [`crate::vm_args`].

use crate::actions::plan_for;
use crate::context::Context;
use crate::daemon::{self, DaemonStart};
use crate::failure::{Failure, Result};
use crate::machine::{Kind, Machine};
use crate::network::Mode;
use crate::setup::checks::gpu_warning;
use crate::start_checks::start_problems;
use crate::text::shell_quote;
use crate::vm_args::{Boot, VmPlan, memory_files_in};
use std::io::Write;
use std::path::Path;

/// The shell's name for the daemon between the steps that start and stop it.
const DAEMON_PID: &str = "daemon";
const SOCKET_WAIT: &str = "0.2";

/// The plan for a machine as it would run beside the user's own tools, or why it can't.
fn plan(context: &Context, machine: &Machine) -> Result<VmPlan> {
    let name = &machine.name;
    let boot = match machine.kind {
        Kind::Windows if machine.installing => {
            return Err(Failure(format!(
                "{name} isn't installed yet. Install it with Guesthouse first: install {name}."
            )));
        }
        Kind::Windows => Boot::Gpu,
        Kind::Plain if !machine.own_linux_gpu() => Boot::Plain { iso: machine.iso.clone() },
        Kind::Plain | Kind::Linux => {
            return Err(Failure(format!(
                "{name} is a Linux machine with a GPU, and command is for Windows machines for now. \
                 docs/standalone.md has a short part on Linux guests."
            )));
        }
    };
    let plan = plan_for(context, machine, boot);
    if !plan.is_gpu() {
        return Ok(VmPlan { drives_by_path: Some(context.drive_ids.clone()), ..plan });
    }
    let files = memory_files_in(&context.paths.shared_memory, plan.slot);
    Ok(plan.standalone(files, &context.drive_ids))
}

/// A line to be read as a command, with everything but the last continued, and the arguments each
/// starting a line of their own, with their values.
fn continued(environment: &[(String, String)], head: &[String], arguments: &[String]) -> Vec<String> {
    let mut lines: Vec<String> =
        environment.iter().map(|(name, value)| format!("{name}={}", shell_quote(value))).collect();
    lines.push(head.iter().map(|word| shell_quote(word)).collect::<Vec<_>>().join(" "));
    let mut grouped = false;
    for word in arguments {
        let quoted = shell_quote(word);
        match lines.last_mut() {
            Some(line) if grouped && !word.starts_with('-') => {
                line.push(' ');
                line.push_str(&quoted);
            }
            _ => lines.push(quoted),
        }
        grouped = true;
    }
    let last = lines.len() - 1;
    for (at, line) in lines.iter_mut().enumerate() {
        if at != 0 {
            line.insert_str(0, "  ");
        }
        if at != last {
            line.push_str(" \\");
        }
    }
    lines
}

fn show(out: &mut dyn Write, lines: &[String]) -> Result<()> {
    lines.iter().try_for_each(|line| writeln!(out, "{line}")).map_err(Failure::from)
}

fn path_word(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

/// The daemon, started in the background and waited for, in the order the shell needs.
fn daemon_steps(context: &Context, plan: &VmPlan, machine: &Machine) -> (Vec<String>, Vec<String>) {
    let mut start = DaemonStart::new(context, plan.slot, &plan.ram, plan.vram.as_deref());
    start.environment.insert("GUESTHOUSE_STORE".to_owned(), context.paths.store.display().to_string());
    if let Some(files) = &plan.memory_by_path {
        start = start.with_memory_files(files);
    }
    if let Some(pci) = machine.windows_amd_card() {
        start = start.on_amd_card(pci);
    }
    let environment: Vec<_> = start.environment.into_iter().collect();
    let command = [start.program.to_string_lossy().into_owned(), start.base.to_string_lossy().into_owned()];
    let mut starting = continued(&environment, &command, &[]);
    let log = path_word(&plan.run_folder.join(daemon::OUTPUT_FILE));
    let last = starting.len() - 1;
    starting[last] += &format!(" > {log} 2>&1 &");
    starting.push(format!("{DAEMON_PID}=$!"));
    let sockets: Vec<String> = daemon::SOCKETS
        .iter()
        .map(|name| format!("[ -S {} ]", path_word(&plan.run_folder.join(name))))
        .collect();
    let waiting = vec![format!(
        "until {}; do kill -0 \"${DAEMON_PID}\" 2>/dev/null || exit 1; sleep {SOCKET_WAIT}; done",
        sockets.join(" && ")
    )];
    (starting, waiting)
}

fn qemu_steps(plan: &VmPlan) -> Vec<String> {
    let words = plan.qemu_command();
    let program = plan.qemu.to_string_lossy();
    let program_at = words.iter().position(|word| *word == program).unwrap_or(0);
    let (head, arguments) = words.split_at(program_at + 1);
    continued(&plan.qemu_environment(), head, arguments)
}

/// Why the machine may not start on this PC, said as notes. The command still prints.
fn notes(context: &Context, machine: &Machine) -> Vec<String> {
    let mut found = Vec::new();
    if machine.kind == Kind::Windows {
        found.extend(gpu_warning(context));
    }
    match start_problems(context, machine) {
        Ok(problems) => found.extend(problems),
        Err(failure) => found.push(failure.0),
    }
    found.into_iter().map(|note| format!("# Note: {note}")).collect()
}

pub(super) fn command(context: &Context, machine: &Machine, out: &mut dyn Write) -> Result<()> {
    let plan = plan(context, machine)?;
    let run_folder = path_word(&plan.run_folder);
    writeln!(out, "# {} ({}), as it would start beside your own tools.", machine.name, machine.id)?;
    writeln!(out, "# This starts nothing. Read it, and put in a script what you want.")?;
    show(out, &notes(context, machine))?;
    writeln!(out, "\n# The run folder holds the sockets, and what an earlier run left in it can't stay.")?;
    writeln!(out, "mkdir -p {run_folder}")?;
    let sockets = daemon::SOCKETS.iter().map(|name| path_word(&plan.run_folder.join(name)));
    writeln!(out, "rm -f {}", sockets.collect::<Vec<_>>().join(" "))?;
    if let Some(swtpm) = plan.swtpm_arguments() {
        writeln!(out, "\n# The virtual TPM. It ends when QEMU lets go of it.")?;
        writeln!(out, "mkdir -p {}", path_word(plan.tpm.as_deref().unwrap_or(&plan.run_folder)))?;
        show(out, &continued(&[], &["swtpm".to_owned(), swtpm[0].clone()], &swtpm[1..]))?;
    }
    if let Some(files) = &plan.memory_by_path {
        let (starting, waiting) = daemon_steps(context, &plan, machine);
        writeln!(out, "\n# The daemon. It makes the guest's memory, in these two files:")?;
        writeln!(out, "# {}\n# {}", files.ram.display(), files.window.display())?;
        show(out, &starting)?;
        writeln!(out, "\n# QEMU connects to its two sockets, so they have to be there first.")?;
        show(out, &waiting)?;
    }
    writeln!(out, "\n# QEMU.")?;
    if plan.network.passt {
        writeln!(out, "# It starts passt for this machine's network, so passt has to be installed.")?;
    }
    if plan.network.mode == Mode::Bridged {
        writeln!(out, "# It runs the bridge helper for this machine's network, which has to be allowed to.")?;
    }
    if machine.has_gpu() {
        let (port, token) = (plan.agent_port, machine.token_file(context));
        writeln!(
            out,
            "# Windows' agent answers on 127.0.0.1:{port} to a request with the token in {}.",
            token.display()
        )?;
        writeln!(out, "# Only Guesthouse's app makes requests of it.")?;
    }
    if !plan.drives.is_empty() {
        writeln!(
            out,
            "# The app opens each drive exclusively, so the host can't mount it while the machine runs."
        )?;
        writeln!(out, "# Here nothing stops that: don't mount a drive while QEMU has it.")?;
    }
    show(out, &qemu_steps(&plan))?;
    if let Some(files) = &plan.memory_by_path {
        writeln!(out, "\n# Stop QEMU before the daemon, and free the memory once both have gone.")?;
        writeln!(out, "kill \"${DAEMON_PID}\"; wait \"${DAEMON_PID}\"")?;
        writeln!(out, "rm -f {} {}", path_word(&files.ram), path_word(&files.window))?;
    }
    Ok(())
}
