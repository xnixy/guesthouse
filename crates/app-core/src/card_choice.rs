//! The cards a Windows machine's GPU-PV can run on, as the user is offered them. A machine names its
//! card in `amd_gpu`: a card's PCI address, or empty for the NVIDIA card setup chose, and `gpu_maker`
//! says whose it is where the address can't. Any card can be picked when the machine is made. After
//! that it can move only to a card of the same maker.

use crate::context::Context;
use crate::gpu_maker::GpuMaker;
use crate::linux::amd::default_linux_gpu;
use crate::machine::Machine;
use crate::render_cards::render_cards;
use crate::setup::cards::{card_in_use, find_cards};

/// Whether a new machine, or a settings page, is offered Intel's cards. They are, as a test stage:
/// Windows can be set up and started on one, but nothing is drawn by the card yet.
const OFFER_INTEL_CARDS: bool = true;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardChoice {
    pub maker: GpuMaker,
    /// What goes in the machine's `amd_gpu`.
    pub pci: String,
    pub label: String,
    /// The card's PCI device id, 0 where it isn't known.
    pub device: u32,
}

fn offered(maker: GpuMaker, intel_offered: bool) -> bool {
    match maker {
        GpuMaker::Nvidia | GpuMaker::Amd => true,
        GpuMaker::Intel => intel_offered,
    }
}

/// The card setup chose, if this PC has an NVIDIA card.
fn nvidia_choice(context: &Context) -> Option<CardChoice> {
    if find_cards(context).is_empty() {
        return None;
    }
    let card = card_in_use(context);
    let label = card.as_ref().map_or_else(|| "NVIDIA".to_owned(), |card| card.name.clone());
    Some(CardChoice {
        maker: GpuMaker::Nvidia,
        pci: String::new(),
        label,
        device: card.map_or(0, |card| card.device),
    })
}

fn choices_offering(context: &Context, intel_offered: bool) -> Vec<CardChoice> {
    let mut choices = Vec::new();
    for maker in GpuMaker::ALL.into_iter().filter(|maker| offered(*maker, intel_offered)) {
        match maker {
            GpuMaker::Nvidia => choices.extend(nvidia_choice(context)),
            GpuMaker::Amd | GpuMaker::Intel => {
                choices.extend(render_cards(context, maker).into_iter().map(|card| CardChoice {
                    maker,
                    label: format!("{} at {}", card.name, card.pci),
                    pci: card.pci,
                    device: card.device,
                }));
            }
        }
    }
    choices
}

/// Every card here a new Windows machine could run on, NVIDIA's first.
pub fn windows_card_choices(context: &Context) -> Vec<CardChoice> {
    choices_offering(context, OFFER_INTEL_CARDS)
}

/// The cards an existing machine may move to: those of its own maker. The one it has stays listed even
/// when it's gone from this PC, so the choice still shows it.
pub fn same_maker_choices(context: &Context, machine: &Machine) -> Vec<CardChoice> {
    let mut choices: Vec<CardChoice> = windows_card_choices(context)
        .into_iter()
        .filter(|choice| machine.card_change_problem(choice.maker).is_none())
        .collect();
    if !choices.iter().any(|choice| choice.pci == machine.amd_gpu) {
        let label = format!("The card at {}, which isn't in this PC now", machine.amd_gpu);
        let maker = machine.windows_maker().unwrap_or(GpuMaker::Nvidia);
        choices.push(CardChoice { maker, pci: machine.amd_gpu.clone(), label, device: 0 });
    }
    choices
}

/// Which of `choices` a new machine starts on: NVIDIA's card if this PC has one, and else the first AMD.
pub fn default_choice(context: &Context, choices: &[CardChoice]) -> usize {
    let usual = default_linux_gpu(context);
    choices.iter().position(|choice| choice.pci == usual).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::amd::stand_in::fake_render_node;
    use crate::setup::cards::stand_in::{NAMES, fake_pci};
    use crate::testing::{Scratch, scratch_context};

    const IGPU: &str = "0000:7c:00.0";

    const INTEL_IGPU: &str = "0000:00:02.0";

    fn with_cards(scratch: &Scratch, nvidia: bool) -> Context {
        let mut context = scratch_context(scratch);
        context.pci_devices = scratch.path("pci");
        context.pci_names = scratch.write("pci.ids", NAMES);
        if nvidia {
            fake_pci(scratch, "0000:01:00.0", 0x2B85, "0x10de", "0x030000");
        }
        fake_render_node(scratch, 128, IGPU, "0x1002", 0x13c0);
        fake_render_node(scratch, 129, INTEL_IGPU, "0x8086", 0x64a0);
        context
    }

    #[test]
    fn a_new_machine_is_offered_every_card_and_starts_on_nvidias() {
        let scratch = Scratch::new();
        let context = with_cards(&scratch, true);
        let choices = windows_card_choices(&context);
        let addresses: Vec<&str> = choices.iter().map(|choice| choice.pci.as_str()).collect();
        assert_eq!(addresses, ["", IGPU, INTEL_IGPU]);
        assert!(choices[0].label.contains("RTX 5090"), "{choices:?}");
        assert!(choices[1].label.ends_with("at 0000:7c:00.0"), "{choices:?}");
        assert_eq!((choices[0].device, choices[1].device), (0x2B85, 0x13C0));
        assert_eq!(default_choice(&context, &choices), 0);
    }

    #[test]
    fn with_no_nvidia_card_a_new_machine_starts_on_amds() {
        let scratch = Scratch::new();
        let context = with_cards(&scratch, false);
        let choices = windows_card_choices(&context);
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[default_choice(&context, &choices)].pci, IGPU);
    }

    #[test]
    fn a_machine_is_offered_only_its_own_makers_cards() {
        let scratch = Scratch::new();
        let context = with_cards(&scratch, true);
        let on_nvidia = Machine::new("w", "W", "/d", "/v");
        let offered = same_maker_choices(&context, &on_nvidia);
        assert_eq!(offered.iter().map(|choice| choice.pci.as_str()).collect::<Vec<_>>(), [""]);
        let on_gone_amd = Machine { amd_gpu: "0000:0a:00.0".into(), ..on_nvidia };
        let offered = same_maker_choices(&context, &on_gone_amd);
        let addresses: Vec<&str> = offered.iter().map(|choice| choice.pci.as_str()).collect();
        assert_eq!(addresses, [IGPU, "0000:0a:00.0"]);
        assert!(offered[1].label.contains("isn't in this PC now"));
    }

    #[test]
    fn intel_cards_are_offered_after_the_others_while_the_switch_is_on_and_not_when_it_is_off() {
        let scratch = Scratch::new();
        let context = with_cards(&scratch, true);
        let offered_now: Vec<GpuMaker> =
            windows_card_choices(&context).iter().map(|choice| choice.maker).collect();
        assert_eq!(offered_now, [GpuMaker::Nvidia, GpuMaker::Amd, GpuMaker::Intel]);
        let without: Vec<GpuMaker> =
            choices_offering(&context, false).iter().map(|choice| choice.maker).collect();
        assert_eq!(without, [GpuMaker::Nvidia, GpuMaker::Amd]);
        let all = choices_offering(&context, true);
        let intel: Vec<&str> = all
            .iter()
            .filter(|choice| choice.maker == GpuMaker::Intel)
            .map(|choice| choice.pci.as_str())
            .collect();
        assert_eq!(intel, [INTEL_IGPU]);
    }

    #[test]
    fn a_machine_on_intel_is_offered_only_intels_cards() {
        let scratch = Scratch::new();
        let context = with_cards(&scratch, true);
        let mut machine = Machine::new("w", "W", "/d", "/v");
        machine.use_windows_card(GpuMaker::Intel, INTEL_IGPU);
        // Listing as the switch will, with Intel on.
        let own: Vec<String> = choices_offering(&context, true)
            .into_iter()
            .filter(|choice| machine.card_change_problem(choice.maker).is_none())
            .map(|choice| choice.pci)
            .collect();
        assert_eq!(own, [INTEL_IGPU]);
    }
}
