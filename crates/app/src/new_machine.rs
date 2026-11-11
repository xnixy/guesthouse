//! The dialog that makes a machine from an ISO. Looking inside the ISO decides what it will be.

use crate::common::{choose_file, in_background};
use crate::driver_row::DriverRow;
use crate::rows::{choice_row, number_row, row_button};
use crate::window::Window;
use adw::prelude::*;
use guesthouse_app_core::account::{default_user, name_problem, password_problem};
use guesthouse_app_core::card_choice::{CardChoice, default_choice, windows_card_choices};
use guesthouse_app_core::display::DriverPackage;
use guesthouse_app_core::gpu_maker::GpuMaker;
use guesthouse_app_core::gpu_support::{kind_offered, plain_reason, ubuntu_note};
use guesthouse_app_core::host::{RamField, physical_cores};
use guesthouse_app_core::iso::{Iso, read_iso};
use guesthouse_app_core::library::{
    MAX_DISK_GIB, MIN_PLAIN_DISK_GIB, MIN_WINDOWS_DISK_GIB, NewMachine, create,
};
use guesthouse_app_core::machine::{Kind, MIN_CORES, MIN_RAM_GIB, Machine};
use guesthouse_app_core::setup::amd::NO_AMD_SETUP;
use guesthouse_app_core::setup::checks::gpu_warning;
use guesthouse_app_core::setup::guest_driver::guest_driver;
use guesthouse_app_core::setup::intel::NO_INTEL_SETUP;
use guesthouse_app_core::tasks::no_slot_text;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

const NO_GPU: &str = "This machine gets no GPU acceleration. It runs on the CPU, with a basic display.";
const DISPLAY_DRIVER_NOTE: &str =
    "Guesthouse adds its own display driver to Windows, signed with a certificate made on this machine.";
const WINDOWS_ACCOUNT_NOTE: &str = "Windows signs in by itself. The password is for permission prompts and Remote \
                                    Desktop. Leave it empty for a random one, kept in the machine's folder.";
const LINUX_ACCOUNT_NOTE: &str = "For a console login if the desktop won't start. Leave the password empty for a \
                                  random one, kept in the machine's folder.";
const WIDTH: i32 = 520;
const HEIGHT: i32 = 720;
const NOTE_MARGIN: i32 = 12;
const CARD_TITLE: &str = "Card";
/// Said plainly until a guest on an AMD card has drawn with it.
const AMD_NOT_FINISHED: &str = "starts on the AMD card. GPU-PV on AMD is not finished, so Windows may have no \
                                GPU there yet";
/// Said plainly, as Intel's support only logs what the guest's driver asks for.
const INTEL_NOT_FINISHED: &str = "starts on the Intel card. Intel support is at an early test stage: Windows runs, but nothing is drawn by the card yet";
const CARD_NOTE: &str = "Windows is installed for this card's maker. It can move to another card of the same maker \
                         later, but not to another maker's.";

/// Sizes to start from. A plain machine is usually something small being tried out.
struct Sizes {
    cores: u32,
    ram: u64,
    disk: u32,
}

const WINDOWS_SIZES: Sizes = Sizes { cores: 8, ram: 16, disk: 128 };
const PLAIN_SIZES: Sizes = Sizes { cores: 4, ram: 4, disk: 32 };
const UBUNTU_SIZES: Sizes = Sizes { cores: 8, ram: 8, disk: 80 };

struct Parts {
    window: Window,
    dialog: adw::Dialog,
    create_button: gtk::Button,
    iso_row: adw::ActionRow,
    verdict: adw::ActionRow,
    hardware: adw::PreferencesGroup,
    name: adw::EntryRow,
    account: adw::PreferencesGroup,
    user: adw::EntryRow,
    password: adw::PasswordEntryRow,
    cores: adw::SpinRow,
    ram: adw::SpinRow,
    disk: adw::SpinRow,
    /// Which card a Windows machine runs on, there only when this PC has more than one to choose.
    card: adw::ComboRow,
    cards: Vec<CardChoice>,
    driver_row: DriverRow,
    note: gtk::Label,
    iso_path: RefCell<Option<String>>,
    iso: RefCell<Option<Iso>>,
    /// The name last offered, to tell it from one the user typed.
    suggested: RefCell<String>,
    no_driver: Cell<bool>,
    /// This PC can't give the machine the RAM it needs, which the RAM setting says.
    no_room: Cell<bool>,
    vm_running: Cell<bool>,
}

fn suggested_name(kind: Kind, path: &str) -> String {
    match kind {
        Kind::Windows => "Windows 11".to_owned(),
        Kind::Linux => "Ubuntu".to_owned(),
        Kind::Plain => {
            Path::new(path).file_stem().map_or_else(String::new, |name| name.to_string_lossy().into_owned())
        }
    }
}

/// A name the user typed stays when they choose another disc. One this dialog offered goes with its
/// disc, or a machine ends up named after the disc they changed their mind about.
fn keeps_following_the_disc(name: &str, suggested: &str) -> bool {
    name.is_empty() || name == suggested
}

#[derive(Clone)]
pub struct NewMachineDialog(Rc<Parts>);

/// The name as the machine keeps it: empty for the default, so a machine made with the default reads
/// the same as one from before the choice.
fn account_name(kind: Kind, typed: &str) -> String {
    if typed == default_user(kind) { String::new() } else { typed.to_owned() }
}

/// A row with a problem says so, in red and in its tooltip.
fn mark_problem(row: &gtk::Widget, problem: Option<&str>) {
    if problem.is_some() {
        row.add_css_class("error");
    } else {
        row.remove_css_class("error");
    }
    row.set_tooltip_text(problem);
}

fn hardware_row(group: &adw::PreferencesGroup, title: &str, lowest: f64, highest: f64) -> adw::SpinRow {
    let row = number_row(title, lowest, lowest, highest);
    group.add(&row);
    row
}

impl NewMachineDialog {
    pub fn new(window: &Window) -> Self {
        let context = window.context();
        let create_button = gtk::Button::builder().label("Create").sensitive(false).build();
        create_button.add_css_class("suggested-action");
        let cancel = gtk::Button::with_label("Cancel");
        let header =
            adw::HeaderBar::builder().show_end_title_buttons(false).show_start_title_buttons(false).build();
        header.pack_start(&cancel);
        header.pack_end(&create_button);
        let page = adw::PreferencesPage::new();
        let disc = adw::PreferencesGroup::new();
        page.add(&disc);
        let iso_row =
            adw::ActionRow::builder().title("ISO").subtitle("Choose the disc to install from.").build();
        let choose = row_button("Choose");
        iso_row.add_suffix(&choose);
        disc.add(&iso_row);
        let verdict = adw::ActionRow::builder().visible(false).activatable(false).build();
        disc.add(&verdict);
        let hardware = adw::PreferencesGroup::new();
        hardware.set_sensitive(false);
        page.add(&hardware);
        let name = adw::EntryRow::builder().title("Name").build();
        hardware.add(&name);
        let cores = hardware_row(&hardware, "Cores", 1.0, physical_cores(&context.cpus).max(1).into());
        let ram = hardware_row(&hardware, "RAM in GiB", 1.0, 1.0);
        let disk = hardware_row(&hardware, "Disk in GiB", MIN_PLAIN_DISK_GIB.into(), MAX_DISK_GIB.into());
        let account = adw::PreferencesGroup::builder().title("Account").visible(false).build();
        page.add(&account);
        let user = adw::EntryRow::builder().title("User name").build();
        account.add(&user);
        let password = adw::PasswordEntryRow::builder().title("Password").build();
        account.add(&password);
        let cards = windows_card_choices(&context);
        let labels: Vec<&str> = cards.iter().map(|choice| choice.label.as_str()).collect();
        let card = choice_row(CARD_TITLE, &labels);
        card.set_subtitle(CARD_NOTE);
        card.set_selected(default_choice(&context, &cards) as u32);
        card.set_visible(false);
        hardware.add(&card);
        let driver_row = DriverRow::new(window, &hardware);
        let note = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .margin_start(NOTE_MARGIN)
            .margin_end(NOTE_MARGIN)
            .margin_bottom(NOTE_MARGIN)
            .build();
        note.add_css_class("dim-label");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&page);
        column.append(&note);
        page.set_vexpand(true);
        let toolbar = adw::ToolbarView::new();
        toolbar.set_content(Some(&column));
        toolbar.add_top_bar(&header);
        let dialog = adw::Dialog::builder()
            .title("New Machine")
            .content_width(WIDTH)
            .content_height(HEIGHT)
            .child(&toolbar)
            .build();
        let this = Self(Rc::new(Parts {
            window: window.clone(),
            dialog,
            create_button,
            iso_row,
            verdict,
            hardware,
            name,
            account,
            user,
            password,
            cores,
            ram,
            disk,
            card,
            cards,
            driver_row,
            note,
            iso_path: RefCell::default(),
            iso: RefCell::default(),
            suggested: RefCell::default(),
            no_driver: Cell::new(false),
            no_room: Cell::new(false),
            vm_running: Cell::new(false),
        }));
        this.set_vm_running(window.vm_running());
        this.connect(&cancel, &choose);
        this
    }

    fn connect(&self, cancel: &gtk::Button, choose: &gtk::Button) {
        let this = self.clone();
        self.0.name.connect_changed(move |_| this.update());
        let this = self.clone();
        self.0.user.connect_changed(move |_| this.update());
        let this = self.clone();
        self.0.password.connect_changed(move |_| this.update());
        let dialog = self.0.dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
        let this = self.clone();
        choose.connect_clicked(move |_| this.on_choose());
        let this = self.clone();
        self.0.create_button.connect_clicked(move |_| this.on_create());
        let this = self.clone();
        self.0.card.connect_selected_notify(move |_| this.card_changed());
        let this = self.clone();
        self.0.driver_row.connect_ready(move || this.card_changed());
        let window = self.0.window.clone();
        let driver_row = self.0.driver_row.clone();
        self.0.dialog.connect_closed(move |_| {
            driver_row.stop();
            window.new_machine_closed();
        });
    }

    pub fn present(&self) {
        self.0.dialog.present(Some(self.0.window.widget()));
    }

    pub fn set_vm_running(&self, running: bool) {
        self.0.vm_running.set(running);
        let note = if running {
            format!("{} Shut one down first.", no_slot_text(&self.0.window.context()))
        } else {
            String::new()
        };
        self.0.note.set_label(&note);
        self.update();
    }

    fn on_choose(&self) {
        let window = self.0.window.clone();
        let failing = window.clone();
        let this = self.clone();
        choose_file(
            window.widget(),
            "Choose an ISO",
            "Disc images",
            "iso",
            move |path| this.read_iso(path),
            move |text| failing.problem(&text),
        );
    }

    /// Takes this ISO as if it had been chosen. It's public for the page checks, where no file dialog
    /// can be answered.
    pub fn read_iso(&self, path: String) {
        *self.0.iso_path.borrow_mut() = Some(path.clone());
        *self.0.iso.borrow_mut() = None;
        self.0.iso_row.set_subtitle(&path);
        self.0.hardware.set_sensitive(false);
        self.show_verdict("Reading the ISO", "");
        self.update();
        let context = self.0.window.context();
        let this = self.clone();
        let reading = path.clone();
        in_background(
            &context,
            move || read_iso(&reading),
            move |result| {
                // Another choice may have come since. Only the newest counts.
                if this.0.iso_path.borrow().as_deref() != Some(&path) {
                    return;
                }
                match result {
                    Ok(iso) => this.show_iso(&path, iso),
                    Err(text) => this.show_verdict(&text, ""),
                }
            },
        );
    }

    fn show_verdict(&self, title: &str, subtitle: &str) {
        let verdict = &self.0.verdict;
        verdict.set_visible(true);
        verdict.set_title(&gtk::glib::markup_escape_text(title));
        verdict.set_subtitle(&gtk::glib::markup_escape_text(subtitle));
    }

    /// `reason` says why this disc is only a plain VM here, if it is.
    fn describe(&self, iso: &Iso, reason: &str) {
        let context = self.0.window.context();
        let windows = iso.kind == Kind::Windows;
        if iso.kind == Kind::Linux {
            self.show_verdict(&iso.title, ubuntu_note(&context));
            return;
        }
        let missing_driver = if windows { guest_driver(&context, &self.chosen_card()).err() } else { None };
        self.0.no_driver.set(missing_driver.is_some());
        // The maker whose installer the row asks for, when it's that driver that is missing.
        let installer_for = match missing_driver {
            Some(missing) if missing == NO_AMD_SETUP => Some(GpuMaker::Amd),
            Some(missing) if missing == NO_INTEL_SETUP => Some(GpuMaker::Intel),
            _ => None,
        };
        self.0.driver_row.show_for(installer_for.map(|maker| (maker, self.chosen_device())));
        if let Some(missing) = missing_driver {
            self.show_verdict(&iso.title, missing);
        } else if windows {
            // The warning is about NVIDIA's card and setup.
            let maker = self.chosen_choice().map_or(GpuMaker::Nvidia, |choice| choice.maker);
            let on_nvidia = maker == GpuMaker::Nvidia;
            let warning = gpu_warning(&context)
                .filter(|_| on_nvidia)
                .map_or_else(String::new, |text| format!(" {text}"));
            // A build from source has no driver to add, so it says nothing about one.
            let display = DriverPackage::find(&context).map_or("", |_| DISPLAY_DRIVER_NOTE);
            let gpu = match maker {
                GpuMaker::Nvidia => "starts with GPU-PV",
                GpuMaker::Amd => AMD_NOT_FINISHED,
                GpuMaker::Intel => INTEL_NOT_FINISHED,
            };
            let said = format!("Installs by itself and {gpu}. {display}{warning}");
            self.show_verdict(&iso.title, said.trim());
        } else {
            self.show_verdict(
                &format!("{}: a plain VM", iso.title),
                format!("You install it by hand. {NO_GPU} {reason}").trim(),
            );
        }
    }

    fn set_sizes(&self, kind: Kind) {
        let context = self.0.window.context();
        let windows = kind == Kind::Windows;
        let sizes = match kind {
            Kind::Windows => WINDOWS_SIZES,
            Kind::Linux => UBUNTU_SIZES,
            Kind::Plain => PLAIN_SIZES,
        };
        let field = if windows {
            RamField::for_gpu(&context, context.slot, MIN_RAM_GIB.into(), sizes.ram)
        } else {
            RamField::for_plain(&context, 1, sizes.ram)
        };
        let parts = &self.0;
        let cores = physical_cores(&context.cpus);
        parts.cores.adjustment().set_lower(if windows { MIN_CORES.into() } else { 1.0 });
        parts.cores.set_value(sizes.cores.min(cores).into());
        parts.ram.adjustment().set_lower(field.lower as f64);
        parts.ram.adjustment().set_upper(field.upper as f64);
        parts.ram.set_value(field.value as f64);
        parts.ram.set_sensitive(field.fits);
        parts.ram.set_subtitle(&field.subtitle);
        parts.no_room.set(!field.fits);
        let least = if windows { MIN_WINDOWS_DISK_GIB } else { MIN_PLAIN_DISK_GIB };
        parts.disk.adjustment().set_lower(least.into());
        parts.disk.set_value(sizes.disk.into());
    }

    fn show_iso(&self, path: &str, iso: Iso) {
        let context = self.0.window.context();
        let reason = plain_reason(&context, iso.kind);
        let iso = Iso { kind: kind_offered(&context, iso.kind), ..iso };
        self.describe(&iso, reason);
        self.set_sizes(iso.kind);
        self.0.card.set_visible(iso.kind == Kind::Windows && self.0.cards.len() > 1);
        let suggestion = suggested_name(iso.kind, path);
        if keeps_following_the_disc(&self.0.name.text(), &self.0.suggested.borrow()) {
            self.0.name.set_text(&suggestion);
        }
        *self.0.suggested.borrow_mut() = suggestion;
        self.show_account(iso.kind);
        *self.0.iso.borrow_mut() = Some(iso);
        self.0.hardware.set_sensitive(true);
        self.update();
    }

    /// The account rows, for a disc that installs by itself. The name starts as the one the guest
    /// scripts have always used, and a name the user typed stays through a change of disc.
    fn show_account(&self, kind: Kind) {
        let parts = &self.0;
        let note = match kind {
            Kind::Windows => WINDOWS_ACCOUNT_NOTE,
            Kind::Linux => LINUX_ACCOUNT_NOTE,
            Kind::Plain => {
                parts.account.set_visible(false);
                return;
            }
        };
        let typed = parts.user.text();
        let was_default = [default_user(Kind::Windows), default_user(Kind::Linux)].contains(&typed.as_str());
        if typed.is_empty() || was_default {
            parts.user.set_text(default_user(kind));
        }
        parts.account.set_description(Some(note));
        parts.account.set_visible(true);
    }

    /// Says on the row what's wrong with its text, if anything, and whether something is.
    fn account_problems(&self) -> bool {
        let parts = &self.0;
        let Some(kind) = parts.iso.borrow().as_ref().map(|iso| iso.kind) else { return false };
        if kind == Kind::Plain {
            return false;
        }
        let name = name_problem(kind, parts.user.text().trim());
        let password = password_problem(&parts.password.text());
        mark_problem(parts.user.upcast_ref(), name.as_deref());
        mark_problem(parts.password.upcast_ref(), password.as_deref());
        name.is_some() || password.is_some()
    }

    /// The PCI address of the card chosen, empty for NVIDIA's, as a machine keeps it.
    fn chosen_card_pci(&self) -> String {
        self.chosen_choice().map(|choice| choice.pci.clone()).unwrap_or_default()
    }

    fn chosen_choice(&self) -> Option<&CardChoice> {
        self.0.cards.get(self.0.card.selected() as usize)
    }

    fn chosen_device(&self) -> u32 {
        self.0.cards.get(self.0.card.selected() as usize).map_or(0, |choice| choice.device)
    }

    /// A Windows machine on the card chosen, to ask what it would need.
    fn chosen_card(&self) -> Machine {
        let mut machine = Machine::new("", "", "", "");
        if let Some(choice) = self.chosen_choice() {
            machine.use_windows_card(choice.maker, &choice.pci);
        }
        machine
    }

    /// Another card can need another maker's driver, which may not be here.
    fn card_changed(&self) {
        let Some(iso) = self.0.iso.borrow().clone() else { return };
        let context = self.0.window.context();
        self.describe(&iso, plain_reason(&context, iso.kind));
        self.update();
    }

    fn update(&self) {
        let parts = &self.0;
        let ready = parts.iso.borrow().is_some()
            && !parts.name.text().trim().is_empty()
            && !self.account_problems()
            && !parts.vm_running.get()
            && !parts.no_driver.get()
            && !parts.no_room.get();
        parts.create_button.set_sensitive(ready);
    }

    fn on_create(&self) {
        let parts = &self.0;
        let (Some(iso), Some(path)) = (parts.iso.borrow().clone(), parts.iso_path.borrow().clone()) else {
            return;
        };
        parts.create_button.set_sensitive(false);
        let new = NewMachine {
            name: parts.name.text().to_string(),
            iso: path,
            kind: iso.kind,
            cores: parts.cores.value() as u32,
            ram_gib: parts.ram.value() as u32,
            disk_gib: parts.disk.value() as u32,
            user: account_name(iso.kind, parts.user.text().trim()),
            password: parts.password.text().to_string(),
            amd_gpu: (iso.kind == Kind::Windows).then(|| self.chosen_card_pci()),
            gpu_maker: self.chosen_choice().map(|choice| choice.maker),
        };
        let context = parts.window.context();
        let working = context.clone();
        let this = self.clone();
        in_background(
            &context,
            move || create(&working, &new),
            move |result| match result {
                Ok(machine) => {
                    let window = &this.0.window;
                    let action = if machine.installing { "install" } else { "start" };
                    window.add_machine(machine.clone());
                    window.launch(action, &machine, &[], None);
                    this.0.dialog.close();
                }
                Err(text) => {
                    this.0.window.problem(&text);
                    this.update();
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggested_name_goes_with_its_disc() {
        assert!(keeps_following_the_disc("", ""));
        assert!(keeps_following_the_disc("win11-eval", "win11-eval"));
    }

    #[test]
    fn a_typed_name_stays() {
        assert!(!keeps_following_the_disc("My VM", "win11-eval"));
        assert!(!keeps_following_the_disc("My VM", ""));
    }

    #[test]
    fn the_default_account_is_kept_as_no_choice() {
        assert_eq!(account_name(Kind::Windows, "gpupv"), "");
        assert_eq!(account_name(Kind::Linux, "desk"), "");
        assert_eq!(account_name(Kind::Windows, "ana"), "ana");
        assert_eq!(account_name(Kind::Linux, "gpupv"), "gpupv");
    }

    #[test]
    fn each_kind_of_disc_has_its_name() {
        assert_eq!(suggested_name(Kind::Windows, "/discs/win.iso"), "Windows 11");
        assert_eq!(suggested_name(Kind::Linux, "/discs/server.iso"), "Ubuntu");
        assert_eq!(suggested_name(Kind::Plain, "/discs/tiny-linux.iso"), "tiny-linux");
    }
}
