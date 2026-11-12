//! A machine's settings, on a few pages so no one page is a long scroll. Each group of rows is a module
//! of its own, and gets the same three things: the window, the machine, and this dialog to put its own
//! dialogs over.

mod account;
mod disk;
mod display;
mod drives;
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
mod windows_card;

use crate::window::{MachineHandle, Window};
use adw::prelude::*;
use guesthouse_app_core::machine::Kind;
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

/// The pages, in the order the dialog shows them.
struct Pages {
    general: adw::PreferencesPage,
    display: adw::PreferencesPage,
    storage: adw::PreferencesPage,
    network: adw::PreferencesPage,
    devices: adw::PreferencesPage,
}

impl Pages {
    fn new(dialog: &adw::PreferencesDialog, kind: Kind) -> Self {
        let add = |name: &str, title: &str, icon: &str| {
            let page = adw::PreferencesPage::builder().name(name).title(title).icon_name(icon).build();
            dialog.add(&page);
            page
        };
        let pages = Self {
            general: add("general", "General", "computer-symbolic"),
            display: add("display", "Display", "video-display-symbolic"),
            storage: add("storage", "Storage", "drive-harddisk-symbolic"),
            network: add("network", "Network", "network-wired-symbolic"),
            devices: add("devices", "Devices", "media-removable-symbolic"),
        };
        // Ubuntu's network is fixed, so its page would be empty.
        pages.network.set_visible(kind != Kind::Linux);
        pages
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
    drives: drives::Drives,
    snapshots: snapshots::Snapshots,
    _token: token::Token,
    _account: account::Account,
    _gpu_preview: gpu_preview::GpuPreview,
    _jail: jail::Jail,
    linux_gpu: linux_gpu::LinuxGpu,
    windows_card: windows_card::WindowsCard,
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
        let pages = Pages::new(&dialog, machine.borrow().kind);
        let shared = Shared { window: window.clone(), machine, dialog };
        let this = Self(Rc::new(Parts {
            hardware: hardware::Hardware::new(&shared, &pages.general),
            windows_card: windows_card::WindowsCard::new(&shared, &pages.general),
            _gpu_preview: gpu_preview::GpuPreview::new(&shared, &pages.general),
            linux_gpu: linux_gpu::LinuxGpu::new(&shared, &pages.general),
            _account: account::Account::new(&shared, &pages.general),
            _token: token::Token::new(&shared, &pages.general),
            _jail: jail::Jail::new(&shared, &pages.general),
            remove: remove::Remove::new(&shared, &pages.general),
            display: display::Display::new(&shared, &pages.display),
            disk: disk::Disk::new(&shared, &pages.storage),
            drives: drives::Drives::new(&shared, &pages.storage),
            snapshots: snapshots::Snapshots::new(&shared, &pages.storage),
            network: network::Network::new(&shared, &pages.network),
            sharing: sharing::Sharing::new(&shared, &pages.devices),
            usb: usb::Usb::new(&shared, &pages.devices),
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
        parts.drives.set_status(status);
        parts.snapshots.set_status(status);
        parts.sharing.set_status(status);
        parts.network.set_status(status);
        parts.usb.set_status(status);
        parts.linux_gpu.set_status(status);
        parts.windows_card.set_status(status);
        parts.remove.set_status(status);
    }
}
