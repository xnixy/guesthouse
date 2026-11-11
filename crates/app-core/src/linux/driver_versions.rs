//! Decides whether the host's NVIDIA files may go to a guest.
//!
//! The guest's user-mode driver talks to the kernel module the host has loaded right now, and the two
//! must be the same version. After an update the files on disk are the new version and the loaded
//! module stays the old one until a reboot, so the loaded module is what everything is compared to.

use std::collections::BTreeMap;

/// Three libraries that stand for the driver as a whole. What they end in must all be the loaded version.
pub const DRIVER_LIBRARIES: [&str; 3] = ["libcuda.so.1", "libnvidia-ml.so.1", "libGLX_nvidia.so.0"];
/// Versions a person has run the whole check on. Another point release of a branch runs with a
/// warning, and a branch nobody knows doesn't run, as a new branch can change what the driver asks of
/// the host, on a card the host's own desktop is using.
pub const TESTED_VERSIONS: [&str; 1] = ["615.78.08"];

/// What the host's kernel and files say.
pub trait DriverHost {
    fn loaded_version(&self) -> Option<String>;
    /// The version of one of the driver's libraries, or `None` if the loader can't find it.
    fn library_version(&self, library: &str) -> Option<String>;
    /// Every file of the driver that a guest might want.
    fn driver_files(&self) -> Vec<String>;
}

pub fn installed_versions(host: &dyn DriverHost) -> BTreeMap<String, Option<String>> {
    DRIVER_LIBRARIES.iter().map(|library| ((*library).to_owned(), host.library_version(library))).collect()
}

pub fn branch(version: &str) -> &str {
    version.split('.').next().unwrap_or(version)
}

/// Says why a driver's branch can't run a Linux guest, or `None` if it's known.
pub fn branch_refusal(loaded: &str) -> Option<String> {
    let mut known: Vec<&str> = TESTED_VERSIONS.iter().map(|version| branch(version)).collect();
    known.dedup();
    branch_refusal_among(loaded, &known)
}

/// Says why a driver's branch isn't one of these, or `None` if it is.
pub fn branch_refusal_among(loaded: &str, known: &[&str]) -> Option<String> {
    (!known.contains(&branch(loaded))).then(|| {
        format!(
            "The host's driver {loaded} is from branch {}, and I only know branch {}. Install a driver from \
             that branch.",
            branch(loaded),
            known.join(", ")
        )
    })
}

/// Says in plain words why these files can't be offered, or `None` if they can.
pub fn refusal(loaded: Option<&str>, installed: &BTreeMap<String, Option<String>>) -> Option<String> {
    let Some(loaded) = loaded else {
        return Some(
            "The host has no NVIDIA kernel module loaded, so there is no driver to match.".to_owned(),
        );
    };
    let missing: Vec<&str> =
        installed.iter().filter(|(_, version)| version.is_none()).map(|(name, _)| name.as_str()).collect();
    if !missing.is_empty() {
        return Some(format!(
            "The loader can't find these libraries of NVIDIA's driver: {}.",
            missing.join(", ")
        ));
    }
    let mut different: Vec<&str> =
        installed.values().flatten().map(String::as_str).filter(|version| *version != loaded).collect();
    different.sort_unstable();
    different.dedup();
    (!different.is_empty()).then(|| {
        format!(
            "The host's NVIDIA files are version {}, but its loaded kernel module is {loaded}. A driver update \
             is probably waiting for a reboot. Reboot the host, then start the VM again.",
            different.join(", ")
        )
    })
}

/// A warning for a known branch at a version nobody has run.
pub fn untested_warning(loaded: &str) -> Option<String> {
    (!TESTED_VERSIONS.contains(&loaded)).then(|| {
        format!(
            "Driver {loaded} is a known branch, but this exact version has not been tested. The modules and \
             tables are built for it, and the guest checks CUDA and Vulkan before it turns the GPU on. Sensors \
             show N/A, as the sensor daemon only knows the tested version."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(version: Option<&str>) -> BTreeMap<String, Option<String>> {
        DRIVER_LIBRARIES.iter().map(|name| ((*name).to_owned(), version.map(str::to_owned))).collect()
    }

    #[test]
    fn the_tested_version_and_its_branch_are_known() {
        assert_eq!(branch_refusal("615.78.08"), None);
        assert_eq!(branch_refusal("615.90.01"), None);
    }

    #[test]
    fn an_unknown_branch_is_refused_by_name() {
        let said = branch_refusal("620.11").unwrap();
        assert!(said.contains("620") && said.contains("615"));
    }

    #[test]
    fn another_point_release_only_warns() {
        assert_eq!(untested_warning("615.78.08"), None);
        assert!(untested_warning("615.90.01").unwrap().contains("615.90.01"));
    }

    #[test]
    fn matching_files_can_be_offered() {
        assert_eq!(refusal(Some("615.78.08"), &versions(Some("615.78.08"))), None);
    }

    #[test]
    fn no_module_missing_packages_and_a_pending_reboot_each_say_so() {
        assert!(refusal(None, &versions(None)).unwrap().contains("no NVIDIA kernel module"));
        assert!(refusal(Some("615.78.08"), &versions(None)).unwrap().contains("can't find"));
        let said = refusal(Some("615.78.08"), &versions(Some("615.90.01"))).unwrap();
        assert!(said.contains("615.90.01") && said.contains("Reboot"));
    }
}
