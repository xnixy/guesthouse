//! The drives of this PC that the guest has beside its disk.

use super::Shared;
use crate::common::in_background;
use crate::rows::{icon_button, row_button};
use crate::settings::network::show_root_step;
use adw::prelude::*;
use guesthouse_app_core::host_drives::{
    HostDrive, add_drive, can_open, drive_note, host_drives, remove_drive, why_not_offered,
};
use guesthouse_app_core::machine::{Drive, Kind, MAX_DRIVES, Machine};
use guesthouse_app_core::network_settings::other_machines;
use guesthouse_app_core::tasks::{State, Status};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const WIDTH: i32 = 480;
const HEIGHT: i32 = 560;
const HELP: &str = "Whole disks of this PC. The guest can read and write all of one, and this PC can't use \
                    it while the machine runs.";
const INSTALLING: &str = "Drives can be added once Windows is installed.";
const NEXT_START: &str = "Changes apply the next time the VM starts.";
const AT_MOST: &str = "A machine can have 3 drives at most.";
const ACCESS_AFTER: &str =
    "After this, every program of yours can read and write this drive, not only the VM.";

/// What the PC has, read off the GTK thread.
#[derive(Default)]
struct View {
    listed: Vec<HostDrive>,
    /// Why a drive of this machine can't be used now, by its name.
    notes: HashMap<String, String>,
    /// The drives of this machine that this user can't open.
    closed: HashSet<String>,
}

struct Parts {
    shared: Shared,
    group: adw::PreferencesGroup,
    add: gtk::Button,
    rows: RefCell<Vec<adw::ActionRow>>,
    view: RefCell<View>,
    running: Cell<bool>,
}

#[derive(Clone)]
pub struct Drives(Rc<Parts>);

impl Drives {
    pub fn new(shared: &Shared, page: &adw::PreferencesPage) -> Self {
        let group = adw::PreferencesGroup::builder()
            .title("Drives of This PC")
            .description(HELP)
            // Ubuntu's machine has no controller to put them on.
            .visible(shared.machine.borrow().kind != Kind::Linux)
            .build();
        page.add(&group);
        let add = row_button("Add Drive");
        group.set_header_suffix(Some(&add));
        let this = Self(Rc::new(Parts {
            shared: shared.clone(),
            group,
            add,
            rows: RefCell::default(),
            view: RefCell::default(),
            running: Cell::new(false),
        }));
        let handle = this.clone();
        this.0.add.connect_clicked(move |_| handle.on_add());
        this.read();
        this
    }

    /// Looks at the PC, then shows the machine's drives with what it found.
    fn read(&self) {
        let context = self.0.shared.window.context();
        let machine = self.0.shared.machine.borrow().clone();
        let reading = context.clone();
        let this = self.clone();
        in_background(
            &context,
            move || {
                let others = other_machines(&reading);
                let mut view = View { listed: host_drives(&reading), ..View::default() };
                for drive in &machine.drives {
                    if let Some(note) = drive_note(&reading, drive, &view.listed, &others) {
                        view.notes.insert(drive.id.clone(), note);
                    }
                    if !can_open(&reading, drive) && reading.drive_ids.join(&drive.id).exists() {
                        view.closed.insert(drive.id.clone());
                    }
                }
                Ok(view)
            },
            move |result| match result {
                Ok(view) => {
                    *this.0.view.borrow_mut() = view;
                    this.show();
                }
                Err(text) => this.0.shared.window.problem(&text),
            },
        );
    }

    fn show(&self) {
        let parts = &self.0;
        for row in parts.rows.borrow_mut().drain(..) {
            parts.group.remove(&row);
        }
        let drives = parts.shared.machine.borrow().drives.clone();
        let rows: Vec<adw::ActionRow> = drives.iter().map(|drive| self.row_for(drive)).collect();
        for row in &rows {
            parts.group.add(row);
        }
        *parts.rows.borrow_mut() = rows;
        self.set_add();
    }

    fn row_for(&self, drive: &Drive) -> adw::ActionRow {
        let view = self.0.view.borrow();
        let host = view.listed.iter().find(|host| host.id == drive.id);
        let mut lines = vec![drive.id.clone()];
        lines.extend(view.notes.get(&drive.id).cloned());
        let row = adw::ActionRow::builder()
            .title(host.map_or_else(|| drive.id.clone(), HostDrive::title))
            .subtitle(lines.join("\n"))
            .subtitle_lines(4)
            .use_markup(false)
            .build();
        if view.closed.contains(&drive.id) {
            let access = row_button("Give Access");
            let this = self.clone();
            let chosen = drive.id.clone();
            access.connect_clicked(move |_| this.on_access(&chosen));
            row.add_suffix(&access);
        }
        let remove = icon_button("user-trash-symbolic", "Remove Drive");
        let this = self.clone();
        let chosen = drive.id.clone();
        remove.connect_clicked(move |_| {
            let shared = &this.0.shared;
            shared.change(|machine| remove_drive(&shared.window.context(), machine, &chosen));
            this.read();
        });
        row.add_suffix(&remove);
        row
    }

    /// Whether another can be added, and if not, why.
    fn set_add(&self) {
        let parts = &self.0;
        let machine = parts.shared.machine.borrow();
        let full = machine.drives.len() >= MAX_DRIVES;
        parts.add.set_sensitive(!full && !machine.installing);
        let reason = if machine.installing {
            INSTALLING
        } else if full {
            AT_MOST
        } else {
            ""
        };
        parts.add.set_tooltip_text(Some(reason).filter(|text| !text.is_empty()));
    }

    fn on_access(&self, id: &str) {
        let shared = &self.0.shared;
        let view = self.0.view.borrow();
        let Some(host) = view.listed.iter().find(|host| host.id == id) else { return };
        match host.access_command() {
            Ok(commands) => {
                let body = format!(
                    "This user cannot open {}. Run this once in a terminal, then start the machine again. \
                     It lets whoever is logged in on this PC use this drive. {ACCESS_AFTER}",
                    host.title()
                );
                show_root_step(shared, &body, Some(&commands));
            }
            Err(failure) => shared.window.problem(&failure.0),
        }
    }

    /// The list of what the PC has, read now, to choose one from.
    fn on_add(&self) {
        let context = self.0.shared.window.context();
        let machine = self.0.shared.machine.borrow().clone();
        let reading = context.clone();
        let this = self.clone();
        in_background(
            &context,
            move || Ok((host_drives(&reading), other_machines(&reading))),
            move |result| match result {
                Ok((listed, others)) => this.present_list(&listed, &others, &machine),
                Err(text) => this.0.shared.window.problem(&text),
            },
        );
    }

    fn present_list(&self, listed: &[HostDrive], others: &[Machine], machine: &Machine) {
        let group = adw::PreferencesGroup::builder()
            .description("Drives this PC is using, and ones another machine has, can't be given.")
            .build();
        let dialog =
            adw::Dialog::builder().title("Add a Drive").content_width(WIDTH).content_height(HEIGHT).build();
        for drive in listed {
            let reason = why_not_offered(drive, machine, others);
            let mut lines = vec![drive.id.clone()];
            if drive.removable {
                lines.push("Removable".to_owned());
            }
            lines.extend(reason.clone());
            let row = adw::ActionRow::builder()
                .title(drive.title())
                .subtitle(lines.join("\n"))
                .subtitle_lines(4)
                .use_markup(false)
                .activatable(reason.is_none())
                .sensitive(reason.is_none())
                .build();
            let (this, chosen, closing) = (self.clone(), drive.id.clone(), dialog.clone());
            row.connect_activated(move |_| {
                closing.close();
                let shared = &this.0.shared;
                let drive = Drive { id: chosen.clone() };
                shared.change(|machine| add_drive(&shared.window.context(), machine, drive));
                this.read();
            });
            group.add(&row);
        }
        if listed.is_empty() {
            group.add(&adw::ActionRow::builder().title("No drive found").build());
        }
        let page = adw::PreferencesPage::new();
        page.add(&group);
        let content = adw::ToolbarView::new();
        content.add_top_bar(&adw::HeaderBar::new());
        content.set_content(Some(&page));
        dialog.set_child(Some(&content));
        dialog.present(Some(&self.0.shared.dialog));
    }

    pub fn set_status(&self, status: &Status) {
        let parts = &self.0;
        let running = status.state != State::Off || status.busy;
        let installing = parts.shared.machine.borrow().installing;
        let text = if installing {
            INSTALLING.to_owned()
        } else if running {
            format!("{HELP} {NEXT_START}")
        } else {
            HELP.to_owned()
        };
        parts.group.set_description(Some(&text));
        if parts.running.replace(running) != running {
            self.read();
        }
        self.set_add();
    }
}
