//! The image a Linux the user installed sets itself up from on an AMD card. It's the driver image's
//! label and setup folder, with no driver: setup-own.sh reads `gpu=amd` in its manifest and sets up
//! the guest's Mesa instead. It's made once for each state of the guest's scripts.

use super::driver_image::{
    READ_ONLY, SETUP_ON_IMAGE, VOLUME_LABEL, copy_setup, run, setup_files, sources_id,
};
use crate::failure::Result;
use crate::iso::iso_maker;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST: &str = "gpu=amd\n";

/// The setup image for the scripts in `sources`, made in `folder` if it has to be.
pub fn amd_setup_image(sources: &Path, folder: &Path) -> Result<PathBuf> {
    let id = sources_id(sources, &setup_files(sources))?;
    let image = folder.join(format!("amd-setup-{id}.iso"));
    if image.exists() {
        return Ok(image);
    }
    let staging = folder.join(format!("amd-staging-{}", std::process::id()));
    let made = make(sources, &staging, &image);
    let _ = fs::remove_dir_all(&staging);
    made.map(|()| image)
}

fn make(sources: &Path, staging: &Path, image: &Path) -> Result<()> {
    let content = staging.join("image");
    fs::create_dir_all(&content)?;
    copy_setup(sources, &content.join(SETUP_ON_IMAGE))?;
    fs::write(content.join("manifest"), MANIFEST)?;
    let partial = staging.join("image.iso");
    run(Command::new(iso_maker())
        .args(["-quiet", "-r", "-V", VOLUME_LABEL, "-o"])
        .arg(&partial)
        .arg(&content))?;
    fs::set_permissions(&partial, fs::Permissions::from_mode(READ_ONLY))?;
    fs::rename(&partial, image)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::layout::GUEST_SETUP;
    use crate::testing::Scratch;

    #[test]
    fn the_image_has_the_scripts_and_says_it_is_for_an_amd_card() {
        let scratch = Scratch::new();
        scratch.write(&format!("linux/{GUEST_SETUP}/setup-own.sh"), "echo set up\n");
        let (sources, folder) = (scratch.path("linux"), scratch.path("images"));
        fs::create_dir_all(&folder).unwrap();
        let image = amd_setup_image(&sources, &folder).unwrap();
        assert_eq!(amd_setup_image(&sources, &folder).unwrap(), image, "made once");
        let listing = Command::new("isoinfo").args(["-R", "-f", "-i"]).arg(&image).output().unwrap();
        let listing = String::from_utf8_lossy(&listing.stdout);
        assert!(listing.contains("/setup/setup-own.sh") && listing.contains("/manifest"), "{listing}");
        let manifest =
            Command::new("isoinfo").args(["-R", "-x", "/manifest", "-i"]).arg(&image).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&manifest.stdout), MANIFEST);
        scratch.write(&format!("linux/{GUEST_SETUP}/setup-own.sh"), "echo set up again\n");
        assert_ne!(amd_setup_image(&sources, &folder).unwrap(), image, "new scripts, new image");
    }
}
