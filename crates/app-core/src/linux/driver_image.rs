//! Makes the read-only image that carries the host's driver to a guest. It's made once per driver
//! version and state of the guest's sources. An image already made for the loaded driver is used as it
//! is, even if the files on disk have since moved on, because it still matches what is loaded.

use super::driver_files;
use super::driver_versions::{
    DriverHost, TESTED_VERSIONS, branch_refusal, installed_versions, refusal, untested_warning,
};
use super::gpu_off::{
    GUEST_DRIVER_FILES, GUEST_NO_SOURCE, GUEST_TABLES_DIFFER, GUEST_UNKNOWN_DRIVER, GpuOff,
};
use super::layout::{BUILT_SOURCES, GUEST_SETUP};
use super::tables::{TABLE_FILES, match_the_device, write_tables};
use crate::failure::Result;
use crate::iso::iso_maker;
use crate::machine::sorted_names;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) const VOLUME_LABEL: &str = "NVFWD_DRIVER";
/// The folders the guest's modules include from, and only their headers.
const HEADER_FOLDERS: [&str; 8] = [
    "src/nvidia-modeset/kapi/interface",
    "src/nvidia-modeset/interface",
    "src/common/sdk/nvidia/inc",
    "src/common/modeset/timing",
    "src/common/unix/common/inc",
    "src/common/modeset/hdmipacket",
    "src/common/inc",
    "src/nvidia/arch/nvalloc/unix/include",
];
const SOURCE_ID_LENGTH: usize = 12;
/// Where the guest's scripts go on the image, for a Linux the user installed to set itself up from.
pub(super) const SETUP_ON_IMAGE: &str = "setup";
pub(super) const READ_ONLY: u32 = 0o444;

/// What came of asking for an image.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The image to attach, and a warning if this version is untested.
    Ready { image: PathBuf, warning: Option<String> },
    /// Why the VM starts without a GPU.
    Refused(GpuOff),
}

fn source_version(tree: &Path) -> Option<String> {
    let text = fs::read_to_string(tree.join("version.mk")).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "NVIDIA_VERSION").then(|| value.trim().to_owned())
    })
}

fn walk(root: &Path, name: &str, found: &mut Vec<String>) {
    let path = root.join(name);
    if path.is_dir() {
        for child in sorted_names(&path) {
            walk(root, &format!("{name}/{child}"), found);
        }
    } else if path.is_file() {
        found.push(name.to_owned());
    }
}

fn our_files(sources: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for name in BUILT_SOURCES {
        walk(sources, name, &mut found);
    }
    found.sort();
    found
}

/// The guest's scripts, which go on the image as they are. They aren't built, so they're no part of
/// the manifest's id, and a new script doesn't rebuild a guest that doesn't use them.
pub(super) fn setup_files(sources: &Path) -> Vec<String> {
    let mut found = Vec::new();
    walk(sources, GUEST_SETUP, &mut found);
    found.sort();
    found
}

/// A copy keeps each file's mode, and the image keeps whether a file runs.
pub(super) fn copy_setup(sources: &Path, to: &Path) -> Result<()> {
    for name in setup_files(sources) {
        let target = to.join(&name[GUEST_SETUP.len() + 1..]);
        if let Some(folder) = target.parent() {
            fs::create_dir_all(folder)?;
        }
        fs::copy(sources.join(&name), target)?;
    }
    Ok(())
}

/// Changes when the guest's sources do, so a guest rebuilds when they move on.
pub(super) fn sources_id(sources: &Path, names: &[String]) -> Result<String> {
    let mut child = Command::new("sha256sum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut input = child.stdin.take().ok_or_else(|| crate::Failure::new("sha256sum took no input."))?;
    for name in names {
        input.write_all(name.as_bytes())?;
        input.write_all(&fs::read(sources.join(name))?)?;
    }
    drop(input);
    let said = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&said.stdout).chars().take(SOURCE_ID_LENGTH).collect())
}

pub(super) fn run(command: &mut Command) -> Result<()> {
    let result = command.output()?;
    if !result.status.success() {
        let said = String::from_utf8_lossy(&result.stderr);
        return Err(crate::Failure(format!("{:?} failed: {}", command.get_program(), said.trim())));
    }
    Ok(())
}

fn tar(archive: &Path, verb: &str, prefix: &str, root: &Path, names: &[String]) -> Result<()> {
    run(Command::new("tar")
        .arg(verb)
        .arg(archive)
        .args(["--owner=root", "--group=root", "--numeric-owner"])
        .arg(format!("--transform=s,^,{prefix},"))
        .arg("-C")
        .arg(root)
        .args(names))
}

fn headers(tree: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for folder in HEADER_FOLDERS {
        walk(tree, folder, &mut found);
    }
    found.retain(|name| name.ends_with(".h"));
    found
}

fn write_sources(path: &Path, sources: &Path, tree: &Path, names: &[String], tables: &Path) -> Result<()> {
    let generated = |name: &&String| TABLE_FILES.iter().any(|table| name.ends_with(*table));
    let plain: Vec<String> = names.iter().filter(|name| !generated(name)).cloned().collect();
    tar(path, "-cf", "nvfwd/", sources, &plain)?;
    let made: Vec<String> = TABLE_FILES.iter().map(|name| (*name).to_owned()).collect();
    tar(path, "-rf", "nvfwd/guest/", tables, &made)?;
    tar(path, "-rf", "nvidia/", tree, &["kernel-open".to_owned()])?;
    tar(path, "-rf", "nvidia/", tree, &headers(tree))
}

/// Where a link goes, if that's beside it. Debian's links lead out through `/etc/alternatives`, and
/// those would dangle in a guest, so what they point to is copied instead.
fn sibling_link(source: &Path) -> Option<PathBuf> {
    fs::read_link(source).ok().filter(|target| target.components().count() == 1)
}

fn copy_file(source: &Path, target: &Path, place: &str) -> Result<()> {
    if let Some(folder) = target.parent() {
        fs::create_dir_all(folder)?;
    }
    if place.ends_with("nvidia_icd.x86_64.json") {
        fs::write(target, driver_files::rewrite_icd(&fs::read_to_string(source)?))?;
    } else if let Some(sibling) = sibling_link(source) {
        symlink(sibling, target)?;
    } else {
        fs::copy(source, target)?;
    }
    Ok(())
}

/// Everything about the host that can refuse the image, before anything is built.
fn check_before_building(host: &dyn DriverHost, tree: &Path) -> std::result::Result<String, GpuOff> {
    let Some(loaded) = host.loaded_version() else {
        let detail = refusal(None, &installed_versions(host)).unwrap_or_default();
        return Err(GpuOff::new(GUEST_DRIVER_FILES, detail));
    };
    if let Some(reason) = branch_refusal(&loaded) {
        return Err(GpuOff::new(GUEST_UNKNOWN_DRIVER, reason));
    }
    if source_version(tree).as_deref() != Some(&loaded) {
        let detail = format!(
            "{} is not NVIDIA's source at version {loaded}, and the guest's modules need its headers. Get it \
             with: git clone --depth 1 --branch {loaded} https://github.com/NVIDIA/open-gpu-kernel-modules",
            tree.display()
        );
        return Err(GpuOff::new(GUEST_NO_SOURCE, detail));
    }
    Ok(loaded)
}

/// A driver update or a reboot during the copy must not leave a mixed image.
fn still_matches(host: &dyn DriverHost, loaded: &str) -> bool {
    let now = host.loaded_version();
    now.as_deref() == Some(loaded) && refusal(now.as_deref(), &installed_versions(host)).is_none()
}

fn build(
    host: &dyn DriverHost,
    sources: &Path,
    tree: &Path,
    pairs: &[(String, String)],
    names: &[String],
    (loaded, id): (&str, &str),
    (staging, image): (&Path, &Path),
) -> Result<Option<GpuOff>> {
    let tables = staging.join("tables");
    fs::create_dir_all(&tables)?;
    write_tables(tree, &tables)?;
    if !match_the_device(&tables, loaded)? {
        let detail = format!(
            "Driver {loaded} changes calls that the nvfwd device checks, so this QEMU can't forward them \
             safely. It needs a Guesthouse that knows this driver."
        );
        return Ok(Some(GpuOff::new(GUEST_TABLES_DIFFER, detail)));
    }
    let content = staging.join("image");
    fs::create_dir_all(&content)?;
    for (source, place) in pairs {
        copy_file(Path::new(source), &content.join(place), place)?;
    }
    write_sources(&content.join("sources.tar"), sources, tree, names, &tables)?;
    copy_setup(sources, &content.join(SETUP_ON_IMAGE))?;
    let tested = if TESTED_VERSIONS.contains(&loaded) { "yes" } else { "no" };
    fs::write(content.join("manifest"), format!("version={loaded}\nid={id}\ntested={tested}\n"))?;
    let partial = staging.join("image.iso");
    // -r makes every file root's and world readable. Rock Ridge keeps the library links.
    run(Command::new(iso_maker())
        .args(["-quiet", "-r", "-V", VOLUME_LABEL, "-o"])
        .arg(&partial)
        .arg(&content))?;
    if !still_matches(host, loaded) {
        let detail = "The host's driver changed while the image was being made. Start the VM again.";
        return Ok(Some(GpuOff::new(GUEST_DRIVER_FILES, detail)));
    }
    fs::set_permissions(&partial, fs::Permissions::from_mode(READ_ONLY))?;
    fs::rename(&partial, image)?;
    Ok(None)
}

/// The image for the loaded driver, made if it has to be. `sources` is what ships with the app, `tree`
/// NVIDIA's open source at the loaded version, and `folder` where images are kept.
pub fn prepare(host: &dyn DriverHost, sources: &Path, tree: &Path, folder: &Path) -> Result<Outcome> {
    let loaded = match check_before_building(host, tree) {
        Ok(loaded) => loaded,
        Err(reason) => return Ok(Outcome::Refused(reason)),
    };
    let names = our_files(sources);
    let id = format!("{loaded}-{}", sources_id(sources, &names)?);
    let setup_id = sources_id(sources, &setup_files(sources))?;
    let image = folder.join(format!("driver-{id}-{setup_id}.iso"));
    let warning = untested_warning(&loaded);
    if image.exists() {
        return Ok(Outcome::Ready { image, warning });
    }
    if let Some(reason) = refusal(Some(&loaded), &installed_versions(host)) {
        return Ok(Outcome::Refused(GpuOff::new(GUEST_DRIVER_FILES, reason)));
    }
    let pairs = driver_files::chosen(&host.driver_files());
    let places: Vec<String> = pairs.iter().map(|(_, place)| place.clone()).collect();
    let lacking = driver_files::missing(&places, &loaded);
    if !lacking.is_empty() {
        let detail = format!("The driver image would lack:\n  {}", lacking.join("\n  "));
        return Ok(Outcome::Refused(GpuOff::new(GUEST_DRIVER_FILES, detail)));
    }
    let staging = folder.join(format!("staging-{}", std::process::id()));
    fs::create_dir_all(&staging)?;
    let built = build(host, sources, tree, &pairs, &names, (&loaded, &id), (&staging, &image));
    let _ = fs::remove_dir_all(&staging);
    Ok(match built? {
        Some(reason) => Outcome::Refused(reason),
        None => Outcome::Ready { image, warning },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;

    struct FakeHost {
        loaded: Option<String>,
        installed: Option<String>,
        files: Vec<String>,
    }

    impl DriverHost for FakeHost {
        fn loaded_version(&self) -> Option<String> {
            self.loaded.clone()
        }

        fn library_version(&self, _library: &str) -> Option<String> {
            self.installed.clone()
        }

        fn driver_files(&self) -> Vec<String> {
            self.files.clone()
        }
    }

    fn host(version: &str) -> FakeHost {
        FakeHost { loaded: Some(version.to_owned()), installed: Some(version.to_owned()), files: Vec::new() }
    }

    fn tree(scratch: &Scratch, version: &str) -> PathBuf {
        scratch.write("tree/version.mk", format!("NVIDIA_VERSION = {version}\n"));
        scratch.path("tree")
    }

    fn sources(scratch: &Scratch) -> PathBuf {
        scratch.write("sources/guest/nvidia.c", "module");
        scratch.path("sources")
    }

    fn image_name(sources: &Path, version: &str) -> String {
        let id = sources_id(sources, &our_files(sources)).unwrap();
        format!("driver-{version}-{id}-{}.iso", sources_id(sources, &setup_files(sources)).unwrap())
    }

    fn reason(outcome: Outcome) -> String {
        match outcome {
            Outcome::Refused(off) => off.detail,
            Outcome::Ready { .. } => panic!("it should have been refused"),
        }
    }

    #[test]
    fn an_unknown_branch_turns_the_gpu_off_even_with_an_image() {
        let scratch = Scratch::new();
        let tree = tree(&scratch, "620.11");
        let said =
            reason(prepare(&host("620.11"), &sources(&scratch), &tree, &scratch.path("images")).unwrap());
        assert!(said.contains("branch 620"));
    }

    #[test]
    fn no_module_is_a_refusal() {
        let scratch = Scratch::new();
        let tree = tree(&scratch, "615.78.08");
        let none = FakeHost { loaded: None, installed: None, files: Vec::new() };
        let said = reason(prepare(&none, &sources(&scratch), &tree, &scratch.path("images")).unwrap());
        assert!(said.contains("no NVIDIA kernel module"));
    }

    #[test]
    fn the_wrong_source_tree_says_how_to_get_the_right_one() {
        let scratch = Scratch::new();
        let tree = tree(&scratch, "600.1");
        let said =
            reason(prepare(&host("615.78.08"), &sources(&scratch), &tree, &scratch.path("images")).unwrap());
        assert!(said.contains("git clone") && said.contains("615.78.08"));
    }

    #[test]
    fn a_pending_reboot_is_a_refusal_when_there_is_no_image_yet() {
        let scratch = Scratch::new();
        let tree = tree(&scratch, "615.78.08");
        let waiting = FakeHost { installed: Some("615.90.01".into()), ..host("615.78.08") };
        let said = reason(prepare(&waiting, &sources(&scratch), &tree, &scratch.path("images")).unwrap());
        assert!(said.contains("Reboot"));
    }

    #[test]
    fn an_image_for_the_loaded_driver_is_used_as_it_is() {
        let scratch = Scratch::new();
        let tree = tree(&scratch, "615.78.08");
        let sources = sources(&scratch);
        let image = scratch.write(&format!("images/{}", image_name(&sources, "615.78.08")), "made earlier");
        let moved_on = FakeHost { installed: Some("615.90.01".into()), ..host("615.78.08") };
        let found = prepare(&moved_on, &sources, &tree, &scratch.path("images")).unwrap();
        assert_eq!(found, Outcome::Ready { image, warning: None });
    }

    #[test]
    fn another_point_release_warns_and_goes_on() {
        let scratch = Scratch::new();
        let tree = tree(&scratch, "615.90.01");
        let sources = sources(&scratch);
        scratch.write(&format!("images/{}", image_name(&sources, "615.90.01")), "made earlier");
        let found = prepare(&host("615.90.01"), &sources, &tree, &scratch.path("images")).unwrap();
        assert!(matches!(found, Outcome::Ready { warning: Some(_), .. }));
    }

    #[test]
    fn a_change_to_the_sources_changes_the_images_name() {
        let scratch = Scratch::new();
        let sources = sources(&scratch);
        let before = sources_id(&sources, &our_files(&sources)).unwrap();
        assert_eq!(before.len(), SOURCE_ID_LENGTH);
        scratch.write("sources/guest/nvidia.c", "another module");
        assert_ne!(sources_id(&sources, &our_files(&sources)).unwrap(), before);
    }

    #[test]
    fn a_new_script_names_a_new_image_and_leaves_the_built_id_alone() {
        let scratch = Scratch::new();
        let sources = sources(&scratch);
        scratch.write("sources/setup/guest/gpu-up.sh", "one");
        let (built, name) =
            (sources_id(&sources, &our_files(&sources)).unwrap(), image_name(&sources, "615.78.08"));
        scratch.write("sources/setup/guest/gpu-up.sh", "two");
        assert_eq!(sources_id(&sources, &our_files(&sources)).unwrap(), built);
        assert_ne!(image_name(&sources, "615.78.08"), name);
        assert_eq!(setup_files(&sources), ["setup/guest/gpu-up.sh"]);
    }

    #[test]
    fn the_scripts_go_on_the_image_as_they_are() {
        let scratch = Scratch::new();
        let sources = sources(&scratch);
        let script = scratch.write("sources/setup/guest/setup-own.sh", "#!/bin/bash\n");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        scratch.write("sources/setup/guest/files/hooks/90-nvfwd.hook", "[Trigger]\n");
        copy_setup(&sources, &scratch.path("image/setup")).unwrap();
        let copied = scratch.path("image/setup/setup-own.sh");
        assert_eq!(fs::metadata(copied).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(scratch.path("image/setup/files/hooks/90-nvfwd.hook").is_file());
    }

    #[test]
    fn a_link_beside_its_file_stays_a_link_and_one_that_leads_away_is_copied() {
        let scratch = Scratch::new();
        scratch.write("real/libfoo.so.1.2", "the library");
        symlink("libfoo.so.1.2", scratch.path("real/libfoo.so.1")).unwrap();
        symlink(scratch.path("real/libfoo.so.1.2"), scratch.path("real/libbar.so.1")).unwrap();
        assert!(sibling_link(&scratch.path("real/libfoo.so.1")).is_some());
        let target = scratch.path("image/lib/libbar.so.1");
        copy_file(&scratch.path("real/libbar.so.1"), &target, "lib/libbar.so.1").unwrap();
        assert!(!target.is_symlink());
        assert_eq!(fs::read_to_string(target).unwrap(), "the library");
    }

    #[test]
    fn missing_files_are_named() {
        let scratch = Scratch::new();
        let tree = tree(&scratch, "615.78.08");
        let said =
            reason(prepare(&host("615.78.08"), &sources(&scratch), &tree, &scratch.path("images")).unwrap());
        assert!(said.contains("libcuda.so.615.78.08, from NVIDIA's driver"));
    }
}
