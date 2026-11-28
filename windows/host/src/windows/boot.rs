//! Boots the machine: the card's driver onto its disk, or onto a Linux guest's files disk, a network
//! endpoint, the machine created and started through HCS, and then its GPU, in that order, as HCS
//! gives a GPU only to a running machine. It stays until the machine stops, as the machine stops
//! when its last handle closes, or until its time is up.

use super::{
    absolute, chosen_card, linux, machine, need_elevation, print_guest_report, refresh_windows_disk,
};
use crate::events::{Sink, State};
use crate::guest::linux::install_finished;
use crate::guest::{SHUTDOWN_LINE, report_done};
use crate::guest_ctl::{GuestCtl, LineWriter, read_lines};
use crate::machine::Guest;
use crate::record::{Record, files_disk};
use crate::settings::BootSettings;
use guesthouse_hcs::host;
use guesthouse_hcs::hvsock::parse_guid;
use guesthouse_hcs::network::{self, Endpoint, GUID};
use guesthouse_hcs::schema::network::{Network, bridged_network_id, bridged_network_name};
use guesthouse_hcs::schema::{Gpu, ModifySettingRequest, NetworkAdapter};
use guesthouse_hcs::serial::{self, OpenError, SerialPort, Server};
use guesthouse_hcs::system::{self, ComputeSystem};
use guesthouse_hcs::worker;
use guesthouse_jail::windows::{Handles, Jail, Jailed};
use std::io::{PipeReader, Read, Write};
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

/// Guesthouse's own network in HCN, which every machine shares and which outlives a run.
const NETWORK_ID: GUID = GUID::from_u128(0x5c2d8e41_7a3b_4f60_9d1e_3b8a6c4f2e17);
const NETWORK_NAME: &str = "Guesthouse network";
/// The network earlier builds made, with no DHCP server. HCN keeps a network until the host
/// restarts, so it's deleted here once nothing is plugged into it.
const OLD_NETWORK_ID: GUID = GUID::from_u128(0x9f3a6c2e_5b1d_4e8a_a7c4_2d6e8b0f1a35);
/// How often the machine's state is read while it runs.
const POLL: Duration = Duration::from_secs(1);
/// How long the guest's serial port may take to appear once the machine starts.
const SERIAL_SECONDS: u64 = 30;
/// The jailed reader of the serial port, beside this program.
const RELAY: &str = "guesthouse-serial-relay.exe";
/// Its AppContainer, which Windows keeps between runs.
const RELAY_PROFILE: &str = "guesthouse.serial-relay";
/// While the machine's state can't be read, the reason is told once in this many tries.
const UNREADABLE_REPEAT: u32 = 60;
/// How long a guest asked to shut down at the end of its time has before it's stopped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(180);
/// Ubuntu's installer speaks on the serial port soon after it starts, but only when the files
/// disk's own menu started it. An install boot this quiet for this long started something else.
const INSTALL_SILENCE: Duration = Duration::from_secs(300);
/// How long the line asking the guest to shut down may wait for the port, behind a line being
/// written, before the guest is asked through HCS instead.
const SHUTDOWN_WRITE: Duration = Duration::from_secs(2);

/// Boots the machine `settings` names and runs it until it stops, its time is up, or `stop` is set:
/// once to ask the guest to shut down, and again to stop it at once. Everything it has to say goes
/// to `sink`, ending with the machine's state as stopped. An error that ends it is returned, for the
/// caller to report, and not sent too.
pub fn boot(settings: &BootSettings, sink: Arc<dyn Sink>, stop: Arc<AtomicBool>) -> Result<(), String> {
    boot_with(settings, sink, stop, GuestCtl::new())
}

/// [`boot`], with `ctl` as the way to ask remoteman's host in a Windows guest while it runs. It
/// refuses every request once this returns.
pub fn boot_with(
    settings: &BootSettings,
    sink: Arc<dyn Sink>,
    stop: Arc<AtomicBool>,
    ctl: GuestCtl,
) -> Result<(), String> {
    sink.state(State::Starting);
    let ran = run(settings, &sink, stop, &ctl);
    ctl.end();
    sink.state(State::Off);
    ran
}

#[allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk
fn run(
    settings: &BootSettings,
    sink: &Arc<dyn Sink>,
    stop: Arc<AtomicBool>,
    ctl: &GuestCtl,
) -> Result<(), String> {
    need_elevation()?;
    let disk = absolute(&settings.disk)?;
    // A snapshot cut short by a crash is finished or undone first, so the disk is there to boot.
    crate::snapshots::repair(&disk)?;
    if !disk.is_file() {
        return Err(format!("There is no disk at {}. prepare makes one.", disk.display()));
    }
    let card = chosen_card(&settings.gpu)?;
    // A machine left from an earlier run still has the disk open, so it goes before the disk is
    // attached for the driver.
    if let Some(earlier) = ComputeSystem::open(&settings.name) {
        sink.note(format!(
            "A machine called {} is still there from an earlier run. Stopping it.",
            settings.name
        ));
        if let Err(error) = earlier.terminate() {
            sink.problem(format!("{error}"));
        }
    }
    // A shared folder that has gone isn't worth not booting for, so the machine boots without it.
    let mut shares: Vec<_> = settings
        .shares
        .iter()
        .filter(|share| {
            let there = share.folder.is_dir();
            if !there {
                sink.problem(format!(
                    "The shared folder {} isn't there, so this boot doesn't share it.",
                    share.folder.display()
                ));
            }
            there
        })
        .cloned()
        .collect();
    // The machine's own account opens the shared folders too, as it does its disks. A folder it
    // can't be let into isn't worth not booting for, so the machine boots without it. It's let in
    // before the guest is told of its shares, so the guest isn't told of one it can't have.
    shares.retain(|share| match system::grant_vm_access(&settings.name, &share.folder) {
        Ok(()) => true,
        Err(error) => {
            sink.problem(format!(
                "This boot doesn't share {}, as the machine couldn't be let into it: {error}",
                share.folder.display()
            ));
            false
        }
    });
    let mut record = Record::load(&disk)?;
    if record.mac.is_none() {
        record.mac = Some(network::mac_from(&host::new_guid().map_err(|error| error.to_string())?));
        record.save(&disk)?;
    }
    let hosts = settings
        .host_addresses
        .as_ref()
        .map(|addresses| crate::screen::hosts_file(settings.bridge.is_some(), addresses));
    match record.guest {
        Guest::Windows => refresh_windows_disk(
            &disk,
            card.as_ref(),
            settings.nvapi_wrapper,
            &shares,
            hosts.as_deref(),
            sink.as_ref(),
        )?,
        Guest::Linux => {
            linux::refresh_disk(&files_disk(&disk), card.as_ref(), &shares, record.installed, sink.as_ref())?;
            if !record.installed {
                sink.note("This boot installs Ubuntu. The machine turns itself off when it's done.");
            }
        }
    }
    let mac = record.mac.clone().expect("the record has a MAC by now");
    let (_endpoint, adapter) = if settings.network {
        plug_in(&mac, settings.bridge.as_deref(), sink.as_ref())?
    } else {
        (None, None)
    };
    // The pipes are named afresh each boot, with a random word.
    let pipe_tag =
        serial::pipe_tag().map_err(|error| format!("This boot's pipes couldn't be named: {error}"))?;
    let mut machine = machine(settings, disk, &record, adapter, pipe_tag);
    machine.shares = shares;
    system::ensure_guest_state_file(&machine.guest_state_path()).map_err(|error| error.to_string())?;
    system::ensure_runtime_state_file(&machine.runtime_state_path()).map_err(|error| error.to_string())?;
    let mut opened = vec![machine.disk.clone(), machine.guest_state_path(), machine.runtime_state_path()];
    opened.extend(machine.more_disks.iter().map(|disk| PathBuf::from(&disk.path)));
    // The disks of the snapshots its disk is made on, which the machine reads too.
    opened.extend(crate::snapshots::chain(&machine.disk)?);
    for path in &opened {
        system::grant_vm_access(&machine.name, path).map_err(|error| error.to_string())?;
    }

    let document = machine.document();
    let running = ComputeSystem::create(&machine.name, &document).map_err(|error| {
        let sent = serde_json::to_string_pretty(&document).unwrap_or_default();
        format!("{error}\n\nThe document sent:\n{sent}")
    })?;
    running.start().map_err(|error| error.to_string())?;
    sink.note(format!("{} started.", machine.name));
    if !machine.console_access.is_empty() {
        sink.note(format!("Its console is Remote Desktop on {}.", machine.console_pipe()));
        sink.console(&machine.console_pipe());
    }
    sink.state(State::Running);

    // SSH with no network, for an Ubuntu guest that's installed, as only it has the unit that
    // answers on its side. It's kept until the machine has stopped, and then it lets go of its port.
    let _ssh = settings
        .ssh_port
        .filter(|_| record.guest == Guest::Linux && record.installed)
        .and_then(|port| super::ssh::listen(port, &running, sink));

    // A guest still setting itself up gets no card: the host's card reset twice while a guest with
    // GPU-PV was in setup or shutting down, and setup doesn't need it. The guest's first boot
    // finishes setup, and every boot after has the card.
    if card.is_some() && !record.installed {
        sink.note("This boot sets the guest up, with no card. The boots after it have the card.");
    }
    if let Some(card) = card.as_ref().filter(|_| record.installed) {
        let change = ModifySettingRequest::gpu(&Gpu::list([card.interface_path.as_str()]));
        match running.modify(&change) {
            Ok(()) => sink.note(format!("It has {} by GPU-PV.", card.name)),
            Err(error) => sink.problem(format!("It runs without its card, as HCS refused it: {error}")),
        }
    }

    let pipe = machine.serial_pipe();
    // The serial port is taken only from the machine's own worker, which runs as the machine's account.
    let serial_pipe = running
        .properties()
        .map_err(|error| error.to_string())
        .and_then(|properties| {
            parse_guid(&properties.runtime_id)
                .ok_or_else(|| format!("HCS gave the machine the runtime ID {:?}", properties.runtime_id))
        })
        .map(|id| SerialPipe { name: pipe, worker: Server::Account(worker::account(id)) });
    let unjailed = !settings.jail;
    let disk = machine.disk.clone();
    let heard = Arc::new(AtomicBool::new(false));
    let heard_here = Arc::clone(&heard);
    let installing = record.guest == Guest::Linux && !record.installed;
    let guest = record.guest;
    // The guest's line saying its install is done is when the installer stops being attached.
    let until_report = settings.until_report;
    let line_sink = Arc::clone(sink);
    let line_stop = Arc::clone(&stop);
    let mut marked = false;
    let on_line = move |line: &str| {
        heard_here.store(true, Ordering::SeqCst);
        // The same as Ctrl+C: the guest is asked to shut down, and stopped if it won't.
        if until_report && report_done(line) {
            line_sink.note("The guest has reported, so it's asked to shut down.");
            line_stop.store(true, Ordering::SeqCst);
        }
        let set_up = match record.guest {
            Guest::Linux => install_finished(line),
            // Its report only runs once Windows has finished setting itself up.
            Guest::Windows => report_done(line),
        };
        // Each change is made to the record as it is on the disk now, so neither undoes the other.
        if let Some(fingerprint) = crate::screen::fingerprint_in(line) {
            let kept =
                Record::load(&disk).and_then(|kept| Record { screen: Some(fingerprint), ..kept }.save(&disk));
            match kept {
                Ok(()) => line_sink.note("The guest's screen is ready, and the viewer knows it."),
                Err(error) => line_sink.problem(error),
            }
        }
        if set_up && !record.installed && !marked {
            marked = true;
            let kept = Record::load(&disk).and_then(|kept| Record { installed: true, ..kept }.save(&disk));
            match kept {
                Ok(()) => line_sink.note("The guest is set up. The next boot gives it its card."),
                Err(error) => line_sink.problem(error),
            }
        }
    };
    let serial_sink = Arc::clone(sink);
    let ended = Arc::new(AtomicBool::new(false));
    let serial_ended = Arc::clone(&ended);
    // The port's writing end goes to `ctl` while the guest has it open, for asking the guest to shut
    // down and, a Windows guest, for remoteman's commands.
    let serial_ctl = ctl.clone();
    let answers = guest == Guest::Windows;
    match serial_pipe {
        Ok(pipe) => {
            thread::spawn(move || {
                print_serial(&pipe, unjailed, on_line, serial_sink, &serial_ended, &serial_ctl, answers)
            });
        }
        Err(error) => sink.problem(format!(
            "The guest's serial port is left unread, as the machine's worker couldn't be known: {error}"
        )),
    }
    sink.note("Waiting for the guest.");
    let limit = settings.for_minutes.map(|minutes| Duration::from_secs(minutes * 60));
    let silence = installing.then_some((INSTALL_SILENCE, heard));
    let waited = wait(&running, &stop, limit, silence, ctl, sink);
    ended.store(true, Ordering::SeqCst);
    ctl.end();
    // The report a Windows guest writes on its own disk can be read once the machine has let go.
    drop(running);
    if guest == Guest::Windows {
        print_guest_report(&machine.disk, sink.as_ref());
    }
    waited
}

/// An endpoint on Guesthouse's network, or on the network bridged onto the adapter `bridge` names,
/// deleted when dropped, and the card with this MAC that plugs into it.
fn plug_in(
    mac: &str,
    bridge: Option<&str>,
    sink: &dyn Sink,
) -> Result<(Option<Endpoint>, Option<NetworkAdapter>), String> {
    let _ = network::delete_network(&OLD_NETWORK_ID);
    let network_id = match bridge {
        None => {
            network::ensure_ics_network(&NETWORK_ID, NETWORK_NAME).map_err(|error| error.to_string())?;
            NETWORK_ID
        }
        Some(adapter) => bridged_network(adapter, sink)?,
    };
    let id = host::new_guid().map_err(|error| error.to_string())?;
    let endpoint = Endpoint::create(&network_id, id, mac).map_err(|error| error.to_string())?;
    match endpoint.properties() {
        Ok(properties) => sink.note(format!("The network gave its card: {properties}")),
        Err(error) => sink.problem(error.to_string()),
    }
    let adapter = NetworkAdapter { endpoint_id: endpoint.id_string(), mac_address: mac.to_owned() };
    Ok((Some(endpoint), Some(adapter)))
}

/// The network bridged onto the physical adapter `adapter`, made if HCN doesn't have it: one per
/// adapter, which HCN keeps until the host restarts. A boot that can't have it fails, as a machine
/// meant for the host's network that's put on another one is worse than one that doesn't start.
fn bridged_network(adapter: &str, sink: &dyn Sink) -> Result<GUID, String> {
    let id = GUID::from_u128(bridged_network_id(adapter));
    let wanted = Network::transparent(&bridged_network_name(adapter), adapter);
    sink.note(format!("Bridging the machine onto {adapter}, so it takes an address from the router."));
    match network::ensure_network(&id, &wanted) {
        Ok(true) => sink.note(format!(
            "Made the network {}. Windows binds its switch to {adapter}, which can drop this PC's own \
             connection for a few seconds.",
            wanted.name
        )),
        Ok(false) => {}
        Err(error) => {
            return Err(format!(
                "{error}. Bridging onto {adapter} needs Hyper-V's switch, and Wi-Fi adapters often can't be \
                 bridged. Give the machine the shared network, or another adapter, in its settings."
            ));
        }
    }
    Ok(id)
}

/// The serial port's writing end, as `ctl` writes to it.
struct Writing(Arc<SerialPort>);

impl LineWriter for Writing {
    fn write_line(&self, line: &[u8]) -> std::io::Result<()> {
        self.0.write_all(line)
    }
}

/// Asks the guest to shut down on its serial port. Gives whether the line was written.
fn ask_on_port(ctl: &GuestCtl) -> bool {
    ctl.send_line(SHUTDOWN_LINE, SHUTDOWN_WRITE)
}

/// The serial port read as a stream.
struct Reading(Arc<SerialPort>);

impl Read for Reading {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

/// Waits for the machine to stop. Returning closes the last handle, which stops the machine, so only
/// an answer that says it has stopped, or a second Ctrl+C, ends this: an answer that can't be read
/// is told and waited past.
fn wait(
    running: &ComputeSystem,
    stop: &AtomicBool,
    limit: Option<Duration>,
    silence: Option<(Duration, Arc<AtomicBool>)>,
    ctl: &GuestCtl,
    sink: &Arc<dyn Sink>,
) -> Result<(), String> {
    let started = Instant::now();
    let mut asked = false;
    let mut first_answer = true;
    let mut unreadable = 0u32;
    loop {
        thread::sleep(POLL);
        // When its time is up the guest is asked to shut down, as Ctrl+C would, and stopped if it
        // hasn't after a grace.
        if let Some(limit) = limit {
            let elapsed = started.elapsed();
            if (!asked && elapsed >= limit) || (asked && elapsed >= limit + SHUTDOWN_GRACE) {
                sink.note("Its time is up.");
                stop.store(true, Ordering::SeqCst);
            }
        }
        if let Some((quiet, heard)) = &silence
            && !heard.load(Ordering::SeqCst)
            && started.elapsed() >= *quiet
        {
            sink.note(
                "The installer has said nothing on the serial port, which it does only when the files \
                 disk's menu starts it, so the firmware or GRUB started something else. Stopping it.",
            );
            if let Err(error) = running.terminate() {
                sink.problem(format!("{error}"));
            }
            return Ok(());
        }
        if stop.swap(false, Ordering::SeqCst) {
            if asked {
                sink.note("Stopping it now.");
                if let Err(error) = running.terminate() {
                    sink.problem(format!("{error}"));
                }
                return Ok(());
            } else {
                asked = true;
                sink.state(State::Stopping);
                if ask_on_port(ctl) {
                    sink.note(
                        "Asked the guest to shut down, on its serial port. Asking again stops it at once.",
                    );
                    continue;
                }
                sink.note("Asking the guest to shut down. Asking again stops it at once.");
                // The call waits for the guest, so it has a handle and a thread of its own.
                let name = running.id().to_owned();
                let shutdown_sink = Arc::clone(sink);
                thread::spawn(move || {
                    if let Some(machine) = ComputeSystem::open(&name)
                        && let Err(error) = machine.shut_down()
                    {
                        shutdown_sink.problem(error.to_string());
                    }
                });
            }
        }
        match running.properties() {
            Ok(properties) => {
                if first_answer {
                    sink.note(format!("HCS says: {}", properties.raw));
                    first_answer = false;
                }
                unreadable = 0;
                if properties.stopped() {
                    sink.note(format!("The machine stopped: {} {}", properties.state, properties.exit_type));
                    return Ok(());
                }
            }
            Err(error) => {
                if unreadable.is_multiple_of(UNREADABLE_REPEAT) {
                    sink.problem(format!("The machine's state can't be read, so it's left running: {error}"));
                }
                unreadable += 1;
            }
        }
    }
}

/// Sends each line the guest writes to COM1 until the port closes. The port is read by the relay
/// in its jail, as those are bytes the guest chose, unless `unjailed` says to read it here: this
/// only copies the bytes, as they come, to the relay's input, as the host keeps the port to write
/// to it too.
/// The port closes whenever the guest restarts, as Windows does while it sets itself up, so it's
/// opened again each time until the machine has stopped. Its writing end goes to `ctl` while it's
/// open, and the lines of `ctl`'s channel go to it and nowhere else; `answers` says whether the
/// guest is one that answers on it.
fn print_serial(
    pipe: &SerialPipe,
    unjailed: bool,
    mut on_line: impl FnMut(&str),
    sink: Arc<dyn Sink>,
    ended: &AtomicBool,
    ctl: &GuestCtl,
    answers: bool,
) {
    let mut opened_before = false;
    while !ended.load(Ordering::SeqCst) {
        let Some(port) = open_port(pipe, opened_before, ended, sink.as_ref()) else { return };
        ctl.attach(Arc::new(Writing(Arc::clone(&port))), answers);
        if opened_before {
            sink.note("The guest restarted, so its serial port is read again.");
        }
        opened_before = true;
        if unjailed {
            sink.note("Reading the guest's serial port with no jail, as asked.");
            read_lines(Reading(port), &mut on_line, sink.as_ref(), ctl);
            ctl.detach();
            continue;
        }
        match relay(Reading(port)) {
            Ok((jailed, lines)) => {
                read_lines(lines, &mut on_line, sink.as_ref(), ctl);
                ctl.detach();
                let _ = jailed.wait();
            }
            Err(error) => {
                ctl.detach();
                sink.problem(format!(
                    "The guest's serial port is left unread, as its reader couldn't be jailed: {error}. \
                     Booting with no jail reads it anyway."
                ));
                return;
            }
        }
    }
}

/// A machine's serial port's pipe, and who must be serving it: the machine's worker.
struct SerialPipe {
    name: String,
    worker: Server,
}

/// The serial port, once the machine's worker serves it. The first time it has a while to appear
/// before that's told as a problem; after a restart it's waited for for as long as the machine runs.
/// A pipe of its name served by any other process is let go, told once, and the port looked for
/// again.
fn open_port(
    SerialPipe { name: pipe, worker }: &SerialPipe,
    opened_before: bool,
    ended: &AtomicBool,
    sink: &dyn Sink,
) -> Option<Arc<SerialPort>> {
    let started = Instant::now();
    let mut told = false;
    loop {
        if ended.load(Ordering::SeqCst) {
            return None;
        }
        match SerialPort::open(pipe, worker) {
            Ok(port) => return Some(Arc::new(port)),
            Err(error) if !opened_before && started.elapsed() >= Duration::from_secs(SERIAL_SECONDS) => {
                sink.problem(format!("The guest's serial port at {pipe} could not be opened: {error}"));
                return None;
            }
            Err(error @ OpenError::OtherServer { .. }) => {
                if !told {
                    told = true;
                    sink.problem(format!("The pipe {pipe} isn't taken as the guest's serial port: {error}."));
                }
                thread::sleep(POLL);
            }
            Err(OpenError::Io(_)) => thread::sleep(POLL),
        }
    }
}

/// The serial relay, started in its jail, and the end of its output. What the guest writes is
/// copied to its input as it comes, on a thread of its own, until the port closes.
fn relay(mut port: Reading) -> std::io::Result<(Jailed, PipeReader)> {
    let program = std::env::current_exe()?.with_file_name(RELAY);
    let (lines, relay_output) = std::io::pipe()?;
    let (relay_input, mut to_relay) = std::io::pipe()?;
    thread::spawn(move || {
        // Ends when the port closes or the relay goes, and closing its end ends the relay's input.
        let _ = std::io::copy(&mut port, &mut to_relay);
        let _ = to_relay.flush();
    });
    let handles = Handles {
        stdin: Some(OwnedHandle::from(relay_input)),
        stderr: Some(OwnedHandle::from(relay_output.try_clone()?)),
        stdout: Some(OwnedHandle::from(relay_output)),
    };
    let jailed = Jail::new(RELAY_PROFILE).spawn(&program, &[], handles)?;
    Ok((jailed, lines))
}
