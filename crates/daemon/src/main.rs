//! `guesthouse-daemon [PROJECT_FOLDER]`: serves one VM, named by `GUESTHOUSE_SLOT`.
//!
//! The real backends are behind the `nvidia`, `amd` and `intel` features. Each run opens one card:
//! the AMD card named in `GUESTHOUSE_AMD_CARD` or the Intel card named in `GUESTHOUSE_INTEL_CARD` if
//! the app named one, and else the NVIDIA card setup chose. A build without `nvidia` takes its card
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
    use guesthouse_daemon::card_choice::{amd_card, intel_card};
    match (amd_card()?, intel_card()?) {
        (Some(_), Some(_)) => Err("This guest was given both an AMD card and an Intel card.".into()),
        (Some(card), None) => serve_amd(slot, project, Some(card)),
        (None, Some(card)) => serve_intel(slot, project, Some(card)),
        (None, None) => serve_nvidia(slot, project),
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
fn serve_nvidia(slot: Slot, project: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    serve_amd(slot, project, None)
}

/// With neither the NVIDIA nor the AMD backend, it is on the card in the Intel setup file.
#[cfg(all(feature = "intel", not(feature = "nvidia"), not(feature = "amd")))]
fn serve_nvidia(slot: Slot, project: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    serve_intel(slot, project, None)
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
    serve_on_render_node::<AmdBackend>(slot, project, &setup, node, |_| {})
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
    use guesthouse_daemon::vendor_setup::INTEL;

    let mut setup = INTEL.read()?;
    if card.is_some() {
        setup.card_address = card;
    }
    let address = setup.card_address.map(|pci| pci.to_string());
    let node = guesthouse_backend_intel::intel_render_node(address.as_deref())
        .ok_or("No Intel graphics card on the xe driver was found at the address this guest was given.")?;
    log::info!("Intel card at {}", node.device_file.display());
    let software_desktop = env::var_os(INTEL_SOFTWARE_DESKTOP).is_some();
    serve_on_render_node::<IntelBackend>(slot, project, &setup, node.device_file, |backend| {
        backend.set_software_desktop(software_desktop)
    })
}

/// A lab switch for an Intel guest: set, the guest is refused the Direct3D 9, 10 and 11 drivers, so
/// its desktop composes in software while Direct3D 12, compute and Vulkan still reach the backend.
/// Untried on a guest.
#[cfg(feature = "intel")]
const INTEL_SOFTWARE_DESKTOP: &str = "GUESTHOUSE_INTEL_SOFTWARE_DESKTOP";

/// Serves with a backend whose card is reached through one render node, which is the only device
/// file the jail lets this process open. `configure` sets the backend's own switches once it is open.
#[cfg(any(feature = "amd", feature = "intel"))]
fn serve_on_render_node<B: guesthouse_backend::Backend>(
    slot: Slot,
    project: PathBuf,
    setup: &guesthouse_backend::Setup,
    node: PathBuf,
    configure: impl FnOnce(&mut B),
) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_core::{Config, MemoryLimits, Session};
    use guesthouse_daemon::environment::{guest_ram, video_cap};
    use guesthouse_daemon::guest_memory::SharedMemory;
    use guesthouse_daemon::jail::{Needs, lock_calls, lock_files};
    use guesthouse_daemon::router::Router;
    use guesthouse_daemon::server::Server;
    use guesthouse_daemon::startup::start_backend;

    let run_folder = slot.run_folder(&project);
    let memory = SharedMemory::inherited(slot)?;
    let needs = Needs::of_render_node(slot, &project, &setup.driver_folder, node);
    needs.prepare()?;
    let locked = lock_files(&needs)?;
    let mut backend: B = start_backend(setup)?;
    configure(&mut backend);
    let (ram, cap) = (env::var("GUESTHOUSE_RAM").ok(), env::var("GUESTHOUSE_VRAM").ok());
    let limits = MemoryLimits::new(backend.facts(), video_cap(cap.as_deref()), guest_ram(ram.as_deref()));
    let config = Config::new(limits);
    let mut session = Session::new(backend, memory, config);
    session.timings_mut().set_enabled(env::var_os("GUESTHOUSE_TIMING").is_some());
    let mut server = Server::bind(&run_folder, Router::new(session))?;
    lock_calls(locked)?;
    server.run()?;
    Ok(())
}

/// Starts the NVIDIA backend and serves. This opens the NVIDIA card setup chose.
#[cfg(feature = "nvidia")]
fn serve_nvidia(slot: Slot, project: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use guesthouse_backend::Backend;
    use guesthouse_backend_nvidia::NvidiaBackend;
    use guesthouse_backend_nvidia::rm::linux::LinuxRm;
    use guesthouse_core::{Config, MemoryLimits, Session};
    use guesthouse_daemon::environment::{guest_ram, video_cap};
    use guesthouse_daemon::guest_memory::SharedMemory;
    use guesthouse_daemon::jail::{Needs, lock_calls, lock_files};
    use guesthouse_daemon::router::Router;
    use guesthouse_daemon::server::Server;
    use guesthouse_daemon::setup::read_setup;
    use guesthouse_daemon::startup::start_backend;

    let setup = read_setup()?;
    let run_folder = slot.run_folder(&project);
    let memory = SharedMemory::inherited(slot)?;
    let needs = Needs::of_card(slot, &project, &setup.driver_folder);
    needs.prepare()?;
    let locked = lock_files(&needs)?;
    let mut backend: NvidiaBackend<LinuxRm> = start_backend(&setup)?;
    if let Some(micros) = timeslice(env::var("GUESTHOUSE_TIMESLICE_US").ok().as_deref()) {
        backend.set_graphics_timeslice(micros);
    }
    let (ram, cap) = (env::var("GUESTHOUSE_RAM").ok(), env::var("GUESTHOUSE_VRAM").ok());
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

/// A timeslice in microseconds, if the setting is there and makes sense.
#[cfg(feature = "nvidia")]
fn timeslice(text: Option<&str>) -> Option<u64> {
    let text = text?;
    let micros = text.parse().ok();
    if micros.is_none() {
        log::warn!("GUESTHOUSE_TIMESLICE_US is {text}, which isn't a number of microseconds");
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
    use guesthouse_daemon::router::Router;
    use guesthouse_daemon::server::Server;

    /// A window of the usual size, which costs nothing as the stand-in stores only what is written.
    const WINDOW_SIZE: u64 = 8 << 30;

    let run_folder = slot.run_folder(&project);
    // It keeps nothing in them, but holds them as the real one does, so a start from the app is
    // the same from outside. By hand there are none, which is fine here.
    let handed: Vec<_> = Part::ALL.iter().filter_map(|part| part.inherited(slot).ok()).collect();
    if handed.is_empty() {
        log::warn!("no guest memory was handed over");
    }
    let needs = Needs { run_folder: run_folder.clone(), ..Needs::default() };
    needs.prepare()?;
    let locked = lock_files(&needs)?;
    let backend = StandIn::new();
    let (ram, cap) = (env::var("GUESTHOUSE_RAM").ok(), env::var("GUESTHOUSE_VRAM").ok());
    let limits = MemoryLimits::new(backend.facts(), video_cap(cap.as_deref()), guest_ram(ram.as_deref()));
    let memory = SparseMemory::new(WINDOW_BASE + WINDOW_SIZE);
    let router = Router::new(Session::new(backend, memory, Config::new(limits)));
    log::warn!("serving {} with a stand-in GPU: this will not run a guest", run_folder.display());
    let mut server = Server::bind(&run_folder, router)?;
    lock_calls(locked)?;
    server.run()?;
    Ok(())
}
