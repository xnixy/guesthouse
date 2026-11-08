//! Which cards are here, and what is known about each. The list is only a label for the user: what a
//! card can do is read from the card.

use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::machine::sorted_names;
use crate::settings::{CARD_MEMORY_FIELD, read_setup, write_setup};
use crate::usb::sysfs_text;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const NVIDIA_VENDOR: &str = "0x10de";
const NVIDIA_NAMES_START: &str = "\n10de  ";
const DISPLAY_CLASS: &str = "0x03";
/// The card this has been run on.
const TESTED_CARDS: [u32; 1] = [0x2B85];
/// Consumer 50, 40, 20 and 16 series, from pci.ids, which may be used under the 3-clause BSD licence. The workstation cards on these chips don't count, as nothing
/// says their answers are the same. The 30 series has no profile, so it isn't here.
const CONSUMER_CARDS: [(&str, &[u32]); 15] = [
    ("GB202", &[0x2B85, 0x2B87, 0x2B8C]),
    ("GB203", &[0x2C02, 0x2C05, 0x2C09, 0x2C18, 0x2C19, 0x2C58, 0x2C59]),
    ("GB205", &[0x2F04, 0x2F06, 0x2F18, 0x2F58]),
    ("GB206", &[0x2D04, 0x2D05, 0x2D18, 0x2D19, 0x2D58, 0x2D59]),
    ("GB207", &[0x2D83, 0x2D98, 0x2DD8]),
    ("AD102", &[0x2684, 0x2685, 0x2689]),
    ("AD103", &[0x2702, 0x2703, 0x2704, 0x2705, 0x2709, 0x2717, 0x2757]),
    ("AD104", &[0x2782, 0x2783, 0x2786, 0x2788, 0x27A0, 0x27E0]),
    ("AD106", &[0x2803, 0x2805, 0x2808, 0x2820, 0x2822, 0x2860]),
    ("AD107", &[0x2882, 0x28A0, 0x28A1, 0x28A3, 0x28E0, 0x28E1, 0x28E3]),
    ("TU102", &[0x1E03, 0x1E04, 0x1E07]),
    (
        "TU104",
        &[
            0x1E81, 0x1E82, 0x1E84, 0x1E87, 0x1E89, 0x1E90, 0x1E91, 0x1E93, 0x1EC2, 0x1EC7, 0x1ED0, 0x1ED1,
            0x1ED3,
        ],
    ),
    (
        "TU106",
        &[
            0x1F02, 0x1F03, 0x1F06, 0x1F07, 0x1F08, 0x1F09, 0x1F0A, 0x1F10, 0x1F11, 0x1F12, 0x1F14, 0x1F15,
            0x1F42, 0x1F47, 0x1F50, 0x1F51, 0x1F54, 0x1F55,
        ],
    ),
    ("TU116", &[0x2182, 0x2184, 0x2187, 0x2188, 0x2191, 0x2192, 0x21C4, 0x21D1]),
    ("TU117", &[0x1F82, 0x1F83, 0x1F91, 0x1F92, 0x1F94, 0x1F95, 0x1F96, 0x1F99, 0x1F9D, 0x1FD9, 0x1FDD]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Support {
    Tested,
    Experimental,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub device: u32,
    pub name: String,
    /// Empty if it isn't a chip this knows.
    pub chip: String,
    pub support: Support,
    pub revision: u32,
    /// Where the card is on the PCI bus, as sysfs names it, like `0000:01:00.0`. This is what the daemon
    /// opens the card by. A setup written before it was kept has none.
    #[serde(default)]
    pub pci: String,
}

/// A PCI address as sysfs writes it, whatever width the domain was given in: `nvidia-smi` writes
/// eight digits and sysfs four.
fn normal_address(text: &str) -> String {
    let text = text.trim().to_lowercase();
    match text.split_once(':') {
        Some((domain, rest)) if rest.contains(':') => match u32::from_str_radix(domain, 16) {
            Ok(domain) => format!("{domain:04x}:{rest}"),
            Err(_) => text,
        },
        _ => text,
    }
}

/// The memory in MiB of the card at this address, from `nvidia-smi`'s lines of address and memory.
fn parse_card_memory(said: &str, address: &str) -> Option<u64> {
    let wanted = normal_address(address);
    said.lines().find_map(|line| {
        let (at, memory) = line.split_once(',')?;
        (normal_address(at) == wanted).then(|| memory.trim().parse().ok())?
    })
}

fn card_memory_mib(address: &str) -> Option<u64> {
    let said = Command::new("nvidia-smi")
        .args(["--query-gpu=pci.bus_id,memory.total", "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    parse_card_memory(&String::from_utf8_lossy(&said.stdout), address)
}

/// Writes down the card for the daemon, by its PCI address, and how much memory it has for the video
/// memory cap. It's the best supported card, and of those the first by address.
pub fn remember_card(context: &Context) -> Result<()> {
    remember_card_with(context, card_memory_mib)
}

fn remember_card_with(context: &Context, memory_of: impl Fn(&str) -> Option<u64>) -> Result<()> {
    let cards = find_cards(context);
    let card = cards.first().ok_or_else(|| Failure::new("No NVIDIA card was found."))?;
    let memory = memory_of(&card.pci).unwrap_or(0);
    let changes = json!({"card": card, CARD_MEMORY_FIELD: memory});
    write_setup(context, changes.as_object().cloned().unwrap_or_default())
}

fn chip_of(device: u32) -> &'static str {
    CONSUMER_CARDS.iter().find(|(_, devices)| devices.contains(&device)).map_or("", |found| found.0)
}

fn support_of(device: u32) -> Support {
    if TESTED_CARDS.contains(&device) {
        Support::Tested
    } else if chip_of(device).is_empty() {
        Support::Unsupported
    } else {
        Support::Experimental
    }
}

/// The card's name in pci.ids, in the brackets if it has them, as those hold the product.
pub fn card_name(device: u32, names: &Path) -> String {
    let name = product_name(NVIDIA_NAMES_START, device, names);
    name.map_or_else(|| format!("NVIDIA card {device:04x}"), |name| format!("NVIDIA {name}"))
}

/// A device's product in pci.ids, under the vendor whose list starts with `vendor_start`: what's in the
/// brackets if its line has them, or else the whole line.
pub fn product_name(vendor_start: &str, device: u32, names: &Path) -> Option<String> {
    let data = fs::read(names).ok()?;
    let text = String::from_utf8_lossy(&data);
    let start = text.find(vendor_start)?;
    let vendor = &text[start + 1..];
    let wanted = format!("\t{device:04x}  ");
    let mut lines = vendor.lines().skip(1).take_while(|line| line.starts_with('\t') || line.starts_with('#'));
    let line = lines.find_map(|line| line.strip_prefix(&wanted))?;
    let bracketed =
        line.find('[').and_then(|start| Some(&line[start + 1..start + line[start..].rfind(']')?]));
    Some(bracketed.unwrap_or(line).to_owned())
}

fn hex_number(text: &str) -> Option<u32> {
    u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

/// The card setup wrote down, if it's still in this PC. One from a setup that kept no address can't be
/// told from another card of the same model, so it's only taken on a PC with one NVIDIA card.
pub fn chosen_card(context: &Context) -> Option<Card> {
    let setup = read_setup(context);
    let wanted = setup.get("card")?.get("pci").and_then(|pci| pci.as_str()).unwrap_or_default();
    let cards = find_cards(context);
    match (wanted, cards.as_slice()) {
        ("", [only]) => Some(only.clone()),
        ("", _) => None,
        (wanted, _) => cards.into_iter().find(|card| card.pci == normal_address(wanted)),
    }
}

/// The card a GPU machine will run on: the one setup chose, or before setup, the one it would choose.
pub fn card_in_use(context: &Context) -> Option<Card> {
    if read_setup(context).contains_key("card") {
        chosen_card(context)
    } else {
        find_cards(context).into_iter().next()
    }
}

/// Why a GPU machine can't start on the card setup chose, if it can't.
pub fn card_problem(context: &Context) -> Option<String> {
    let setup = read_setup(context);
    let wanted = setup.get("card")?.get("pci").and_then(|pci| pci.as_str()).unwrap_or_default();
    if chosen_card(context).is_some() {
        return None;
    }
    Some(if wanted.is_empty() {
        "This PC has more than one NVIDIA card, and setup didn't write down which one to use. Run Setup Again \
         from the menu."
            .to_owned()
    } else {
        format!(
            "The NVIDIA card setup chose, at {wanted}, isn't in this PC now. Run Setup Again from the menu."
        )
    })
}

/// Which card a GPU machine uses, said only on a PC with more than one NVIDIA card, where it's a question.
pub fn card_in_use_note(context: &Context) -> Option<String> {
    // This runs every second for each GPU machine, so a PC with one card reads no names.
    if nvidia_card_folders(context).len() < 2 {
        return None;
    }
    let card = card_in_use(context)?;
    Some(format!("Runs on the {} at {}.", card.name, card.pci))
}

/// The sysfs folders of NVIDIA's display devices, by PCI address.
fn nvidia_card_folders(context: &Context) -> Vec<String> {
    let root = &context.pci_devices;
    let is_card = |name: &String| {
        let text = |field: &str| sysfs_text(&root.join(name).join(field));
        text("vendor") == NVIDIA_VENDOR && text("class").starts_with(DISPLAY_CLASS)
    };
    sorted_names(root).into_iter().filter(is_card).collect()
}

/// NVIDIA's display devices, the best supported first, and by PCI address after that.
pub fn find_cards(context: &Context) -> Vec<Card> {
    let mut cards = Vec::new();
    for name in nvidia_card_folders(context) {
        let text = |field: &str| sysfs_text(&context.pci_devices.join(&name).join(field));
        let Some(device) = hex_number(&text("device")) else { continue };
        cards.push(Card {
            device,
            name: card_name(device, &context.pci_names),
            chip: chip_of(device).to_owned(),
            support: support_of(device),
            revision: hex_number(&text("revision")).unwrap_or(0),
            pci: normal_address(&name),
        });
    }
    cards.sort_by_key(|card| card.support);
    cards
}

#[cfg(test)]
pub mod stand_in {
    use crate::testing::Scratch;

    pub fn fake_pci(scratch: &Scratch, name: &str, device: u32, vendor: &str, class: &str) {
        scratch.write(&format!("pci/{name}/vendor"), format!("{vendor}\n"));
        scratch.write(&format!("pci/{name}/class"), format!("{class}\n"));
        scratch.write(&format!("pci/{name}/device"), format!("0x{device:04x}\n"));
    }

    pub const NAMES: &str = "10dd  Other\n\t2c02  Not this one\n10de  NVIDIA Corporation\n\
                             \t2c02  GB203 [GeForce RTX 5080]\n\t2b85  GB202 [GeForce RTX 5090]\n\
                             \t2bb1  GB202GL [RTX PRO 6000]\n\t1b80  Plain name\n\
                             \t2704  AD103 [GeForce RTX 4080]\n\t21c4  TU116 [GeForce GTX 1660 SUPER]\n\
                             \t2204  GA102 [GeForce RTX 3090]\n10df  Next\n";
}

#[cfg(test)]
mod tests {
    use super::stand_in::{NAMES, fake_pci};
    use super::*;
    use crate::testing::{Scratch, scratch_context};
    use serde_json::json;

    const DISPLAY: &str = "0x030000";
    const NVIDIA: &str = "0x10de";

    fn context(scratch: &Scratch) -> Context {
        let mut context = scratch_context(scratch);
        context.pci_devices = scratch.path("pci");
        context.pci_names = scratch.write("pci.ids", NAMES);
        context
    }

    #[test]
    fn the_cards_memory_is_the_one_on_its_address() {
        assert_eq!(parse_card_memory("00000000:02:00.0, 32607\n", "0000:02:00.0"), Some(32607));
        let two = "00000000:01:00.0, 16376\n00000000:02:00.0, 6144\n";
        assert_eq!(parse_card_memory(two, "0000:02:00.0"), Some(6144));
        assert_eq!(parse_card_memory(two, "0000:01:00.0"), Some(16376));
        assert_eq!(parse_card_memory(two, "0000:03:00.0"), None);
        assert_eq!(parse_card_memory("Failed to initialize NVML\n", "0000:02:00.0"), None);
    }

    #[test]
    fn workstation_cards_do_not_count() {
        let scratch = Scratch::new();
        let context = context(&scratch);
        fake_pci(&scratch, "0000:00:01.0", 0x2B85, "0x1002", DISPLAY);
        fake_pci(&scratch, "0000:01:00.0", 0x2BB1, NVIDIA, DISPLAY);
        fake_pci(&scratch, "0000:01:00.1", 0x2B85, NVIDIA, "0x040300");
        let cards = find_cards(&context);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].support, Support::Unsupported);
        assert_eq!(cards[0].name, "NVIDIA RTX PRO 6000");
    }

    #[test]
    fn the_best_supported_card_comes_first() {
        let scratch = Scratch::new();
        let context = context(&scratch);
        fake_pci(&scratch, "0000:01:00.0", 0x2BB1, NVIDIA, DISPLAY);
        fake_pci(&scratch, "0000:02:00.0", 0x2C02, NVIDIA, DISPLAY);
        fake_pci(&scratch, "0000:03:00.0", 0x2B85, NVIDIA, DISPLAY);
        let cards = find_cards(&context);
        let found: Vec<(Support, &str)> =
            cards.iter().map(|card| (card.support, card.chip.as_str())).collect();
        assert_eq!(
            found,
            [(Support::Tested, "GB202"), (Support::Experimental, "GB203"), (Support::Unsupported, "")]
        );
        assert_eq!(
            (cards[0].name.as_str(), cards[1].name.as_str()),
            ("NVIDIA GeForce RTX 5090", "NVIDIA GeForce RTX 5080")
        );
    }

    #[test]
    fn the_40_and_16_series_are_experimental_and_named_as_pci_ids_has_them() {
        let scratch = Scratch::new();
        let context = context(&scratch);
        fake_pci(&scratch, "0000:01:00.0", 0x2704, NVIDIA, DISPLAY);
        fake_pci(&scratch, "0000:02:00.0", 0x21C4, NVIDIA, DISPLAY);
        let cards = find_cards(&context);
        let found: Vec<(Support, &str, &str)> =
            cards.iter().map(|card| (card.support, card.chip.as_str(), card.name.as_str())).collect();
        assert_eq!(
            found,
            [
                (Support::Experimental, "AD103", "NVIDIA GeForce RTX 4080"),
                (Support::Experimental, "TU116", "NVIDIA GeForce GTX 1660 SUPER"),
            ]
        );
    }

    #[test]
    fn the_30_series_has_no_profile_and_stays_unsupported() {
        assert_eq!(support_of(0x2204), Support::Unsupported);
    }

    #[test]
    fn a_name_comes_from_pci_ids_or_from_the_number() {
        let scratch = Scratch::new();
        let names = scratch.write("pci.ids", NAMES);
        assert_eq!(card_name(0x1B80, &names), "NVIDIA Plain name");
        assert_eq!(card_name(0x9999, &names), "NVIDIA card 9999");
        assert_eq!(card_name(0x2B85, &scratch.path("nothing")), "NVIDIA card 2b85");
    }

    /// The tester's PC: a 4080 and a 1660 SUPER, both experimental, so the first by address is chosen.
    fn two_cards(scratch: &Scratch) -> Context {
        let context = context(scratch);
        fake_pci(scratch, "0000:02:00.0", 0x21C4, NVIDIA, DISPLAY);
        fake_pci(scratch, "0000:01:00.0", 0x2704, NVIDIA, DISPLAY);
        context
    }

    fn remember(context: &Context, card: serde_json::Value) {
        write_setup(context, json!({ "card": card }).as_object().cloned().unwrap()).unwrap();
    }

    #[test]
    fn of_two_cards_setup_writes_down_the_address_of_the_one_it_chose() {
        let scratch = Scratch::new();
        let context = two_cards(&scratch);
        let cards = find_cards(&context);
        let found: Vec<&str> = cards.iter().map(|card| card.pci.as_str()).collect();
        assert_eq!(found, ["0000:01:00.0", "0000:02:00.0"], "the same support, so by address");
        let said = "00000000:02:00.0, 6144\n00000000:01:00.0, 16376\n";
        remember_card_with(&context, |address| parse_card_memory(said, address)).unwrap();
        let setup = read_setup(&context);
        assert_eq!(setup["card"]["pci"], "0000:01:00.0");
        assert_eq!(setup[CARD_MEMORY_FIELD], 16376, "the chosen card's memory, not the first line's");
        assert_eq!(setup["card"]["device"], 0x2704);
        assert_eq!(chosen_card(&context).map(|card| card.name).as_deref(), Some("NVIDIA GeForce RTX 4080"));
    }

    #[test]
    fn the_card_setup_chose_is_kept_by_its_address_not_its_place_in_the_list() {
        let scratch = Scratch::new();
        let context = two_cards(&scratch);
        remember(&context, json!({"device": 0x21C4, "pci": "0000:02:00.0"}));
        let chosen = chosen_card(&context).unwrap();
        assert_eq!((chosen.pci.as_str(), chosen.device), ("0000:02:00.0", 0x21C4));
        assert_eq!(card_problem(&context), None);
        let note = card_in_use_note(&context).unwrap();
        assert_eq!(note, "Runs on the NVIDIA GeForce GTX 1660 SUPER at 0000:02:00.0.");
    }

    #[test]
    fn two_cards_and_a_setup_with_no_address_or_a_gone_one_say_to_run_setup_again() {
        let scratch = Scratch::new();
        let context = two_cards(&scratch);
        remember(&context, json!({"device": 0x2704}));
        assert_eq!(chosen_card(&context), None);
        assert!(card_problem(&context).unwrap().contains("more than one NVIDIA card"));
        remember(&context, json!({"device": 0x2704, "pci": "0000:03:00.0"}));
        assert!(card_problem(&context).unwrap().contains("at 0000:03:00.0, isn't in this PC now"));
    }

    #[test]
    fn one_card_is_as_before_with_or_without_an_address_and_says_nothing_more() {
        let scratch = Scratch::new();
        let context = context(&scratch);
        fake_pci(&scratch, "0000:01:00.0", 0x2B85, NVIDIA, DISPLAY);
        assert_eq!(
            card_in_use(&context).map(|card| card.pci).as_deref(),
            Some("0000:01:00.0"),
            "before setup"
        );
        remember(&context, json!({"device": 0x2B85}));
        assert_eq!(chosen_card(&context).map(|card| card.device), Some(0x2B85), "an old setup");
        assert_eq!(card_problem(&context), None);
        assert_eq!(card_in_use_note(&context), None);
        remember(&context, json!({"device": 0x2B85, "pci": "0000:01:00.0"}));
        assert_eq!(card_problem(&context), None);
        assert_eq!(card_in_use_note(&context), None);
    }

    #[test]
    fn nvidia_smi_and_sysfs_write_the_same_address() {
        assert_eq!(normal_address("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normal_address("0000:0A:00.0\n"), "0000:0a:00.0");
    }

    #[test]
    fn no_folder_is_no_cards() {
        let scratch = Scratch::new();
        assert!(find_cards(&context(&scratch)).is_empty());
    }
}
