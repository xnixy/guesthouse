//! `guesthouse-daemon [PROJECT_FOLDER]`: serves one VM, named by `GUESTHOUSE_SLOT`.
//!
//! The real backends are behind the `nvidia`, `amd` and `intel` features. Each run opens one card:
//! the AMD card named in `GUESTHOUSE_AMD_CARD`, the Intel card named in `GUESTHOUSE_INTEL_CARD` or the
//! NVIDIA card named in `GUESTHOUSE_NVIDIA_CARD` if the app named one, and else the NVIDIA card setup
//! chose. A build without `nvidia` takes its card
//! from the AMD setup file when none is named, or with no `amd` either, from the Intel one. A build
//! with no backend can only say so. The `stand-in` feature answers messages with nothing behind
//! them, to try the sockets by hand.

use guesthouse_daemon::logger;
use guesthouse_daemon::slot::Slot;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let verbose = env::var_os("GUESTHOUSE_VERBOSE").is_some();
    logger::start(verbose);
    let slot = match Slot::from_environment() {
        Ok(slot) => slot,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let project = env::args_os().nth(1).map_or_else(|| PathBuf::from("."), PathBuf::from);
    match serve(slot, project) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(any(feature = "stand-in", feature = "nvidia", feature = "amd", feature = "intel")))]
fn serve(_slot: Slot, _project: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    Err("This build has no GPU backend in it, so it cannot run a guest.".into())
}

/// Serves on the card the app chose for this guest.
#[cfg(any(feature = "nvidia", feature = "amd", feature = "intel"))]
fn serve(slot: Slot, project: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_daemon::card_choice::{amd_card, intel_card, nvidia_card};
    match (amd_card()?, intel_card()?, nvidia_card()?) {
        (Some(card), None, None) => serve_amd(slot, project, Some(card)),
        (None, Some(card), None) => serve_intel(slot, project, Some(card)),
        (None, None, card) => serve_nvidia(slot, project, card),
        _ => Err("This guest was given cards of more than one maker.".into()),
    }
}

#[cfg(all(any(feature = "nvidia", feature = "intel"), not(feature = "amd")))]
fn serve_amd(
    _slot: Slot,
    _project: PathBuf,
    _card: Option<guesthouse_backend::PciAddress>,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("This daemon was built without the AMD backend, so it cannot run a guest on an AMD card.".into())
}

#[cfg(all(any(feature = "nvidia", feature = "amd"), not(feature = "intel")))]
fn serve_intel(
    _slot: Slot,
    _project: PathBuf,
    _card: Option<guesthouse_backend::PciAddress>,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("This daemon was built without the Intel backend, so it cannot run a guest on an Intel card.".into())
}

/// With no NVIDIA backend, a guest with no card named is on the card in the AMD setup file.
#[cfg(all(feature = "amd", not(feature = "nvidia")))]
fn serve_nvidia(
    slot: Slot,
    project: PathBuf,
    card: Option<guesthouse_backend::PciAddress>,
) -> Result<(), Box<dyn std::error::Error>> {
    without_nvidia(card)?;
    serve_amd(slot, project, None)
}

/// With neither the NVIDIA nor the AMD backend, it is on the card in the Intel setup file.
#[cfg(all(feature = "intel", not(feature = "nvidia"), not(feature = "amd")))]
fn serve_nvidia(
    slot: Slot,
    project: PathBuf,
    card: Option<guesthouse_backend::PciAddress>,
) -> Result<(), Box<dyn std::error::Error>> {
    without_nvidia(card)?;
    serve_intel(slot, project, None)
}

/// A build with no NVIDIA backend can't run a guest the app gave an NVIDIA card.
#[cfg(all(any(feature = "amd", feature = "intel"), not(feature = "nvidia")))]
fn without_nvidia(card: Option<guesthouse_backend::PciAddress>) -> Result<(), Box<dyn std::error::Error>> {
    match card {
        Some(_) => Err(
            "This daemon was built without the NVIDIA backend, so it cannot run a guest on an NVIDIA card."
                .into(),
        ),
        None => Ok(()),
    }
}

/// Starts the AMD backend on `card`, or with none, on the card in the AMD setup file, and serves.
/// This opens that AMD card and no other.
#[cfg(feature = "amd")]
fn serve_amd(
    slot: Slot,
    project: PathBuf,
    card: Option<guesthouse_backend::PciAddress>,
) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_backend_amd::AmdBackend;
    use guesthouse_daemon::vendor_setup::AMD;

    let mut setup = AMD.read()?;
    if card.is_some() {
        setup.card_address = card;
    }
    let address = setup.card_address.map(|pci| pci.to_string());
    let node = guesthouse_backend_amd::amd_render_node(address.as_deref())
        .ok_or("No AMD graphics card was found at the address this guest was given.")?;
    log::info!("AMD card at {}", node.display());
    let memory_device = PathBuf::from(guesthouse_backend_amd::Udmabuf::DEVICE);
    let memory_device = memory_device.exists().then_some(memory_device);
    let needs = guesthouse_daemon::jail::Needs::of_render_node(
        slot,
        &project,
        &setup.driver_folder,
        node,
        memory_device,
    );
    let run_folder = slot.run_folder(&project);
    serve_on_render_node::<AmdBackend>(slot, project, &setup, needs, |backend| {
        if env::var_os(guesthouse_backend_amd::IB_DUMP_VARIABLE).is_some() {
            backend.keep_command_buffers(&run_folder, guesthouse_daemon::logger::started());
        }
        let on = |name| guesthouse_backend_amd::video_switch_is_on(name, env::var_os(name).as_deref());
        if on(guesthouse_backend_amd::VIDEO_VARIABLE) {
            backend.offer_video();
        }
        let rest = env::var(guesthouse_backend_amd::VIDEO_REST_VARIABLE).ok();
        backend.close_and_rest_video(on(guesthouse_backend_amd::VIDEO_CLOSE_VARIABLE), rest.as_deref());
    })
}

/// Starts the Intel backend on `card`, or with none, on the card in the Intel setup file, and
/// serves. This opens that Intel card and no other.
#[cfg(feature = "intel")]
fn serve_intel(
    slot: Slot,
    project: PathBuf,
    card: Option<guesthouse_backend::PciAddress>,
) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_backend_intel::IntelBackend;
    use guesthouse_daemon::jail::Needs;
    use guesthouse_daemon::vendor_setup::INTEL;

    let mut setup = INTEL.read()?;
    if card.is_some() {
        setup.card_address = card;
    }
    // A recording a person named stands in for the card, and then no card is looked for or opened.
    let needs = match guesthouse_backend_intel::recording::recorded_card_file() {
        Some(recording) => Needs::of_recorded_card(slot, &project, &setup.driver_folder, recording),
        None => {
            let address = setup.card_address.map(|pci| pci.to_string());
            let node = guesthouse_backend_intel::intel_render_node(address.as_deref()).ok_or(
                "No Intel graphics card on the xe driver was found at the address this guest was given.",
            )?;
            log::info!("Intel card at {}", node.device_file.display());
            // Intel's backend makes no dma-buf of the guest's memory, so it is given no device for one.
            Needs::of_render_node(slot, &project, &setup.driver_folder, node.device_file, None)
        }
    };
    use guesthouse_backend_intel::stage::{DESKTOP_VARIABLE, Desktop};
    let desktop = Desktop::from_environment();
    if desktop == Desktop::Software {
        log::info!(
            "Windows composes the guest's desktop in software on this start; {DESKTOP_VARIABLE}=0 gives it \
             the hardware desktop instead"
        );
    }
    let run_folder = slot.run_folder(&project);
    serve_on_render_node::<IntelBackend>(slot, project, &setup, needs, |backend| {
        let why = backend.set_desktop(desktop);
        if let Err(error) = guesthouse_daemon::desktop_note::write_note(&run_folder, why.as_deref()) {
            log::warn!("The note on the guest's desktop couldn't be written: {error}");
        }
    })
}

/// Serves with a backend whose card is reached through one render node, which with what else is in
/// `needs` is all the jail lets this process open. A recorded card needs no device at all.
/// `configure` sets the backend's own switches once it is open.
#[cfg(any(feature = "amd", feature = "intel"))]
fn serve_on_render_node<B: guesthouse_backend::Backend>(
    slot: Slot,
    project: PathBuf,
    setup: &guesthouse_backend::Setup,
    needs: guesthouse_daemon::jail::Needs,
    configure: impl FnOnce(&mut B),
) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_core::{Config, MemoryLimits, Session};
    use guesthouse_daemon::environment::{guest_ram, video_cap, within_the_cards};
    use guesthouse_daemon::guest_memory::SharedMemory;
    use guesthouse_daemon::jail::{lock_calls, lock_files};
    use guesthouse_daemon::router::Router;
    use guesthouse_daemon::server::Server;
    use guesthouse_daemon::startup::start_backend;

    let run_folder = slot.run_folder(&project);
    let (ram, cap) = (env::var("GUESTHOUSE_RAM").ok(), env::var("GUESTHOUSE_VRAM").ok());
    let memory = SharedMemory::from_environment(slot, guest_ram(ram.as_deref()))?;
    needs.prepare()?;
    let locked = lock_files(&needs)?;
    let mut backend: B = start_backend(setup)?;
    configure(&mut backend);
    let cap = within_the_cards(video_cap(cap.as_deref()), backend.video_memory_cap());
    let limits = MemoryLimits::new(backend.facts(), cap, guest_ram(ram.as_deref()));
    let config = Config::new(limits);
    let mut session = Session::new(backend, memory, config);
    session.timings_mut().set_enabled(env::var_os("GUESTHOUSE_TIMING").is_some());
    let mut server = Server::bind(&run_folder, Router::new(session))?;
    lock_calls(locked)?;
    server.run()?;
    Ok(())
}

/// Starts the NVIDIA backend on `card`, or with none, on the NVIDIA card setup chose, and serves.
#[cfg(feature = "nvidia")]
fn serve_nvidia(
    slot: Slot,
    project: PathBuf,
    card: Option<guesthouse_backend::PciAddress>,
) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_backend::Backend;
    use guesthouse_backend_nvidia::NvidiaBackend;
    use guesthouse_backend_nvidia::rm::linux::LinuxRm;
    use guesthouse_core::{Config, MemoryLimits, Session};
    use guesthouse_daemon::environment::{guest_ram, video_cap};
    use guesthouse_daemon::guest_memory::SharedMemory;
    use guesthouse_daemon::jail::{Needs, lock_calls, lock_files};
    use guesthouse_daemon::router::Router;
    use guesthouse_daemon::server::Server;
    use guesthouse_daemon::setup::{on_card, read_setup};
    use guesthouse_daemon::startup::start_backend;

    let setup = match card {
        Some(card) => on_card(read_setup()?, card)?,
        None => read_setup()?,
    };
    let run_folder = slot.run_folder(&project);
    let (ram, cap) = (env::var("GUESTHOUSE_RAM").ok(), env::var("GUESTHOUSE_VRAM").ok());
    let memory = SharedMemory::from_environment(slot, guest_ram(ram.as_deref()))?;
    let needs = Needs::of_card(slot, &project, &setup.driver_folder, setup.card_address.as_ref());
    needs.prepare()?;
    let locked = lock_files(&needs)?;
    let mut backend: NvidiaBackend<LinuxRm> = start_backend(&setup)?;
    if let Some(micros) =
        timeslice("GUESTHOUSE_TIMESLICE_US", env::var("GUESTHOUSE_TIMESLICE_US").ok().as_deref())
    {
        backend.set_graphics_timeslice(micros);
    }
    let shared = env::var("GUESTHOUSE_SHARED_CONTEXT_TIMESLICE_US").ok();
    let shared = timeslice("GUESTHOUSE_SHARED_CONTEXT_TIMESLICE_US", shared.as_deref());
    backend.set_shared_context_timeslice(shared.unwrap_or(SHARED_CONTEXT_TIMESLICE_MICROS));
    let limits = MemoryLimits::new(backend.facts(), video_cap(cap.as_deref()), guest_ram(ram.as_deref()));
    let config = Config::new(limits);
    let mut session = Session::new(backend, memory, config);
    session.timings_mut().set_enabled(env::var_os("GUESTHOUSE_TIMING").is_some());
    if env::var_os("GUESTHOUSE_HELD_COPIES").is_some() {
        session.set_copy_order(guesthouse_core::CopyOrder::Held);
    }
    let mut server = Server::bind(&run_folder, Router::new(session))?;
    lock_calls(locked)?;
    server.run()?;
    Ok(())
}

/// How long a ring channel of a context with several queues holds the GPU, unless
/// `GUESTHOUSE_SHARED_CONTEXT_TIMESLICE_US` says otherwise (0 for the RM's own slice). The RM's is
/// 1,024 µs, which a Direct3D 12 queue's waiting channel sits through every frame.
#[cfg(feature = "nvidia")]
const SHARED_CONTEXT_TIMESLICE_MICROS: u64 = 250;

/// A timeslice in microseconds, if the setting is there and makes sense.
#[cfg(feature = "nvidia")]
fn timeslice(name: &str, text: Option<&str>) -> Option<u64> {
    let text = text?;
    let micros = text.parse().ok();
    if micros.is_none() {
        log::warn!("{name} is {text}, which isn't a number of microseconds");
    }
    micros
}

#[cfg(all(feature = "stand-in", not(any(feature = "nvidia", feature = "amd", feature = "intel"))))]
fn serve(slot: Slot, project: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_backend::Backend;
    use guesthouse_backend_standin::{SparseMemory, StandIn};
    use guesthouse_core::{Config, MemoryLimits, Session};
    use guesthouse_daemon::environment::{guest_ram, video_cap};
    use guesthouse_daemon::guest_memory::WINDOW_BASE;
    use guesthouse_daemon::handover::Part;
    use guesthouse_daemon::jail::{Needs, lock_calls, lock_files};
    use guesthouse_daemon::memory_files::MemoryFiles;
    use guesthouse_daemon::router::Router;
    use guesthouse_daemon::server::Server;

    /// A window of the usual size, which costs nothing as the stand-in stores only what is written.
    const WINDOW_SIZE: u64 = 8 << 30;

    let run_folder = slot.run_folder(&project);
    let (ram, cap) = (env::var("GUESTHOUSE_RAM").ok(), env::var("GUESTHOUSE_VRAM").ok());
    // It keeps nothing in them, but holds them as the real one does, so a start from the app, or
    // with the files named, is the same from outside. By hand there are none, which is fine here.
    let held: Vec<_> = match MemoryFiles::from_environment()? {
        Some(files) => {
            let (window, ram) = files.open(guest_ram(ram.as_deref()))?;
            vec![window, ram]
        }
        None => Part::ALL.iter().filter_map(|part| part.inherited(slot).ok()).collect(),
    };
    if held.is_empty() {
        log::warn!("no guest memory was handed over");
    }
    let needs = Needs { run_folder: run_folder.clone(), ..Needs::default() };
    needs.prepare()?;
    let locked = lock_files(&needs)?;
    let backend = StandIn::new();
    let limits = MemoryLimits::new(backend.facts(), video_cap(cap.as_deref()), guest_ram(ram.as_deref()));
    let memory = SparseMemory::new(WINDOW_BASE + WINDOW_SIZE);
    let router = Router::new(Session::new(backend, memory, Config::new(limits)));
    log::warn!("serving {} with a stand-in GPU: this will not run a guest", run_folder.display());
    let mut server = Server::bind(&run_folder, router)?;
    lock_calls(locked)?;
    server.run()?;
    Ok(())
}
