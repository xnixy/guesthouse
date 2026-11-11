//! Which card a guest runs on. Each machine keeps its own: the app names an AMD card by its PCI
//! address in [`AMD_CARD_VARIABLE`], or an Intel card in [`INTEL_CARD_VARIABLE`], and with neither
//! the guest is on the NVIDIA card setup chose.

use guesthouse_backend::PciAddress;
use std::env;

pub const AMD_CARD_VARIABLE: &str = "GUESTHOUSE_AMD_CARD";
pub const INTEL_CARD_VARIABLE: &str = "GUESTHOUSE_INTEL_CARD";

/// The AMD card the app named, if it named one.
pub fn amd_card() -> Result<Option<PciAddress>, String> {
    named_in(AMD_CARD_VARIABLE)
}

/// The Intel card the app named, if it named one.
pub fn intel_card() -> Result<Option<PciAddress>, String> {
    named_in(INTEL_CARD_VARIABLE)
}

fn named_in(variable: &str) -> Result<Option<PciAddress>, String> {
    card_from(variable, env::var(variable).ok().as_deref())
}

fn card_from(variable: &str, text: Option<&str>) -> Result<Option<PciAddress>, String> {
    match text.map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) => PciAddress::parse(text)
            .map(Some)
            .ok_or_else(|| format!("{variable} is {text}, which is not a PCI address.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_card_named_is_the_nvidia_card() {
        assert_eq!(card_from(AMD_CARD_VARIABLE, None), Ok(None));
        assert_eq!(card_from(AMD_CARD_VARIABLE, Some("")), Ok(None));
    }

    #[test]
    fn a_named_card_is_read_as_its_address() {
        let card = card_from(AMD_CARD_VARIABLE, Some("0000:7c:00.0")).unwrap().unwrap();
        assert_eq!(card.to_string(), "0000:7c:00.0");
        let card = card_from(INTEL_CARD_VARIABLE, Some("0000:00:02.0")).unwrap().unwrap();
        assert_eq!(card.to_string(), "0000:00:02.0");
    }

    #[test]
    fn a_name_that_is_not_an_address_is_refused_by_its_variable() {
        let refused = card_from(INTEL_CARD_VARIABLE, Some("renderD128")).unwrap_err();
        assert!(refused.starts_with("GUESTHOUSE_INTEL_CARD is renderD128"), "{refused}");
        assert!(refused.contains("not a PCI address"));
    }
}
