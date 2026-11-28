//! The window keeping in step with the look: how the library is arranged, how big its cards are, and
//! whether the window has a bar of its own.
//!
//! Tide drops the bar when the compositor tiles the window, or always on a compositor that tiles,
//! as that places, moves and closes windows itself. The compositor says a window is tiled by
//! marking all four of its edges as against something. A window snapped to half the screen on a
//! floating desktop is against three, and keeps its bar.

use super::Window;
use super::library_page::Arrangement;
use crate::theme::{self, Drawn};
use adw::prelude::*;
use gtk::gdk;

/// The edges a tiled window is against: all four.
const TILED: gdk::ToplevelState = gdk::ToplevelState::TOP_TILED
    .union(gdk::ToplevelState::BOTTOM_TILED)
    .union(gdk::ToplevelState::LEFT_TILED)
    .union(gdk::ToplevelState::RIGHT_TILED);

/// The window's class while it has no bar of its own.
const BARE: &str = "bare";

pub fn is_tiled(state: gdk::ToplevelState) -> bool {
    state.contains(TILED)
}

fn arrangement(drawn: Drawn) -> Arrangement {
    match drawn {
        Drawn::System => Arrangement::Rail,
        Drawn::Tide | Drawn::Kde => Arrangement::Sidebar,
    }
}

/// Whether a window in this look, tiled or not, has a bar of its own.
pub fn has_bar(drawn: Drawn, tiled: bool, desktop_tiles: bool) -> bool {
    drawn != Drawn::Tide || !(tiled || desktop_tiles)
}

/// Whether a layout of the window's buttons, like `icon:minimize,close`, shows the app's icon.
fn shows_icon(layout: &str) -> bool {
    layout.split([':', ',']).any(|button| button.trim() == "icon")
}

/// Whether the cards are lines of a sidebar in the look that draws the app now.
pub fn compact_cards() -> bool {
    arrangement(theme::drawn()) == Arrangement::Sidebar
}

impl Window {
    /// Whether the compositor has the window tiled just now.
    pub fn is_tiled(&self) -> bool {
        self.0
            .window
            .surface()
            .and_downcast::<gdk::Toplevel>()
            .is_some_and(|toplevel| is_tiled(toplevel.state()))
    }

    /// Whether the window has a bar of its own just now, for the pages in it.
    pub fn has_bar(&self) -> bool {
        has_bar(theme::drawn(), self.is_tiled(), theme::desktop_tiles())
    }

    /// Puts the library and its cards in the arrangement the look has, and the bar where it goes.
    pub(super) fn follow_look(&self) {
        let arrangement = arrangement(theme::drawn());
        self.0.library.arrange(arrangement);
        for card in self.0.cards.borrow().values() {
            card.set_compact(arrangement == Arrangement::Sidebar);
        }
        let bar = self.has_bar();
        self.0.library.set_bar_shown(bar);
        // Without a bar the compositor places the window, and draws its edge: Tide's style leaves
        // the window's own corners and shadow out by this class.
        if bar {
            self.0.window.remove_css_class(BARE);
        } else {
            self.0.window.add_css_class(BARE);
        }
        let layout = gtk::Settings::default().and_then(|settings| settings.gtk_decoration_layout());
        self.0.library.set_brand_icon_shown(!layout.is_some_and(|layout| shows_icon(&layout)));
        if let Some(page) = self.0.screen_page.borrow().as_ref() {
            page.follow_look();
        }
    }

    /// Follows the look from now on, as the preferences change it and the compositor tiles the window.
    pub(super) fn watch_look(&self) {
        let this = self.clone();
        theme::connect_changed(move |_| this.follow_look());
        let this = self.clone();
        self.0.window.connect_realize(move |window| {
            let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() else { return };
            let tiled = this.clone();
            toplevel.connect_state_notify(move |_| tiled.follow_look());
        });
        self.follow_look();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_all_four_edges_are_tiled() {
        assert!(is_tiled(TILED));
        assert!(is_tiled(TILED | gdk::ToplevelState::FOCUSED));
        let half =
            gdk::ToplevelState::TOP_TILED | gdk::ToplevelState::BOTTOM_TILED | gdk::ToplevelState::LEFT_TILED;
        assert!(!is_tiled(half));
    }

    #[test]
    fn an_icon_in_the_buttons_is_seen() {
        assert!(shows_icon("icon:minimize,maximize,close"));
        assert!(!shows_icon(":minimize,maximize,close"));
        assert!(!shows_icon("appmenu:close"));
    }

    #[test]
    fn tide_drops_the_bar_when_the_compositor_places_the_window() {
        assert!(!has_bar(Drawn::Tide, true, false));
        assert!(!has_bar(Drawn::Tide, false, true));
        assert!(has_bar(Drawn::Tide, false, false));
        assert!(has_bar(Drawn::Kde, true, true));
        assert!(has_bar(Drawn::System, true, true));
    }
}
