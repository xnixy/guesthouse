//! The VM and the daemon as processes on this PC.

use super::{Stopped, VmHost};
use crate::context::Context;
use crate::daemon::{self, DaemonStart, detached, is_own_daemon};
use crate::failure::{Failure, Result};
use crate::guest_memory::GuestMemory;
use crate::host::GIB;
use crate::jailing;
use crate::machine::{Machine, sorted_names};
use crate::monitor::Command as Qmp;
use crate::processes::{memory_taken, passt_pid, qemu_pid};
use crate::start_checks::SECOND_INSTANCE_REFUSAL;
use crate::vm_args::{OUTPUT_FILE, PASST_PID_FILE, Sound, VmPlan};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use std::fs::{self, File};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const LAST_WORDS_BYTES: usize = 300;
/// QEMU's words when its D-Bus display isn't installed, which is a package of its own.
const NO_DBUS_DISPLAY: &str = "Display 'dbus' is not available";
const NO_DBUS_DISPLAY_HELP: &str = "This QEMU can't show its screen in Guesthouse. Install its D-Bus \
                                    display, which Fedora calls qemu-ui-dbus, and start the machine again.";
const NO_QEMU_HELP: &str = "QEMU isn't installed. Install qemu-system-x86_64, and start the machine again.";
const STOP_POLL: Duration = Duration::from_millis(500);
const MONITOR_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(200);
const AFTER_SIGTERM_SECONDS: u64 = 20;
const AFTER_SIGKILL_SECONDS: u64 = 10;
const LEFT_OVER_STOP_SECONDS: u64 = 100;
const AGENT_ATTEMPTS: u32 = 120;
const AGENT_ANSWER_SECONDS: u64 = 4;
const AGENT_RETRY: Duration = Duration::from_secs(3);
const DAEMON_STOP_ATTEMPTS: u32 = 100;
const DAEMON_STOP_POLL: Duration = Duration::from_millis(100);
const SOUND_CHECK_SECONDS: &str = "3";
/// What versions before the memfds left in shared memory, which goes once nothing holds it.
const OLD_SHARED_FILES: [&str; 3] = ["iomem", "ram", "display"];
/// These can only be left by an earlier run, and a stale socket would pass for a ready daemon.
const STALE_FILES: [&str; 4] = ["vgpu.sock", "global.sock", "screen.sock", PASST_PID_FILE];
/// The last run's logs are the only evidence of whatever made someone restart.
const LOGS_TO_KEEP: [&str; 3] = [daemon::OUTPUT_FILE, "serial.log", "qemu.log"];
const TPM_SOCKET: &str = "tpm.sock";
/// A new VM hasn't booted yet, whatever an earlier one noted.
const BOOTS_FILE: &str = "guesthouse-boots";
const STOPPED_LATE: &str = "The VM would not stop. Try Force Off.";
const AGENT_UP_SCRIPT: &str = "\"up\"";

/// The VM and the daemon as processes on this PC.
pub struct LocalHost<'a> {
    pub context: &'a Context,
}

impl LocalHost<'_> {
    fn run_file(&self, name: &str) -> std::path::PathBuf {
        self.context.run_folder(None).join(name)
    }

    fn wait_gone(&self, seconds: u64) -> bool {
        let clock = &self.context.clock;
        let end = clock.now() + Duration::from_secs(seconds);
        while qemu_pid(self.context, None).is_some() && clock.now() < end {
            clock.sleep(STOP_POLL);
        }
        qemu_pid(self.context, None).is_none()
    }

    /// passt quits once QEMU has gone, as QEMU was its one client. This is for one that hasn't yet,
    /// or one an earlier run left.
    fn end_passt(&self) {
        let Some(pid) = passt_pid(self.context, None) else { return };
        terminate(Some(pid));
        let clock = &self.context.clock;
        let end = clock.now() + Duration::from_secs(AFTER_SIGKILL_SECONDS);
        while passt_pid(self.context, None).is_some() && clock.now() < end {
            clock.sleep(POLL);
        }
    }

    /// The end of QEMU's output, which says why it failed to start.
    fn last_words(&self) -> String {
        last_words(self.context)
    }

    fn spawn_tpm(&self, plan: &VmPlan) -> Result<()> {
        let (Some(arguments), Some(state)) = (plan.swtpm_arguments(), plan.tpm.as_ref()) else {
            return Ok(());
        };
        // A TPM ends when QEMU lets go of it, unless a QEMU never came.
        let socket = self.run_file(TPM_SOCKET);
        let pattern = format!("^swtpm socket .*path={}", socket.display());
        let _ = Command::new("pkill").args(["-f", &pattern]).status();
        let _ = fs::remove_file(&socket);
        fs::create_dir_all(state)?;
        let done = Command::new("swtpm").args(arguments).status()?;
        if !done.success() {
            return Err(Failure::new("The virtual TPM did not start."));
        }
        Ok(())
    }

    fn spawn_qemu(&self, plan: &VmPlan, memory: Option<&GuestMemory>) -> Result<Child> {
        self.spawn_tpm(plan)?;
        let output = File::create(self.run_file(OUTPUT_FILE))?;
        let command = plan.qemu_command();
        let (program, arguments) = command.split_first().ok_or_else(|| Failure::new("Nothing to run."))?;
        let mut qemu = Command::new(program);
        if plan.clears_environment() {
            qemu.env_clear();
        }
        if let Some(memory) = memory {
            memory.hand_to(&mut qemu);
        }
        let spawned = detached(qemu.args(arguments))
            .envs(plan.qemu_environment())
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?;
        Ok(spawned)
    }

    /// QEMU with no daemon and no shared memory. Returns once its monitor answers.
    fn start_plain(&self, plan: &VmPlan) -> Result<()> {
        let _ = fs::remove_file(self.run_file(BOOTS_FILE));
        // Whatever an earlier machine in this slot had to say about its jail isn't this one's, and
        // its passt, if one is somehow still there, goes before this one's is noted.
        jailing::write_note(&plan.run_folder, &plan.jail_note)?;
        self.end_passt();
        let _ = fs::remove_file(self.run_file(PASST_PID_FILE));
        if !plan.qemu.exists() {
            return Err(Failure::new(NO_QEMU_HELP));
        }
        let mut qemu = self.spawn_qemu(plan, None)?;
        let answered = self.wait_for_monitor(&mut qemu);
        if answered.is_err() {
            self.end_qemu(&mut qemu);
        }
        answered
    }

    fn wait_for_monitor(&self, qemu: &mut Child) -> Result<()> {
        let clock = &self.context.clock;
        let end = clock.now() + MONITOR_WAIT;
        while clock.now() < end {
            if qemu.try_wait()?.is_some() {
                return Err(Failure(start_failure(&self.last_words())));
            }
            if self.context.run_state(None).is_some() {
                return Ok(());
            }
            clock.sleep(POLL);
        }
        Err(Failure::new("QEMU started, but its monitor never answered."))
    }

    /// Ends a QEMU this task started and has given up on. It leads a process group of its own, so
    /// whatever launched it goes too.
    fn end_qemu(&self, qemu: &mut Child) {
        let Some(group) = Pid::from_raw(qemu.id() as i32) else { return };
        let clock = &self.context.clock;
        for (signal, seconds) in
            [(Signal::TERM, AFTER_SIGTERM_SECONDS), (Signal::KILL, AFTER_SIGKILL_SECONDS)]
        {
            if !matches!(qemu.try_wait(), Ok(None)) {
                return;
            }
            let _ = kill_process_group(group, signal);
            let end = clock.now() + Duration::from_secs(seconds);
            while matches!(qemu.try_wait(), Ok(None)) && clock.now() < end {
                clock.sleep(POLL);
            }
        }
    }

    fn clear_run_folder(&self) -> Result<()> {
        for name in STALE_FILES {
            let _ = fs::remove_file(self.run_file(name));
        }
        for name in LOGS_TO_KEEP {
            let log = self.run_file(name);
            if log.is_file() {
                fs::rename(&log, self.run_file(&format!("{name}.1")))?;
            }
        }
        File::create(self.run_file("serial.log"))?;
        Ok(())
    }

    fn remove_old_shared_files(&self) {
        for name in OLD_SHARED_FILES {
            let _ = fs::remove_file(self.context.paths.shared_file(name, self.context.slot));
        }
    }

    fn wait_for_daemon(&self, daemon: &mut Child) -> Result<()> {
        let ready = || daemon::SOCKETS.iter().all(|name| self.run_file(name).exists());
        while !ready() {
            if daemon.try_wait()?.is_some() {
                return Err(Failure(format!("The VM did not start: {}", self.daemons_last_words())));
            }
            self.context.clock.sleep(POLL);
        }
        Ok(())
    }

    fn daemons_last_words(&self) -> String {
        let said = fs::read_to_string(self.run_file(daemon::OUTPUT_FILE)).unwrap_or_default();
        let last = said.lines().last().unwrap_or("it gave no reason");
        format!("the daemon stopped: {last}")
    }

    /// Asks the processes themselves, and not `/proc`. Just after a spawn returns, `/proc` can show a
    /// new QEMU with no words at all, and on a busy PC that lasts long enough to look like it's gone.
    fn wait_for_agent(&self, machine: &Machine, qemu: &mut Child, daemon: &mut Child) -> Result<()> {
        for _ in 0..AGENT_ATTEMPTS {
            if self.context.ask_guest(machine, AGENT_UP_SCRIPT, AGENT_ANSWER_SECONDS, None).contains("up") {
                return Ok(());
            }
            if qemu.try_wait()?.is_some() {
                let words = self.last_words();
                return Err(Failure(format!("The VM stopped unexpectedly. {words}").trim().to_owned()));
            }
            if daemon.try_wait()?.is_some() {
                return Err(Failure(format!("The VM did not start: {}", self.daemons_last_words())));
            }
            self.context.clock.sleep(AGENT_RETRY);
        }
        Err(Failure::new("The VM did not start: the agent never answered."))
    }

    fn daemons(&self) -> Vec<u32> {
        let context = self.context;
        let pids = sorted_names(&context.processes);
        let own = pids.iter().filter(|pid| is_own_daemon(context, pid, context.slot));
        own.filter_map(|pid| pid.parse().ok()).collect()
    }

    /// The VM with its GPU: the daemon must be up before QEMU, and anything left of an earlier run
    /// goes first.
    fn start_gpu(&self, plan: &VmPlan, machine: &Machine) -> Result<()> {
        let sound = if machine.sound { sound_on_this_host() } else { None };
        let plan = VmPlan { sound, ..plan.clone() };
        let choice = jailing::choose(self.context, machine, &plan, &(self.context.jail_offer)(&plan))?;
        let jail = choice.qemu.clone().map(|jail| plan.with_backing_files(jail)).transpose()?;
        let plan = VmPlan { jail, jail_note: choice.note.clone(), ..plan };
        self.stop(machine, LEFT_OVER_STOP_SECONDS)?;
        if qemu_pid(self.context, None).is_some()
            || !self.daemons().is_empty()
            || memory_taken(self.context, None)
        {
            return Err(Failure::new("The VM did not start: something is still running."));
        }
        self.clear_run_folder()?;
        self.remove_old_shared_files();
        // The app's own copies close when this returns, and the memory goes back once both have gone.
        let memory =
            (self.context.reserve_memory)(self.context, plan.slot, u64::from(machine.ram_gib) * GIB)?;
        jailing::write_note(&plan.run_folder, &plan.jail_note)?;
        let mut start = DaemonStart::new(self.context, plan.slot, &plan.ram, plan.vram.as_deref());
        if !choice.daemon_jailed {
            start = start.without_jail();
        }
        if let Some(maker) = machine.windows_maker() {
            start = start.on_card(maker, &machine.amd_gpu)?;
        }
        let mut daemon = start.spawn(&plan.run_folder, &memory)?;
        self.wait_for_daemon(&mut daemon)?;
        let mut qemu = self.spawn_qemu(&plan, Some(&memory))?;
        let answered = self.wait_for_agent(machine, &mut qemu, &mut daemon);
        if answered.is_err() {
            self.end_qemu(&mut qemu);
        }
        answered
    }
}

impl VmHost for LocalHost<'_> {
    fn start(&self, plan: &VmPlan, machine: &Machine) -> Result<()> {
        self.context.make_run_folder(Some(plan.slot))?;
        if !plan.is_gpu() {
            return self.start_plain(plan);
        }
        if self.context.second_instance() {
            return Err(Failure::new(SECOND_INSTANCE_REFUSAL));
        }
        self.start_gpu(plan, machine)
    }

    /// With no daemon there's no agent to rely on, so the power button goes first.
    fn stop(&self, machine: &Machine, seconds: u64) -> Result<Stopped> {
        if machine.has_gpu() && self.context.second_instance() {
            return Err(Failure::new(SECOND_INSTANCE_REFUSAL));
        }
        let mut how = Stopped::Cleanly;
        if qemu_pid(self.context, None).is_some() {
            if machine.has_gpu() {
                self.context.ask_guest(machine, "Stop-Computer -Force", crate::agent::QUICK_SECONDS, None);
            } else {
                self.context.ask_monitor(None, &Qmp::bare("system_powerdown"));
            }
            if !self.wait_gone(seconds) {
                terminate(qemu_pid(self.context, None));
                if !self.wait_gone(AFTER_SIGTERM_SECONDS) {
                    return Err(Failure::new(STOPPED_LATE));
                }
                how = Stopped::TurnedOff;
            }
        }
        self.end_passt();
        if machine.has_gpu() {
            self.release()?;
        }
        Ok(how)
    }

    fn release(&self) -> Result<()> {
        if self.context.second_instance() {
            return Err(Failure::new(SECOND_INSTANCE_REFUSAL));
        }
        if qemu_pid(self.context, None).is_none() {
            self.end_passt();
        }
        for pid in self.daemons() {
            terminate(Some(pid));
        }
        for _ in 0..DAEMON_STOP_ATTEMPTS {
            if self.daemons().is_empty() {
                break;
            }
            self.context.clock.sleep(DAEMON_STOP_POLL);
        }
        if !memory_taken(self.context, None) {
            self.remove_old_shared_files();
        }
        Ok(())
    }
}

fn terminate(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| Pid::from_raw(pid as i32)) {
        let _ = kill_process(pid, Signal::TERM);
    }
}

/// QEMU won't start if it can't reach PipeWire, as from a session with no sound server. Only GPU
/// machines get sound, as they always have.
fn sound_on_this_host() -> Option<Sound> {
    let reachable = Command::new("timeout")
        .args([SOUND_CHECK_SECONDS, "pw-cli", "info", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    reachable
        .is_ok_and(|status| status.success())
        .then(|| Sound { sink: std::env::var("SOUND_SINK").ok().filter(|sink| !sink.is_empty()) })
}

/// The end of QEMU's output, which says why it failed to start.
/// What to tell the user when QEMU stops at once. Most of what QEMU says is clear as it is.
pub(super) fn start_failure(said: &str) -> String {
    if said.contains(NO_DBUS_DISPLAY) {
        return NO_DBUS_DISPLAY_HELP.to_owned();
    }
    format!("The VM did not start. {said}").trim().to_owned()
}

pub fn last_words(context: &Context) -> String {
    let path = context.run_folder(None).join(OUTPUT_FILE);
    let text = fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&text);
    let text = text.trim();
    let start = text.char_indices().rev().nth(LAST_WORDS_BYTES - 1).map_or(0, |found| found.0);
    text[start..].to_owned()
}
