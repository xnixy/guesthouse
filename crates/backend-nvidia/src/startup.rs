//! Finding out what is installed, without touching the driver.

use crate::profile::{guest_branch, host_branch, host_branch_of_version, mix_tested};
use guesthouse_backend::{CardLabel, DriverFound, DriverSurvey, DriverVersion, Refusal, Setup};
use std::fs;
use std::path::Path;

const HOST_VERSION_FILE: &str = "/sys/module/nvidia/version";

/// Cards that have been run. This changes nothing the backend does. It is only what a person is told.
const TESTED_DEVICES: [u16; 1] = [0x2B85];

/// Whether this card is one that has been run. A label for people, like [`CardLabel`].
pub fn card_tested(device: u16) -> bool {
    TESTED_DEVICES.contains(&device)
}

/// A Windows driver version's last digits are the vendor's version: `x.y.z6.1742` is `617.42`.
fn guest_version_digits(version: DriverVersion) -> (u32, u32) {
    let [_, _, third, fourth] = version.parts.map(u32::from);
    ((third % 10) * 100 + fourth / 100, fourth % 100)
}

/// The vendor's branch of a guest driver.
pub fn guest_branch_of(version: DriverVersion) -> String {
    guest_version_digits(version).0.to_string()
}

/// Judges a host version against the profiles.
pub fn survey_versions(host_version: &str, setup: &Setup) -> DriverSurvey {
    let branch = host_branch_of_version(host_version);
    let profile = host_branch(branch);
    let (guest_branch_number, guest_point) = guest_version_digits(setup.guest_driver_version);
    let tested_card = setup.card.is_some_and(|card| card_tested(card.device));
    DriverSurvey {
        host: DriverFound { version: host_version.to_owned(), branch: branch.to_owned() },
        guest: DriverFound {
            version: format!("{guest_branch_number}.{guest_point:02}"),
            branch: setup.guest_branch.clone(),
        },
        host_branch_known: profile.is_some(),
        guest_branch_known: guest_branch(&setup.guest_branch).is_some(),
        host_version_tested: profile.is_some_and(|profile| profile.tested_versions.contains(&host_version)),
        mixed_branches_tested: mix_tested(branch, &setup.guest_branch),
        card: if tested_card { CardLabel::Tested } else { CardLabel::Experimental },
    }
}

/// Reads the host's version from the kernel module's own file, which asks nothing of the driver.
pub fn read_host_version(path: &Path) -> Result<String, Refusal> {
    let text = fs::read_to_string(path).map_err(|error| {
        Refusal(format!("The host's NVIDIA driver is not loaded ({}: {error}).", path.display()))
    })?;
    Ok(text.trim().to_owned())
}

pub fn survey(setup: &Setup) -> Result<DriverSurvey, Refusal> {
    Ok(survey_versions(&read_host_version(Path::new(HOST_VERSION_FILE))?, setup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::{PciCard, StartDecision, decide_start};
    use std::path::PathBuf;

    fn setup(guest: &str) -> Setup {
        let guest_driver_version = DriverVersion::parse(guest).unwrap();
        Setup {
            guest_driver_version,
            guest_branch: guest_branch_of(guest_driver_version),
            driver_folder: PathBuf::new(),
            card: Some(PciCard { vendor: 0x10DE, device: 0x2B85, revision: 0xA1 }),
            card_address: None,
        }
    }

    #[test]
    fn a_windows_version_names_the_vendors_branch() {
        let setup = setup("32.0.16.1742");
        assert_eq!(setup.guest_branch, "617");
        assert_eq!(survey_versions("615.78.08", &setup).guest.version, "617.42");
    }

    #[test]
    fn the_tested_pair_runs() {
        assert_eq!(decide_start(&survey_versions("615.78.08", &setup("32.0.16.1742"))), StartDecision::Run);
    }

    #[test]
    fn another_point_release_runs_with_a_warning() {
        let decision = decide_start(&survey_versions("615.80.01", &setup("32.0.16.1742")));
        assert!(matches!(decision, StartDecision::RunWithWarning(_)));
    }

    #[test]
    fn a_host_branch_with_no_profile_is_refused() {
        let decision = decide_start(&survey_versions("999.10.00", &setup("32.0.16.1742")));
        assert!(matches!(decision, StartDecision::Refuse(_)));
    }

    #[test]
    fn branch_620_runs_a_617_guest() {
        let tested = decide_start(&survey_versions("620.70.06", &setup("32.0.16.1742")));
        assert_eq!(tested, StartDecision::Run);
        let other = decide_start(&survey_versions("620.80.00", &setup("32.0.16.1742")));
        assert!(matches!(other, StartDecision::RunWithWarning(_)), "{other:?}");
    }

    #[test]
    fn a_guest_branch_with_no_profile_is_refused() {
        let decision = decide_start(&survey_versions("615.78.08", &setup("32.0.16.2042")));
        let StartDecision::Refuse(reason) = decision else { panic!("it should refuse") };
        assert!(reason.starts_with("The guest driver"), "{reason}");
    }

    #[test]
    fn a_card_never_run_is_labelled_and_treated_the_same() {
        let mut setup = setup("32.0.16.1742");
        setup.card = Some(PciCard { vendor: 0x10DE, device: 0x2C02, revision: 0xA1 });
        let survey = survey_versions("615.78.08", &setup);
        assert_eq!(survey.card, CardLabel::Experimental);
        assert_eq!(decide_start(&survey), StartDecision::Run);
    }

    #[test]
    fn a_driver_that_is_not_loaded_is_said_plainly() {
        assert!(read_host_version(Path::new("/nonexistent")).unwrap_err().0.contains("not loaded"));
    }
}
