//! Which card a Windows machine's GPU-PV runs on. Windows in it was installed for that card's maker,
//! so the row offers only cards of the same maker, and only while the machine is off. It's there on a
//! PC with more than one card to run on, or when the machine's own card is gone.

use super::{Shared, is_off};
use crate::rows::choice_row;
use adw::prelude::*;
use guesthouse_app_core::card_choice::{same_maker_choices, windows_card_choices};
use guesthouse_app_core::machine::Kind;
use guesthouse_app_core::tasks::Status;

const TITLE: &str = "Card";
const NOTE: &str = "Windows here was installed for this card's maker. It can move to another card of the same maker \
                    while it's off. For another maker's card, make a new machine.";

pub struct WindowsCard {
    row: Option<adw::ComboRow>,
}

impl WindowsCard {
    pub fn new(shared: &Shared, page: &adw::PreferencesPage) -> Self {
        let machine = shared.machine.borrow().clone();
        let context = shared.window.context();
        if machine.kind != Kind::Windows {
            return Self { row: None };
        }
        let choices = same_maker_choices(&context, &machine);
        let own_card_here = windows_card_choices(&context).iter().any(|choice| choice.pci == machine.amd_gpu);
        if windows_card_choices(&context).len() < 2 && own_card_here {
            return Self { row: None };
        }
        let group = adw::PreferencesGroup::builder().title("GPU").build();
        page.add(&group);
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        let row = choice_row(TITLE, &labels);
        row.set_subtitle(NOTE);
        let at = choices.iter().position(|choice| choice.pci == machine.amd_gpu).unwrap_or(0);
        row.set_selected(at as u32);
        let saving = shared.clone();
        row.connect_selected_notify(move |row| {
            let Some(choice) = choices.get(row.selected() as usize) else { return };
            if saving.machine.borrow().amd_gpu == choice.pci {
                return;
            }
            saving.machine.borrow_mut().use_windows_card(choice.maker, &choice.pci);
            saving.save();
        });
        group.add(&row);
        Self { row: Some(row) }
    }

    pub fn set_status(&self, status: &Status) {
        if let Some(row) = &self.row {
            row.set_sensitive(is_off(status));
        }
    }
}
