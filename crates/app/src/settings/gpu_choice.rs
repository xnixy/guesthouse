//! Which card a Linux guest's GPU is: the NVIDIA card, forwarded, or an AMD card. The row is only there
//! on a PC with an AMD card, and lists each of them. A guest has one or the other, never both.

use super::Shared;
use crate::rows::choice_row;
use adw::prelude::*;
use guesthouse_app_core::linux::amd::{amd_cards, try_native_again};

const TITLE: &str = "Card";
const NVIDIA: &str = "NVIDIA, forwarded";
const NOTE: &str = "An AMD card gives the guest a virtio GPU on it. The guest builds its Mesa for it once, and again \
                    when its distro updates Mesa. If that fails, it goes on with Venus, which needs no build.";

/// The row, added to `group`, or nothing on a PC with no AMD card.
pub fn add_gpu_choice(shared: &Shared, group: &adw::PreferencesGroup) -> Option<adw::ComboRow> {
    let cards = amd_cards(&shared.window.context());
    let chosen = shared.machine.borrow().amd_gpu.clone();
    if cards.is_empty() && chosen.is_empty() {
        return None;
    }
    let mut choices: Vec<(String, String)> = vec![(String::new(), NVIDIA.to_owned())];
    choices
        .extend(cards.into_iter().map(|card| (card.pci.clone(), format!("{} at {}", card.name, card.pci))));
    if !choices.iter().any(|(pci, _)| *pci == chosen) {
        choices.push((chosen.clone(), format!("The AMD card at {chosen}, which isn't in this PC now")));
    }
    let names: Vec<&str> = choices.iter().map(|(_, name)| name.as_str()).collect();
    let row = choice_row(TITLE, &names);
    row.set_subtitle(NOTE);
    let at = choices.iter().position(|(pci, _)| *pci == chosen).unwrap_or(0);
    row.set_selected(at as u32);
    let saving = shared.clone();
    row.connect_selected_notify(move |row| {
        let Some((pci, _)) = choices.get(row.selected() as usize) else { return };
        // Another card is a new start for native context, whatever the last one did.
        let context = saving.window.context();
        try_native_again(&context, &saving.machine.borrow());
        saving.machine.borrow_mut().amd_gpu.clone_from(pci);
        saving.save();
    });
    group.add(&row);
    Some(row)
}
