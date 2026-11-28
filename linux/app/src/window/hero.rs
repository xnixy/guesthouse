//! The chosen machine at length, at the top of the library: its last screen behind its name, what
//! it is, what there is to say about it, and everything that can be done to it now.

use super::machine_card::{MachineCard, SCREEN_BUSY, SCREEN_LABEL, chip};
use crate::rows::{icon_button, row_button};
use adw::prelude::*;
use guesthouse_app_core::app_windows::open_windows::OpenProgram;
use guesthouse_app_core::tasks::State;
use std::cell::RefCell;
use std::rc::Rc;

const PADDING: i32 = 32;
const SPACING: i32 = 16;
const CHIP_SPACING: i32 = 8;
/// Tall enough to read as the page's main thing with nothing but a name in it.
const LEAST_HEIGHT: i32 = 340;
/// The same beside a sidebar, so it fits a window half the screen wide.
const COMPACT_PADDING: i32 = 20;
const COMPACT_LEAST_HEIGHT: i32 = 236;

struct Parts {
    root: gtk::Overlay,
    body: gtk::Box,
    picture: gtk::Picture,
    spinner: gtk::Spinner,
    dot: gtk::Box,
    state: gtk::Label,
    name: gtk::Label,
    kind: gtk::Label,
    specs: gtk::FlowBox,
    notes: gtk::Label,
    /// What is open as app windows: a chip for each program, and the way to open another.
    apps: gtk::FlowBox,
    open_app: gtk::Button,
    open: RefCell<Vec<OpenProgram>>,
    start: gtk::Button,
    show: gtk::Button,
    stop: gtk::Button,
    force: gtk::Button,
    card: RefCell<Option<MachineCard>>,
}

#[derive(Clone)]
pub struct Hero(Rc<Parts>);

/// A row of things that goes on to the next line when the window is narrow.
fn wrapping_row() -> gtk::FlowBox {
    let row = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(CHIP_SPACING as u32)
        .row_spacing(CHIP_SPACING as u32)
        .max_children_per_line(8)
        .halign(gtk::Align::Start)
        .build();
    row.add_css_class("hero-row");
    row
}

/// Adds to a wrapping row, where a child must not be something the keyboard stops on.
fn add_to(row: &gtk::FlowBox, widget: &impl IsA<gtk::Widget>) {
    row.append(widget);
    if let Some(holder) = widget.parent() {
        holder.set_focusable(false);
    }
}

/// Shows or hides a thing in a wrapping row, with the place the row keeps for it.
fn set_shown(widget: &impl IsA<gtk::Widget>, shown: bool) {
    widget.set_visible(shown);
    if let Some(holder) = widget.parent() {
        holder.set_visible(shown);
    }
}

fn left_label(style: &str) -> gtk::Label {
    let label = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    label.add_css_class(style);
    label
}

impl Hero {
    pub fn new() -> Self {
        let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).can_shrink(true).build();
        let scrim = gtk::Box::new(gtk::Orientation::Vertical, 0);
        scrim.add_css_class("hero-scrim");
        let spinner = gtk::Spinner::builder().spinning(true).build();
        let dot = gtk::Box::builder().valign(gtk::Align::Center).build();
        dot.add_css_class("state-dot");
        let state = gtk::Label::new(None);
        let chip = gtk::Box::builder().spacing(CHIP_SPACING).halign(gtk::Align::Start).build();
        chip.add_css_class("state-chip");
        chip.append(&spinner);
        chip.append(&dot);
        chip.append(&state);
        let name = left_label("hero-title");
        let kind = left_label("hero-kind");
        let specs = wrapping_row();
        let notes = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).build();
        notes.add_css_class("hero-notes");
        let apps = wrapping_row();
        apps.add_css_class("app-chips");
        apps.set_visible(false);
        let open_app = row_button("Open Another App");
        open_app.add_css_class("flat");
        let actions = wrapping_row();
        let start = row_button("Start");
        start.add_css_class("suggested-action");
        let show = row_button(SCREEN_LABEL);
        show.add_css_class("suggested-action");
        let stop = row_button("Shut Down");
        let force = row_button("Force Off");
        force.add_css_class("destructive-action");
        let settings = icon_button("emblem-system-symbolic", "Settings");
        for button in [&start, &show, &stop, &force, &settings] {
            add_to(&actions, button);
        }
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(SPACING)
            .valign(gtk::Align::End)
            .margin_top(PADDING)
            .margin_bottom(PADDING)
            .margin_start(PADDING)
            .margin_end(PADDING)
            .build();
        for part in [chip.upcast_ref::<gtk::Widget>(), name.upcast_ref(), kind.upcast_ref()] {
            body.append(part);
        }
        for part in
            [specs.upcast_ref::<gtk::Widget>(), apps.upcast_ref(), notes.upcast_ref(), actions.upcast_ref()]
        {
            body.append(part);
        }
        let root = gtk::Overlay::builder()
            .child(&picture)
            .overflow(gtk::Overflow::Hidden)
            .height_request(LEAST_HEIGHT)
            .build();
        root.add_css_class("hero");
        // Nothing to show until a machine is chosen.
        root.set_visible(false);
        root.add_overlay(&scrim);
        root.add_overlay(&body);
        // The words decide the height. The picture only fills what they take.
        root.set_measure_overlay(&body, true);
        let parts = Parts {
            root,
            body,
            picture,
            spinner,
            dot,
            state,
            name,
            kind,
            specs,
            notes,
            apps,
            open_app,
            open: RefCell::default(),
            start,
            show,
            stop,
            force,
            card: RefCell::default(),
        };
        let this = Self(Rc::new(parts));
        this.connect(&this.0.start, MachineCard::on_start);
        this.connect(&this.0.show, MachineCard::show_screen);
        this.connect(&this.0.stop, MachineCard::shut_down);
        this.connect(&this.0.force, MachineCard::on_force);
        this.connect(&settings, MachineCard::show_settings);
        this.connect(&this.0.open_app, MachineCard::open_apps_page);
        this
    }

    /// A button does its thing to whichever machine is chosen when it's pressed.
    fn connect(&self, button: &gtk::Button, action: fn(&MachineCard)) {
        let this = self.clone();
        button.connect_clicked(move |_| {
            let card = this.0.card.borrow().clone();
            if let Some(card) = card {
                action(&card);
            }
        });
    }

    /// Smaller, for beside a sidebar of machines.
    pub fn set_compact(&self, compact: bool) {
        let (padding, height) =
            if compact { (COMPACT_PADDING, COMPACT_LEAST_HEIGHT) } else { (PADDING, LEAST_HEIGHT) };
        let body = &self.0.body;
        body.set_margin_top(padding);
        body.set_margin_bottom(padding);
        body.set_margin_start(padding);
        body.set_margin_end(padding);
        self.0.root.set_height_request(height);
        // Beside a sidebar the hero is all the page has, so its picture takes the height there is.
        self.0.root.set_vexpand(compact);
        if compact {
            self.0.root.add_css_class("tall");
        } else {
            self.0.root.remove_css_class("tall");
        }
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.0.root
    }

    /// The machine it shows, which is the one the keyboard's shortcuts are for.
    pub fn chosen(&self) -> Option<MachineCard> {
        self.0.card.borrow().clone()
    }

    pub fn shows(&self, card: &MachineCard) -> bool {
        self.0.card.borrow().as_ref().is_some_and(|chosen| chosen.widget() == card.widget())
    }

    pub fn choose(&self, card: Option<&MachineCard>) {
        *self.0.card.borrow_mut() = card.cloned();
        self.0.root.set_visible(card.is_some());
        self.refresh();
        self.show_picture();
    }

    /// What the machine is, without its state: "Windows with GPU-PV", then one fact each.
    fn set_facts(&self, headline: &str, state: State) {
        let parts = &self.0;
        while let Some(child) = parts.specs.first_child() {
            parts.specs.remove(&child);
        }
        let facts = headline.strip_prefix(&format!("{}. ", state.name()));
        let mut facts = facts.map(|facts| facts.split(", ")).into_iter().flatten();
        // A headline that isn't the state and the summary is a step or a failed install: all of it.
        parts.kind.set_label(facts.next().unwrap_or(headline));
        parts.kind.set_ellipsize(gtk::pango::EllipsizeMode::None);
        parts.kind.set_wrap(true);
        for fact in facts {
            let label = gtk::Label::new(Some(fact));
            label.add_css_class("spec");
            add_to(&parts.specs, &label);
        }
        parts.specs.set_visible(parts.specs.first_child().is_some());
    }

    /// A chip for each program with windows open, made again only when that changes.
    fn set_apps(&self, programs: &[OpenProgram]) {
        let parts = &self.0;
        if *parts.open.borrow() == programs && parts.apps.first_child().is_some() == !programs.is_empty() {
            return;
        }
        *parts.open.borrow_mut() = programs.to_vec();
        parts.apps.remove_all();
        for program in programs {
            parts.apps.append(&chip(program));
        }
        if !programs.is_empty() {
            add_to(&parts.apps, &parts.open_app);
        }
        parts.apps.set_visible(!programs.is_empty());
    }

    /// Puts the chosen machine's last screen behind the words. It's read from its file, so only
    /// when there may be a new one.
    pub fn show_picture(&self) {
        let card = self.0.card.borrow().clone();
        self.0.picture.set_paintable(card.and_then(|card| card.thumbnail()).as_ref());
    }

    /// Takes in what the chosen machine shows of itself now.
    pub fn refresh(&self) {
        let parts = &self.0;
        let Some(card) = parts.card.borrow().clone() else { return };
        let shown = card.shown();
        parts.name.set_label(&card.machine().borrow().name);
        parts.state.set_label(shown.state.name());
        parts.spinner.set_visible(shown.busy);
        parts.dot.set_visible(!shown.busy);
        if shown.is_off() {
            parts.dot.add_css_class("off");
        } else {
            parts.dot.remove_css_class("off");
        }
        shown.weight.mark(&parts.dot);
        self.set_facts(&shown.headline, shown.state);
        parts.notes.set_label(&shown.notes.join("\n"));
        parts.notes.set_visible(!shown.notes.is_empty());
        parts.start.set_label(shown.start_label);
        set_shown(&parts.start, shown.is_off());
        parts.start.set_sensitive(shown.can_start);
        set_shown(&parts.show, shown.has_screen());
        parts.show.set_sensitive(!card.screen_busy());
        parts.show.set_label(if card.screen_busy() { SCREEN_BUSY } else { SCREEN_LABEL });
        set_shown(&parts.stop, shown.state == State::Running);
        parts.stop.set_sensitive(shown.can_stop);
        set_shown(&parts.force, !shown.is_off());
        self.set_apps(&card.open_programs());
    }
}

impl Default for Hero {
    fn default() -> Self {
        Self::new()
    }
}
