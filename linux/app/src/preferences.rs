//! What the app itself can be told, as opposed to a machine: its colours, whether a login starts it,
//! and where the desktop's shortcuts go from a machine's screen.

use crate::login;
use crate::notify;
use crate::rows::{choice_row, explain};
use crate::theme::{self, Look};
use crate::viewer::{self, CAPTURE_KEYS};
use crate::window::{TRAY, Window};
use adw::prelude::*;
use guesthouse_app_core::settings::read_prefs;
use serde_json::{Value, json};

/// The colour schemes to choose from, and what the preference holds for each. The first follows the
/// desktop, and is what anything else in the preference means too.
const SCHEMES: [(&str, &str); 3] = [("Follow Desktop", ""), ("Light", "light"), ("Dark", "dark")];
const LOOK_INFO: &str = "Automatic picks by the desktop: Tide on a tiling window manager such as Hyprland or \
                         Sway, KDE on Plasma, and System, your desktop's own theme, everywhere else. Tide is \
                         compact, and drops its bar when the window is tiled.";
/// The second is the desktop's own, where it has one.
const ACCENTS: [&str; 2] = ["Blue", "Desktop's Accent"];
const ACCENT_INFO: &str = "The desktop's accent colour is read when the app starts, and again when this is \
                           changed. A desktop that has none leaves it blue.";
const NOTIFICATIONS_INFO: &str = "Only while the window isn't in front: a machine that has started, has been \
                                  installed or has stopped by itself, and a new version of Guesthouse.";
const WIDTH: i32 = 520;

pub fn show(window: &Window) {
    let prefs = read_prefs(&window.context());
    let saved = prefs.get(theme::SCHEME).and_then(Value::as_str).map(str::to_owned);
    let names: Vec<&str> = SCHEMES.iter().map(|(name, _)| *name).collect();
    let colours = choice_row("Colour Scheme", &names);
    let chosen = SCHEMES.iter().position(|(_, value)| Some(*value) == saved.as_deref()).unwrap_or(0);
    colours.set_selected(chosen as u32);
    let remembering = window.clone();
    colours.connect_selected_notify(move |row| {
        let value = SCHEMES.get(row.selected() as usize).map_or("", |(_, value)| *value);
        remembering.remember(json!({ theme::SCHEME: value }));
        theme::follow(&read_prefs(&remembering.context()));
    });
    let desktops = theme::wants_desktop_accent(&prefs);
    let accent = choice_row("Accent Colour", &ACCENTS);
    explain(&accent, ACCENT_INFO);
    accent.set_selected(u32::from(desktops));
    let remembering = window.clone();
    accent.connect_selected_notify(move |row| {
        let value = if row.selected() == 1 { theme::DESKTOP_ACCENT } else { "" };
        remembering.remember(json!({ theme::ACCENT: value }));
        theme::follow(&read_prefs(&remembering.context()));
    });
    let looks: Vec<&str> = Look::ALL.iter().map(|look| look.name()).collect();
    let whose = choice_row("Look", &looks);
    explain(&whose, LOOK_INFO);
    let saved_look = Look::from_prefs(&prefs);
    whose.set_selected(Look::ALL.iter().position(|look| *look == saved_look).unwrap_or(0) as u32);
    let remembering = window.clone();
    whose.connect_selected_notify(move |row| {
        let look = Look::ALL.get(row.selected() as usize).copied().unwrap_or(Look::Automatic);
        remembering.remember(json!({ theme::LOOK_OF: look.value() }));
        theme::follow(&read_prefs(&remembering.context()));
    });
    let look = adw::PreferencesGroup::builder().title("Appearance").build();
    look.add(&colours);
    look.add(&whose);
    look.add(&accent);
    let at_login = adw::SwitchRow::builder()
        .title("Start at Login")
        .subtitle("And the machines set to start with this PC")
        .active(login::starts_at_login())
        .build();
    let telling = window.clone();
    at_login.connect_active_notify(move |row| {
        if let Err(failure) = login::set_starts_at_login(row.is_active()) {
            telling.problem(&format!("Could not change what starts at login: {failure}"));
        }
    });
    let in_tray = adw::SwitchRow::builder()
        .title("Keep Running in the Tray")
        .subtitle("Closing the window leaves the app in the tray")
        .active(window.has_tray())
        .build();
    let asking = window.clone();
    in_tray.connect_active_notify(move |row| {
        let wanted = row.is_active();
        let there = asking.set_tray(wanted, false);
        asking.remember(json!({ TRAY: there }));
        if wanted && !there {
            asking.problem("This desktop has no tray for the icon to go in.");
            row.set_active(false);
        }
    });
    let notifications = adw::SwitchRow::builder()
        .title("Notifications")
        .subtitle("When a machine starts, installs or stops")
        .active(notify::wanted(window))
        .build();
    explain(&notifications, NOTIFICATIONS_INFO);
    let remembering = window.clone();
    notifications.connect_active_notify(move |row| {
        remembering.remember(json!({ notify::NOTIFICATIONS: row.is_active() }))
    });
    let background = adw::PreferencesGroup::builder().title("Running").build();
    background.add(&at_login);
    background.add(&in_tray);
    background.add(&notifications);
    let shortcuts = adw::SwitchRow::builder()
        .title("Send Desktop Shortcuts to the Guest")
        .subtitle("Such as Alt+Tab and Super")
        .active(viewer::shortcuts_on_focus(&prefs))
        .build();
    explain(
        &shortcuts,
        &format!(
            "Whenever a machine's screen has the keyboard, the desktop's shortcuts go to the guest. Turned off, \
             your desktop keeps them, as a window manager run by its shortcuts, such as Hyprland or Sway, \
             needs. Typing still goes to the guest, and {CAPTURE_KEYS} hands it the shortcuts with the mouse \
             until it's pressed again."
        ),
    );
    let remembering = window.clone();
    shortcuts.connect_active_notify(move |row| {
        remembering.remember(json!({ viewer::SHORTCUTS_ON_FOCUS: row.is_active() }))
    });
    let screens = adw::PreferencesGroup::builder().title("Machine Screens").build();
    screens.add(&shortcuts);
    let page = adw::PreferencesPage::new();
    page.add(&look);
    page.add(&background);
    page.add(&screens);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    let dialog = adw::Dialog::builder().title("Preferences").child(&view).content_width(WIDTH).build();
    dialog.present(Some(window.widget()));
}
