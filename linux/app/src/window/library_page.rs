//! The first page: the machines, or a way to make the first one.
//!
//! The machines are arranged one of two ways, by the look. Under the desktop's own theme they are
//! cards in a rail under the hero, as wide as the window lets them be. In the app's own looks they
//! are a list down a sidebar, beside the hero (see [`Sidebar`]). The hero, the share panel and the
//! cards are the same widgets either way, and move when the look does.

use super::Window;
use super::hero::Hero;
use super::library_sidebar::Sidebar;
use super::share_panel::SharePanel;
use crate::application::APP_ID;
use crate::rows::name_for_readers;
use adw::prelude::*;
use gtk::gio;
use guesthouse_app_core::updates::VERSION;
use std::cell::{Cell, RefCell};

const COLUMN_SPACING: i32 = 20;
const COLUMN_MARGIN: i32 = 24;
const SIDE_MARGIN: i32 = 20;
const COLUMN_WIDTH: i32 = 1160;
const CARD_SPACING: u32 = 16;
/// As many cards side by side as a wide window has room for.
const CARDS_ACROSS: u32 = 3;
const BRAND_SPACING: i32 = 8;
const RAIL: &str = "machines";
const SIDEBAR: &str = "sidebar";
const EMPTY: &str = "empty";

/// Where the machines go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrangement {
    /// Cards in a rail under the hero.
    Rail,
    /// A list down a sidebar, beside the hero.
    Sidebar,
}

pub struct LibraryPage {
    pub page: adw::NavigationPage,
    hero: Hero,
    share: SharePanel,
    rail: gtk::FlowBox,
    /// The rail's column: the hero, the share panel, the rail and the problems, in that order.
    column: gtk::Box,
    machines_label: gtk::Label,
    sidebar: Sidebar,
    problems: gtk::Label,
    stack: gtk::Stack,
    header: adw::HeaderBar,
    /// The app's name in the bar.
    brand: gtk::Box,
    add: gtk::Button,
    start: gtk::Button,
    update: adw::Banner,
    /// The cards in the order they came, to be moved from one arrangement to the other.
    cards: RefCell<Vec<gtk::Box>>,
    arrangement: Cell<Arrangement>,
    has_machines: Cell<bool>,
}

/// Takes a widget out of whatever box, rail or list it's in.
fn detach(widget: &impl IsA<gtk::Widget>) {
    let Some(parent) = widget.parent() else { return };
    if let Some(holder) = parent.downcast_ref::<gtk::FlowBoxChild>() {
        if let Some(rail) = holder.parent().and_downcast::<gtk::FlowBox>() {
            rail.remove(holder);
        }
    } else if let Some(row) = parent.downcast_ref::<gtk::ListBoxRow>() {
        if let Some(list) = row.parent().and_downcast::<gtk::ListBox>() {
            list.remove(row);
        }
    } else if let Some(holder) = parent.downcast_ref::<gtk::Box>() {
        holder.remove(widget);
    }
}

/// The app's name and version, where a title would be. The icon is the first child.
fn brand() -> gtk::Box {
    let name = gtk::Label::new(Some("Guesthouse"));
    name.add_css_class("brand");
    let version = gtk::Label::new(Some(VERSION));
    version.add_css_class("dim-label");
    version.add_css_class("caption");
    let brand = gtk::Box::builder().spacing(BRAND_SPACING).margin_start(BRAND_SPACING).build();
    brand.append(&gtk::Image::from_icon_name(&format!("{APP_ID}-symbolic")));
    brand.append(&name);
    brand.append(&version);
    brand
}

fn main_menu() -> gio::Menu {
    let app_items = gio::Menu::new();
    app_items.append(Some("Import Machine…"), Some("win.import"));
    app_items.append(Some("Preferences"), Some("win.preferences"));
    let setup_items = gio::Menu::new();
    setup_items.append(Some("Run Setup Again"), Some("win.setup"));
    setup_items.append(Some("Show Log"), Some("win.log"));
    setup_items.append(Some("Send Report…"), Some("win.report"));
    setup_items.append(Some("Check for Updates"), Some("win.check-updates"));
    let about_items = gio::Menu::new();
    about_items.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    about_items.append(Some("About Guesthouse"), Some("win.about"));
    let menu = gio::Menu::new();
    menu.append_section(None, &app_items);
    menu.append_section(None, &setup_items);
    menu.append_section(None, &about_items);
    menu
}

impl LibraryPage {
    pub fn new(description: &str) -> Self {
        let hero = Hero::new();
        let share = SharePanel::new();
        let rail = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .column_spacing(CARD_SPACING)
            .row_spacing(CARD_SPACING)
            .max_children_per_line(CARDS_ACROSS)
            .valign(gtk::Align::Start)
            .build();
        rail.add_css_class("rail");
        let machines = gtk::Label::builder().label("Machines").xalign(0.0).build();
        machines.add_css_class("section-label");
        let problems = gtk::Label::builder().wrap(true).xalign(0.0).visible(false).build();
        problems.add_css_class("error");
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(COLUMN_SPACING)
            .margin_top(COLUMN_MARGIN)
            .margin_bottom(COLUMN_MARGIN)
            .margin_start(SIDE_MARGIN)
            .margin_end(SIDE_MARGIN)
            .build();
        column.append(&machines);
        column.append(&rail);
        let clamp = adw::Clamp::builder().child(&column).maximum_size(COLUMN_WIDTH).build();
        let scroller = gtk::ScrolledWindow::builder().child(&clamp).build();
        let start = gtk::Button::builder().label("New Machine").halign(gtk::Align::Center).build();
        start.add_css_class("pill");
        start.add_css_class("suggested-action");
        let empty = crate::rows::message_page(APP_ID, "No Machines", description, &start);
        let stack = gtk::Stack::new();
        let sidebar = Sidebar::new(&brand(), &main_menu());
        stack.add_named(&scroller, Some(RAIL));
        stack.add_named(&sidebar.root, Some(SIDEBAR));
        stack.add_named(&empty, Some(EMPTY));
        let add = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label("New Machine").build())
            .build();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
        let header_brand = brand();
        header.pack_start(&header_brand);
        let menu = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .primary(true)
            .tooltip_text("Main Menu")
            .menu_model(&main_menu())
            .build();
        name_for_readers(&menu, "Main Menu");
        header.pack_end(&menu);
        header.pack_end(&add);
        let toolbar = adw::ToolbarView::new();
        toolbar.set_content(Some(&stack));
        toolbar.add_top_bar(&header);
        let update = adw::Banner::builder().button_label("Update").action_name("win.update").build();
        toolbar.add_top_bar(&update);
        let page = adw::NavigationPage::builder().title("Guesthouse").tag("library").child(&toolbar).build();
        let this = Self {
            page,
            hero,
            share,
            rail,
            column,
            machines_label: machines,
            sidebar,
            problems,
            stack,
            header,
            brand: header_brand,
            add,
            start,
            update,
            cards: RefCell::default(),
            arrangement: Cell::new(Arrangement::Rail),
            has_machines: Cell::new(false),
        };
        this.place_column();
        this
    }

    /// Puts the hero, the share panel and the problems where the arrangement has them.
    fn place_column(&self) {
        let parts: [&gtk::Widget; 3] =
            [self.hero.widget().upcast_ref(), self.share.widget().upcast_ref(), self.problems.upcast_ref()];
        for part in parts {
            detach(part);
        }
        match self.arrangement.get() {
            Arrangement::Rail => {
                self.column.prepend(self.share.widget());
                self.column.prepend(self.hero.widget());
                self.column.append(&self.problems);
            }
            Arrangement::Sidebar => {
                for part in parts {
                    self.sidebar.column.append(part);
                }
            }
        }
    }

    /// Moves everything to this arrangement.
    pub fn arrange(&self, arrangement: Arrangement) {
        if self.arrangement.replace(arrangement) == arrangement {
            return;
        }
        let compact = arrangement == Arrangement::Sidebar;
        self.place_column();
        self.hero.set_compact(compact);
        for card in self.cards.borrow().iter() {
            detach(card);
            self.place_card(card);
        }
        let chosen = self.hero.chosen();
        self.mark_chosen(chosen.as_ref().map(|card| card.widget()));
        self.machines_label.set_visible(!compact);
        self.show_page();
    }

    fn place_card(&self, card: &gtk::Box) {
        match self.arrangement.get() {
            Arrangement::Rail => self.rail.append(card),
            Arrangement::Sidebar => self.sidebar.list.append(card),
        }
    }

    /// Leaves the app's icon out of the bar when the window's buttons already show it, as KWin's
    /// layout does.
    pub fn set_brand_icon_shown(&self, shown: bool) {
        let brand = self.brand.first_child();
        if let Some(icon) = brand {
            icon.set_visible(shown);
        }
    }

    /// Whether the page has a bar of its own. Without one, the app's name, its menu and New Machine
    /// go at the top of the sidebar, and the window is moved and closed by the compositor.
    pub fn set_bar_shown(&self, shown: bool) {
        self.header.set_visible(shown);
        self.sidebar.set_top_shown(!shown);
    }

    pub fn connect_new_machine(&self, window: &Window) {
        for button in [&self.add, &self.start, &self.sidebar.add] {
            let window = window.clone();
            button.connect_clicked(move |_| window.show_new_machine());
        }
    }

    pub fn offer_update(&self, version: &str) {
        self.update.set_title(&format!("Guesthouse {version} is out. You have {VERSION}."));
        self.update.set_revealed(true);
    }

    pub fn hero(&self) -> &Hero {
        &self.hero
    }

    pub fn share_panel(&self) -> &SharePanel {
        &self.share
    }

    pub fn add(&self, card: &gtk::Box) {
        self.cards.borrow_mut().push(card.clone());
        self.place_card(card);
    }

    pub fn remove(&self, card: &gtk::Box) {
        self.cards.borrow_mut().retain(|known| known != card);
        detach(card);
    }

    /// Calls `chosen` with the card that was clicked, or that Enter was pressed on.
    pub fn connect_chosen(&self, chosen: impl Fn(&gtk::Widget) + 'static) {
        let chosen = std::rc::Rc::new(chosen);
        let from_rail = chosen.clone();
        self.rail.connect_child_activated(move |_, holder| {
            if let Some(card) = holder.child() {
                from_rail(&card);
            }
        });
        // The keys move the selection in the sidebar, and the ring and the hero go with it, so the
        // shortcuts act on the machine that's ringed.
        self.sidebar.list.connect_row_selected(move |_, row| {
            if let Some(card) = row.and_then(gtk::ListBoxRow::child) {
                chosen(&card);
            }
        });
    }

    /// Rings the card of the machine the hero shows.
    pub fn mark_chosen(&self, chosen: Option<&gtk::Box>) {
        self.sidebar.mark_chosen(chosen);
        let mut holder = self.rail.first_child();
        while let Some(found) = holder {
            let is_chosen = chosen.is_some_and(|card| card.parent().as_ref() == Some(&found));
            if is_chosen {
                found.add_css_class("chosen");
            } else {
                found.remove_css_class("chosen");
            }
            holder = found.next_sibling();
        }
    }

    pub fn show(&self, has_machines: bool, errors: &[String]) {
        self.problems.set_label(&errors.join("\n"));
        self.problems.set_visible(!errors.is_empty());
        self.has_machines.set(has_machines || !errors.is_empty());
        self.show_page();
    }

    fn show_page(&self) {
        let page = match self.arrangement.get() {
            _ if !self.has_machines.get() => EMPTY,
            Arrangement::Rail => RAIL,
            Arrangement::Sidebar => SIDEBAR,
        };
        self.stack.set_visible_child_name(page);
    }
}
