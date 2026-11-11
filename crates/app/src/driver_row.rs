//! The row that takes a card maker's Windows driver out of the maker's installer, for a Windows machine
//! on an AMD or Intel card. It's there only while the driver is missing, and says whether the
//! installer covers the card.

use crate::common::{choose_file, in_background};
use crate::rows::row_button;
use crate::window::Window;
use adw::prelude::*;
use gtk::glib;
use guesthouse_app_core::Failure;
use guesthouse_app_core::gpu_maker::GpuMaker;
use guesthouse_app_core::setup::amd_driver::{AMD_RELEASE, read_amd_installer, unpack_amd};
use guesthouse_app_core::setup::driver::Extraction;
use guesthouse_app_core::setup::intel_driver::{INTEL_WINDOWS_DRIVER, read_intel_installer, unpack_intel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

/// What the row says for each maker whose driver is taken from an installer.
fn title(maker: GpuMaker) -> String {
    format!("{}'s driver", maker.name())
}

fn ask(maker: GpuMaker) -> String {
    match maker {
        GpuMaker::Amd => format!(
            "Choose AMD's installer for Adrenalin {AMD_RELEASE}. The display driver is taken out of it."
        ),
        GpuMaker::Intel => format!(
            "Choose Intel's installer for graphics driver {INTEL_WINDOWS_DRIVER}, gfx_win_101.9175.exe. The drivers for the card are taken out of it."
        ),
        GpuMaker::Nvidia => String::new(),
    }
}

struct Parts {
    window: Window,
    row: adw::ActionRow,
    choose: gtk::Button,
    bar: gtk::ProgressBar,
    /// The maker whose installer is asked for, and the card it must cover.
    maker: Cell<GpuMaker>,
    device: Cell<u32>,
    extraction: RefCell<Option<Arc<Extraction>>>,
    /// What runs once the driver is unpacked.
    ready: RefCell<Option<Rc<dyn Fn()>>>,
}

#[derive(Clone)]
pub struct DriverRow(Rc<Parts>);

impl DriverRow {
    pub fn new(window: &Window, group: &adw::PreferencesGroup) -> Self {
        let row = adw::ActionRow::builder().title(title(GpuMaker::Amd)).visible(false).build();
        let choose = row_button("Choose");
        row.add_suffix(&choose);
        let bar = gtk::ProgressBar::builder().visible(false).valign(gtk::Align::Center).build();
        row.add_suffix(&bar);
        group.add(&row);
        let this = Self(Rc::new(Parts {
            window: window.clone(),
            row,
            choose: choose.clone(),
            bar,
            maker: Cell::new(GpuMaker::Amd),
            device: Cell::new(0),
            extraction: RefCell::default(),
            ready: RefCell::default(),
        }));
        let choosing = this.clone();
        choose.connect_clicked(move |_| choosing.on_choose());
        this
    }

    /// `ready` runs once the driver is unpacked.
    pub fn connect_ready(&self, ready: impl Fn() + 'static) {
        *self.0.ready.borrow_mut() = Some(Rc::new(ready));
    }

    /// Shows the row for the card `device` of `maker` while its driver is missing, and hides it
    /// otherwise.
    pub fn show_for(&self, wanted: Option<(GpuMaker, u32)>) {
        let parts = &self.0;
        parts.row.set_visible(wanted.is_some());
        if let Some((maker, device)) = wanted {
            parts.maker.set(maker);
            parts.device.set(device);
            parts.row.set_title(&title(maker));
            parts.row.set_subtitle(&ask(maker));
        }
    }

    fn on_choose(&self) {
        let window = self.0.window.clone();
        let failing = window.clone();
        let this = self.clone();
        let dialog = format!("Choose {}'s Driver Installer", self.0.maker.get().name());
        choose_file(
            window.widget(),
            &dialog,
            "Windows installers",
            "exe",
            move |path| this.unpack(path),
            move |text| failing.problem(&text),
        );
    }

    fn unpack(&self, path: String) {
        let parts = &self.0;
        parts.choose.set_sensitive(false);
        parts.row.set_subtitle("Reading the installer");
        parts.bar.set_fraction(0.0);
        parts.bar.set_visible(true);
        let extraction = Arc::new(Extraction::new(&path));
        *parts.extraction.borrow_mut() = Some(extraction.clone());
        let (progress, percentages) = async_channel::unbounded();
        let (bar, row) = (parts.bar.clone(), parts.row.clone());
        glib::spawn_future_local(async move {
            while let Ok(percent) = percentages.recv().await {
                row.set_subtitle("Copying the driver's files");
                bar.set_fraction(f64::from(percent) / 100.0);
            }
        });
        let (maker, device) = (parts.maker.get(), parts.device.get());
        let context = parts.window.context();
        let working = context.clone();
        let this = self.clone();
        in_background(
            &context,
            move || {
                let progress = |percent| {
                    let _ = progress.try_send(percent);
                };
                let stopped = || Failure::new("Stopped.");
                match maker {
                    GpuMaker::Intel => {
                        let package = read_intel_installer(&path, device)?;
                        if extraction.cancelled() {
                            return Err(stopped());
                        }
                        unpack_intel(&extraction, &working, &package, &progress)
                    }
                    GpuMaker::Amd => {
                        let package = read_amd_installer(&path, device)?;
                        if extraction.cancelled() {
                            return Err(stopped());
                        }
                        unpack_amd(&extraction, &working, &package, &progress)
                    }
                    // Its driver comes from the setup page, and this row is never shown for it.
                    GpuMaker::Nvidia => Err(Failure::new("NVIDIA's driver is set up from the setup page.")),
                }
            },
            move |result| {
                let parts = &this.0;
                *parts.extraction.borrow_mut() = None;
                parts.bar.set_visible(false);
                parts.choose.set_sensitive(true);
                match result {
                    Ok(_) => {
                        let ready = parts.ready.borrow().clone();
                        if let Some(ready) = ready {
                            ready();
                        }
                    }
                    Err(text) => parts.row.set_subtitle(&glib::markup_escape_text(&text)),
                }
            },
        );
    }

    /// Stops an unpacking that is under way, as when its dialog closes.
    pub fn stop(&self) {
        if let Some(extraction) = self.0.extraction.borrow().as_ref() {
            extraction.cancel();
        }
    }
}
