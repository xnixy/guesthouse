//! Windows' own DirectX diagnostic, run in a running guest through its agent, with its report
//! brought back beside the daemon's log. The tool makes Direct3D devices on every adapter, so on an
//! Intel card it is what makes the guest's driver talk to the daemon. It runs without anyone at the
//! screen, which matters while the card draws nothing and the desktop may not show anything.
//!
//! What a tester of an Intel card sends is gathered in the same step, in a folder of the machine's
//! own: the report, the daemon's log as it is then, and what the host's driver says about the card.

use super::Task;
use super::guest_upload::quoted;
use crate::daemon::OUTPUT_FILE;
use crate::failure::{Failure, Result};
use crate::gpu_maker::GpuMaker;
use crate::text::base64_decode;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Where the guest writes the report, and what it's called beside the daemon's log.
const GUEST_REPORT: &str = r"C:\gpupv\dxdiag.txt";
pub const REPORT_FILE: &str = "dxdiag.txt";
/// The folder in the machine's own folder that a run's files are gathered in, and the file the
/// host's view of the card goes in. The probe is a program of the Intel backend.
const GATHERED_FOLDER: &str = "test-report";
const PROBE_FILE: &str = "intel-probe.txt";
const PROBE_PROGRAM: &str = "intel-probe";
const START_SECONDS: u64 = 20;
const POLL_SECONDS: u64 = 10;
const POLL_EVERY: Duration = Duration::from_secs(5);
const WAIT_MINUTES: u64 = 5;
const FETCH_SECONDS: u64 = 60;
const STARTED_MARK: &str = "dxdiag started";
const DONE_MARK: &str = "dxdiag done";
const REPORT_MARK: &str = "report:";

fn start_script() -> String {
    format!(
        "Remove-Item -LiteralPath {report} -Force -ErrorAction SilentlyContinue; \
         Start-Process -FilePath dxdiag.exe -ArgumentList '/dontskip','/t',{report} -WindowStyle Hidden; \
         '{STARTED_MARK}'",
        report = quoted(GUEST_REPORT)
    )
}

/// The report is written when dxdiag has finished its tests, and the process goes after that.
fn poll_script() -> String {
    format!(
        "if ((Test-Path -LiteralPath {}) -and -not (Get-Process dxdiag -ErrorAction SilentlyContinue)) \
         {{ '{DONE_MARK}' }} else {{ 'waiting' }}",
        quoted(GUEST_REPORT)
    )
}

/// The report as one run of base64, which the agent's line wrapping can't harm, as the reader drops
/// whitespace.
fn fetch_script() -> String {
    format!("'{REPORT_MARK}' + [Convert]::ToBase64String([IO.File]::ReadAllBytes({}))", quoted(GUEST_REPORT))
}

fn report_from(answer: &str) -> Option<Vec<u8>> {
    let encoded: String = answer.split(REPORT_MARK).nth(1)?.split_whitespace().collect();
    base64_decode(&encoded).filter(|data| !data.is_empty())
}

/// What the host's driver says about the machine's Intel card, or why that isn't known.
fn probe_text(task: &Task) -> String {
    let program = task.context.paths.binary(PROBE_PROGRAM);
    match Command::new(&program).arg(&task.machine.amd_gpu).output() {
        Ok(done) => {
            let said = [&done.stdout[..], &done.stderr[..]].concat();
            String::from_utf8_lossy(&said).into_owned()
        }
        Err(error) => format!("{PROBE_PROGRAM} could not be run from {}: {error}\n", program.display()),
    }
}

/// Puts the report, the daemon's log and the probe's answer in one folder, and says where it is.
fn gather(task: &Task) -> Result<PathBuf> {
    let folder = task.machine.folder(task.context).join(GATHERED_FOLDER);
    fs::create_dir_all(&folder)?;
    fs::copy(task.run_file(REPORT_FILE), folder.join(REPORT_FILE))?;
    // A daemon that never started has no log, and the folder says so by lacking it.
    let _ = fs::copy(task.run_file(OUTPUT_FILE), folder.join(OUTPUT_FILE));
    fs::write(folder.join(PROBE_FILE), probe_text(task))?;
    Ok(folder)
}

/// Runs dxdiag in the guest, waits for it, and saves its report in the run folder.
pub(super) fn dxdiag(task: &mut Task) -> Result<()> {
    if task.pid().is_none() {
        return Err(Failure::new("The machine isn't running, so there is no Windows to run dxdiag in."));
    }
    task.step("Running dxdiag in Windows");
    if !task.ask_guest(&start_script(), START_SECONDS).contains(STARTED_MARK) {
        return Err(Failure::new(
            "Windows' agent didn't answer, so dxdiag was not started. If the machine only just started, wait \
             a minute and run this again. If it stays quiet, the guest has stopped, and daemon.out is what to send.",
        ));
    }
    let end = task.now() + Duration::from_secs(WAIT_MINUTES * 60);
    loop {
        task.sleep(POLL_EVERY);
        if task.ask_guest(&poll_script(), POLL_SECONDS).contains(DONE_MARK) {
            break;
        }
        if task.now() >= end {
            return Err(Failure(format!(
                "dxdiag had not finished after {WAIT_MINUTES} minutes. The guest may be stuck, and daemon.out \
                 says what the daemon saw."
            )));
        }
    }
    task.step("Fetching the report");
    let answer = task.ask_guest(&fetch_script(), FETCH_SECONDS);
    let report =
        report_from(&answer).ok_or_else(|| Failure::new("dxdiag's report could not be read back."))?;
    let saved = task.run_file(REPORT_FILE);
    fs::write(&saved, report)?;
    if task.machine.windows_maker() == Some(GpuMaker::Intel) {
        let folder = gather(task)?;
        task.step(&format!(
            "The report, the daemon's log and the card's details are in {}",
            folder.display()
        ));
    } else {
        task.step(&format!("The report is in {}", saved.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::tests::{machine, plain_task_context, with_vm_running};
    use crate::agent::stand_in::ScriptedAgent;
    use crate::testing::Scratch;
    use crate::text::base64_encode;
    use crate::vm_host::stand_in::RecordingHost;
    use std::sync::Arc;

    fn run(answers: &[&str], running: bool) -> (Result<()>, Arc<ScriptedAgent>, Scratch, Vec<u8>) {
        let (result, agent, scratch, saved, _) = run_on(answers, running, None);
        (result, agent, scratch, saved)
    }

    /// The same for a machine on a card of `maker`, with the machine's folder as well.
    fn run_on(
        answers: &[&str],
        running: bool,
        maker: Option<GpuMaker>,
    ) -> (Result<()>, Arc<ScriptedAgent>, Scratch, Vec<u8>, PathBuf) {
        let scratch = Scratch::new();
        let mut context = plain_task_context(&scratch);
        scratch.program(
            "program/intel-probe",
            "#!/bin/sh
echo \"probe of $1\"
",
        );
        if running {
            with_vm_running(&context);
        }
        let agent = Arc::new(ScriptedAgent::answering(answers));
        context.agent = agent.clone();
        let mut vm = machine(&context);
        if let Some(maker) = maker {
            vm.use_windows_card(maker, "0000:00:02.0");
        }
        crate::agent::make_token(&context, &vm).unwrap();
        let folder = vm.folder(&context);
        let host = RecordingHost::default();
        let mut task = Task::new(&context, &host, "dxdiag", vm);
        context.make_run_folder(None).unwrap();
        fs::write(task.run_file(OUTPUT_FILE), "the daemon's log").unwrap();
        let result = dxdiag(&mut task);
        let saved = fs::read(task.run_file(REPORT_FILE)).unwrap_or_default();
        (result, agent, scratch, saved, folder)
    }

    #[test]
    fn the_report_comes_back_into_the_run_folder_once_dxdiag_has_finished() {
        let report = "System Information\r\n  Display Devices: Intel(R) Arc(TM) Graphics\r\n";
        let wrapped = base64_encode(report.as_bytes()).replace("YXRpb24", "YXRpb24\r\n");
        let sent = format!("{REPORT_MARK}{wrapped}\r\n");
        let (result, agent, _scratch, saved) =
            run(&[STARTED_MARK, "waiting", "waiting", DONE_MARK, &sent], true);
        result.unwrap();
        assert_eq!(saved, report.as_bytes());
        let asked = agent.requests.lock().unwrap();
        assert!(
            asked[0].2.contains("dxdiag.exe") && asked[0].2.contains("'/dontskip','/t'"),
            "{}",
            asked[0].2
        );
        assert_eq!(asked.len(), 5, "a start, three looks, and the fetch");
    }

    #[test]
    fn for_an_intel_machine_the_report_the_log_and_the_probes_answer_are_gathered_in_its_folder() {
        let answers = [STARTED_MARK, DONE_MARK, &format!("{REPORT_MARK}{}", base64_encode(b"the report"))];
        let (result, _, _scratch, _, folder) = run_on(&answers, true, Some(GpuMaker::Intel));
        result.unwrap();
        let gathered = folder.join(GATHERED_FOLDER);
        assert_eq!(fs::read_to_string(gathered.join(REPORT_FILE)).unwrap(), "the report");
        assert_eq!(fs::read_to_string(gathered.join(PROBE_FILE)).unwrap().trim(), "probe of 0000:00:02.0");
        assert_eq!(fs::read_to_string(gathered.join(OUTPUT_FILE)).unwrap(), "the daemon's log");
        let (result, _, _scratch, _, folder) = run_on(&answers, true, None);
        result.unwrap();
        assert!(!folder.join(GATHERED_FOLDER).exists(), "only an Intel machine gathers");
    }

    #[test]
    fn a_stopped_vm_or_a_quiet_agent_is_said_and_nothing_is_saved() {
        let (result, agent, _scratch, _) = run(&[], false);
        assert!(result.unwrap_err().0.contains("isn't running"));
        assert!(agent.requests.lock().unwrap().is_empty());
        let (result, _, _scratch, saved) = run(&[], true);
        assert!(result.unwrap_err().0.contains("agent didn't answer"));
        assert!(saved.is_empty());
    }

    #[test]
    fn a_dxdiag_that_never_finishes_is_given_up_on_and_a_report_that_does_not_read_is_said() {
        let (result, _, _scratch, _) = run(&[STARTED_MARK], true);
        assert!(result.unwrap_err().0.contains("not finished after 5 minutes"));
        let (result, _, _scratch, saved) = run(&[STARTED_MARK, DONE_MARK, "report:"], true);
        assert!(result.unwrap_err().0.contains("could not be read back"));
        assert!(saved.is_empty());
    }
}
