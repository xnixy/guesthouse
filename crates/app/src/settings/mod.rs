//! A machine's settings. Each group of rows is a module of its own, and gets the same three things:
//! the window, the machine, and this dialog to put its own dialogs over.

mod disk;
mod display;
mod forwards;
mod gpu_choice;
mod gpu_preview;
mod hardware;
mod jail;
mod linux_gpu;
mod network;
mod remove;
mod sharing;
mod snapshots;
mod token;
mod usb;

use crate::window::{MachineHandle, Window};
use adw::prelude::*;
use guesthouse_app_core::tasks::{State, Status};
use std::rc::Rc;

/// What every group of rows works with.
#[derive(Clone)]
pub struct Shared {
    window: Window,
    machine: MachineHandle,
    dialog: adw::PreferencesDialog,
}

impl Shared {
    fn save(&self) {
        self.window.save_machine(&self.machine.borrow());
        self.window.refresh();
    }

    /// Runs a change that can be refused, and shows the refusal.
    fn change(
        &self,
        action: impl FnOnce(&mut guesthouse_app_core::machine::Machine) -> guesthouse_app_core::Result<()>,
    ) {
        if let Err(failure) = action(&mut self.machine.borrow_mut()) {
            self.window.problem(&failure.0);
        }
        self.window.refresh();
    }
}

/// Whether the VM is off, with nothing going on.
fn is_off(status: &Status) -> bool {
    status.state == State::Off && !status.busy
}

struct Parts {
    shared: Shared,
    hardware: hardware::Hardware,
    display: display::Display,
    disk: disk::Disk,
    snapshots: snapshots::Snapshots,
    _token: token::Token,
    _gpu_preview: gpu_preview::GpuPreview,
    _jail: jail::Jail,
    linux_gpu: linux_gpu::LinuxGpu,
    sharing: sharing::Sharing,
    network: network::Network,
    usb: usb::Usb,
    remove: remove::Remove,
}

#[derive(Clone)]
pub struct SettingsDialog(Rc<Parts>);

impl SettingsDialog {
    pub fn new(window: &Window, machine: MachineHandle) -> Self {
        let dialog = adw::PreferencesDialog::new();
        dialog.set_title(&format!("{} Settings", machine.borrow().name));
        let page = adw::PreferencesPage::new();
        dialog.add(&page);
        let shared = Shared { window: window.clone(), machine, dialog };
        let this = Self(Rc::new(Parts {
            hardware: hardware::Hardware::new(&shared, &page),
            display: display::Display::new(&shared, &page),
            disk: disk::Disk::new(&shared, &page),
            snapshots: snapshots::Snapshots::new(&shared, &page),
            sharing: sharing::Sharing::new(&shared, &page),
            network: network::Network::new(&shared, &page),
            usb: usb::Usb::new(&shared, &page),
            _token: token::Token::new(&shared, &page),
            _gpu_preview: gpu_preview::GpuPreview::new(&shared, &page),
            _jail: jail::Jail::new(&shared, &page),
            linux_gpu: linux_gpu::LinuxGpu::new(&shared, &page),
            remove: remove::Remove::new(&shared, &page),
            shared,
        }));
        let window = window.clone();
        this.0.shared.dialog.connect_closed(move |_| window.settings_closed());
        this
    }

    pub fn machine_id(&self) -> String {
        self.0.shared.machine.borrow().id.clone()
    }

    pub fn present(&self) {
        self.0.shared.dialog.present(Some(self.0.shared.window.widget()));
    }

    pub fn set_status(&self, status: &Status) {
        let parts = &self.0;
        parts.hardware.set_status(status);
        parts.display.set_status(status);
        parts.disk.set_status(status);
        parts.snapshots.set_status(status);
        parts.sharing.set_status(status);
        parts.network.set_status(status);
        parts.usb.set_status(status);
        parts.linux_gpu.set_status(status);
        parts.remove.set_status(status);
    }
}
