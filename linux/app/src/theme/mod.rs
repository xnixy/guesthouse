//! The app's looks, and the one in use.
//!
//! One preference, Look, says which draws the app. Automatic, the default, picks by what the desktop
//! says it is: Tide on a tiling compositor, the KDE look on Plasma, and the desktop's own GTK theme
//! (System) everywhere else. Each can also be asked for by name.
//!
//! Tide and KDE are the app's own: their colours go in under the names libadwaita reads its own
//! from, which is what lets one set of shapes serve dark and light. System leaves the desktop's
//! theme to draw everything it knows, and styles only the widgets that are the app's own, in the
//! theme's colours.

mod accent;
pub mod desktop;
mod glass;
mod kde;
mod look;
pub mod stylesheet;

pub use look::{Drawn, LOOK_OF, Look};

use desktop::Desktop;
use guesthouse_app_core::settings::Fields;
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use stylesheet::Wanted;

/// Above USER, as a desktop can write colours of its own into the user's gtk.css.
const PRIORITY: u32 = gtk::STYLE_PROVIDER_PRIORITY_USER + 1;
/// Where an app's own style usually goes, under the desktop's theme and the user's gtk.css. It's
/// where the app's own widgets go when the desktop's theme is the one wanted.
const UNDER_THE_THEME: u32 = gtk::STYLE_PROVIDER_PRIORITY_APPLICATION;
/// The preference that says which colours to use.
pub const SCHEME: &str = "scheme";
/// The preference that says whose accent colour to use, and what it holds for the desktop's.
pub const ACCENT: &str = "accent";
pub const DESKTOP_ACCENT: &str = "desktop";

/// The stylesheet in use and what decides its text.
struct State {
    provider: gtk::CssProvider,
    display: gtk::gdk::Display,
    desktop: Desktop,
    /// Whether the compositor blurs behind windows, asked once.
    glass: bool,
    drawn: Drawn,
    /// The accent colour wanted, if it's the desktop's and the desktop has one.
    accent: Option<accent::Rgb>,
    /// Whether the KDE look has put the window's buttons in KWin's order, over GTK's own setting.
    kwin_buttons: bool,
    /// Told when the look that draws the app changes.
    listeners: Vec<Rc<dyn Fn(Drawn)>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// What the preference asks for. Anything else follows the desktop. Where the desktop doesn't say,
/// Tide is dark, and the desktop's theme and KDE's are left as they come: a desktop that says
/// nothing is most often a light one.
fn scheme(prefs: &Fields, drawn: Drawn) -> adw::ColorScheme {
    match prefs.get(SCHEME).and_then(Value::as_str) {
        Some("dark") => adw::ColorScheme::ForceDark,
        Some("light") => adw::ColorScheme::ForceLight,
        _ if drawn == Drawn::Tide => adw::ColorScheme::PreferDark,
        _ => adw::ColorScheme::Default,
    }
}

/// Whether the desktop's accent is the one wanted. The KDE look takes Plasma's unless told not to;
/// the others take it when asked.
fn desktop_accent_wanted(prefs: &Fields, drawn: Drawn) -> bool {
    match prefs.get(ACCENT).and_then(Value::as_str) {
        Some(value) => value == DESKTOP_ACCENT,
        None => drawn == Drawn::Kde,
    }
}

/// The same, for the look that draws the app now, as the preferences show it.
pub fn wants_desktop_accent(prefs: &Fields) -> bool {
    desktop_accent_wanted(prefs, drawn())
}

/// The desktop's accent, when that's the one wanted. On KDE, what Plasma selects with stands in
/// for a portal that says none.
fn wanted_accent(prefs: &Fields, drawn: Drawn) -> Option<accent::Rgb> {
    if !desktop_accent_wanted(prefs, drawn) {
        return None;
    }
    accent::desktop_accent().or_else(|| (drawn == Drawn::Kde).then(kde::selection).flatten())
}

fn load() {
    STATE.with_borrow(|state| {
        let Some(state) = state else { return };
        let dark = adw::StyleManager::default().is_dark();
        let kde_colours = if state.drawn == Drawn::Kde { kde::colours(dark) } else { String::new() };
        let wanted = Wanted { dark, accent: state.accent, glass: state.glass, kde_colours };
        state.provider.load_from_string(&stylesheet::text(state.drawn, &wanted));
    });
}

/// Puts the stylesheet over the desktop's theme or under it.
fn place(state: &State) {
    gtk::style_context_remove_provider_for_display(&state.display, &state.provider);
    let priority = if state.drawn == Drawn::System { UNDER_THE_THEME } else { PRIORITY };
    gtk::style_context_add_provider_for_display(&state.display, &state.provider, priority);
}

/// The window's buttons in KWin's order for the KDE look. Leaving it, GTK's setting goes back to
/// following the desktop.
fn place_buttons(display: &gtk::gdk::Display, kde: bool, set_before: bool) -> bool {
    let settings = gtk::Settings::for_display(display);
    if kde {
        settings.set_gtk_decoration_layout(Some(&kde::button_layout()));
    } else if set_before {
        settings.reset_property("gtk-decoration-layout");
    }
    kde
}

/// The KDE look's buttons, by what draws the app now. Outside any borrow of the state, as GTK
/// tells the windows of the change at once.
fn follow_buttons() {
    let Some((display, kde, set_before)) = STATE.with_borrow(|state| {
        state.as_ref().map(|state| (state.display.clone(), state.drawn == Drawn::Kde, state.kwin_buttons))
    }) else {
        return;
    };
    if !kde && !set_before {
        return;
    }
    let set = place_buttons(&display, kde, set_before);
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state {
            state.kwin_buttons = set;
        }
    });
}

/// Gives every window on this display the look the preferences ask for, and keeps it in step with
/// dark and light.
pub fn install(display: &gtk::gdk::Display, prefs: &Fields) {
    let desktop = Desktop::current();
    let drawn = Look::from_prefs(prefs).drawn_on(&desktop);
    adw::StyleManager::default().set_color_scheme(scheme(prefs, drawn));
    let state = State {
        provider: gtk::CssProvider::new(),
        display: display.clone(),
        glass: glass::compositor_blurs(),
        desktop,
        drawn,
        accent: wanted_accent(prefs, drawn),
        kwin_buttons: false,
        listeners: Vec::new(),
    };
    STATE.set(Some(state));
    STATE.with_borrow(|state| {
        if let Some(state) = state {
            place(state);
        }
    });
    follow_buttons();
    load();
    adw::StyleManager::default().connect_dark_notify(|_| load());
}

/// Takes in changed preferences while the app runs.
pub fn follow(prefs: &Fields) {
    // Nothing that GTK may call back into runs while the state is borrowed to be changed.
    let changed = STATE.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        let drawn = Look::from_prefs(prefs).drawn_on(&state.desktop);
        let before = std::mem::replace(&mut state.drawn, drawn);
        Some((before, drawn, state.listeners.clone()))
    });
    let Some((before, drawn, listeners)) = changed else { return };
    let accent = wanted_accent(prefs, drawn);
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state {
            state.accent = accent;
        }
    });
    adw::StyleManager::default().set_color_scheme(scheme(prefs, drawn));
    STATE.with_borrow(|state| {
        if let Some(state) = state {
            place(state);
        }
    });
    follow_buttons();
    load();
    if before != drawn {
        for listener in listeners {
            listener(drawn);
        }
    }
}

/// The look that draws the app now. Before the look is installed, the desktop's theme.
pub fn drawn() -> Drawn {
    STATE.with_borrow(|state| state.as_ref().map_or(Drawn::System, |state| state.drawn))
}

/// Whether the compositor places and closes windows itself.
pub fn desktop_tiles() -> bool {
    STATE.with_borrow(|state| state.as_ref().is_some_and(|state| state.desktop.tiles()))
}

/// Calls `changed` with the new look each time another one draws the app.
pub fn connect_changed(changed: impl Fn(Drawn) + 'static) {
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state {
            state.listeners.push(Rc::new(changed));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kde_takes_plasmas_accent_unless_told_not_to() {
        let prefs = |text: &str| serde_json::from_str::<Fields>(text).unwrap();
        assert!(desktop_accent_wanted(&prefs("{}"), Drawn::Kde));
        assert!(!desktop_accent_wanted(&prefs(r#"{"accent":""}"#), Drawn::Kde));
        assert!(!desktop_accent_wanted(&prefs("{}"), Drawn::Tide));
        assert!(desktop_accent_wanted(&prefs(r#"{"accent":"desktop"}"#), Drawn::Tide));
    }

    #[test]
    fn tide_is_dark_unless_asked_and_the_others_follow_the_desktop() {
        let prefs = |text: &str| serde_json::from_str::<Fields>(text).unwrap();
        assert_eq!(scheme(&prefs("{}"), Drawn::System), adw::ColorScheme::Default);
        assert_eq!(scheme(&prefs("{}"), Drawn::Kde), adw::ColorScheme::Default);
        assert_eq!(scheme(&prefs("{}"), Drawn::Tide), adw::ColorScheme::PreferDark);
        assert_eq!(scheme(&prefs(r#"{"scheme":"light"}"#), Drawn::Tide), adw::ColorScheme::ForceLight);
    }
}
