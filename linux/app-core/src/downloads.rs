//! Files the app fetches itself, so a machine needs nothing found by hand: Ubuntu's installer disc, and
//! an Android machine's image. Each is downloaded once into the fetched parts' folder, going on from where
//! a stopped download left off, and is only kept if its SHA-256 is the one its publisher gives.

use crate::failure::{Failure, Result};
use crate::paths::Paths;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(500);
#[cfg(test)]
use guesthouse_download_core::linux_host::PARTIAL;
/// A download's file names, the words curl is given, the reading of what it says, how far a download
/// is, the check of its SHA-256 and the sentences, as the Linux host has them in every host's
/// `guesthouse_download_core`.
use guesthouse_download_core::linux_host::{
    self as rules, CHECKING, CURL_WORDS, GO_ON_WORDS, HEAD_WORD, checked_marker, partial_file,
};

pub use guesthouse_catalog_core::android::ANDROID_IMAGE_NAME;
/// Ubuntu's release folder, its checksum list and the disc's name, and the Android image, are in
/// every host's catalog, `guesthouse_catalog_core`.
use guesthouse_catalog_core::android::{ANDROID_IMAGE_HOST, ANDROID_IMAGE_SHA256};
#[cfg(test)]
use guesthouse_catalog_core::ubuntu_amd64::{UBUNTU_PREFIX, UBUNTU_SUFFIX};
use guesthouse_catalog_core::ubuntu_amd64::{UBUNTU_RELEASE, UBUNTU_SUMS, newest_server_disc};
const UBUNTU_FOLDER: &str = "ubuntu";

const ANDROID_FOLDER: &str = "android";

fn said(output: &std::process::Output) -> String {
    rules::said(&output.stderr)
}

fn curl() -> Command {
    let mut command = Command::new("curl");
    command.args(CURL_WORDS);
    command.stdin(Stdio::null());
    command
}

/// A small text file's contents, such as a checksum list.
pub fn text_at(url: &str) -> Result<String> {
    let output = curl().arg(url).output().map_err(|error| Failure(rules::not_started(error)))?;
    if !output.status.success() {
        return Err(Failure(rules::unreadable(url, &said(&output))));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A file's size as its server says it, if it does.
pub fn size_at(url: &str) -> Option<u64> {
    let output = curl().args([HEAD_WORD, url]).output().ok()?;
    rules::content_length(&output.stdout)
}

pub fn sha256(file: &Path) -> Result<String> {
    let output = Command::new("sha256sum").arg(file).stdin(Stdio::null()).output()?;
    rules::sha256sum_sum(&output.stdout).ok_or_else(|| Failure(said(&output)))
}

/// A size in GiB or MiB, as said before a download: every host's formatters are in
/// `guesthouse_basics::words`.
pub use guesthouse_basics::words::size_in_gb_or_mb_up as size_text;

/// Downloads `url` to `to`, going on from what an earlier try left, and tells `step` how far it is as
/// "`what`: N%".
fn download(url: &str, to: &Path, bytes: Option<u64>, what: &str, step: &dyn Fn(&str)) -> Result<()> {
    let mut child = curl()
        .args(GO_ON_WORDS)
        .arg(to)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Failure(rules::not_started(error)))?;
    let mut shown = None;
    while child.try_wait()?.is_none() {
        let have = fs::metadata(to).map_or(0, |found| found.len());
        let text = rules::progress(what, have, bytes);
        if shown.as_ref() != Some(&text) {
            step(&text);
            shown = Some(text);
        }
        sleep(POLL);
    }
    let ended = child.wait_with_output()?;
    if ended.status.success() {
        return Ok(());
    }
    Err(Failure(rules::stopped(what, &said(&ended))))
}

/// Whether `file` is there and was already checked against `wanted`.
fn already_checked(file: &Path, wanted: &str) -> bool {
    file.is_file()
        && fs::read_to_string(checked_marker(file)).is_ok_and(|sum| rules::marker_says(&sum, wanted))
}

/// Fetches `url` to `file` unless it's already there and checked, and keeps it only if its SHA-256 is
/// `wanted`.
pub fn fetch_checked(url: &str, wanted: &str, file: &Path, what: &str, step: &dyn Fn(&str)) -> Result<()> {
    if already_checked(file, wanted) {
        return Ok(());
    }
    fs::create_dir_all(file.parent().unwrap_or(Path::new("")))?;
    let partial = partial_file(file);
    step(what);
    download(url, &partial, size_at(url), what, step)?;
    step(CHECKING);
    if !rules::sum_is_wanted(&sha256(&partial)?, wanted) {
        let _ = fs::remove_file(&partial);
        return Err(Failure(rules::wrong_file(what)));
    }
    fs::rename(&partial, file)?;
    fs::write(checked_marker(file), wanted)?;
    Ok(())
}

/// Ubuntu's server disc that an Ubuntu machine installs from, fetched if it isn't here.
pub struct UbuntuDisc {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub file: PathBuf,
}

/// The disc an Ubuntu machine would download now: what Ubuntu publishes, and where it's kept here.
pub fn ubuntu_disc(paths: &Paths) -> Result<UbuntuDisc> {
    let sums = text_at(&format!("{UBUNTU_RELEASE}{UBUNTU_SUMS}"))?;
    let (name, sha256) = newest_server_disc(&sums)
        .ok_or_else(|| Failure::new("Ubuntu's release page lists no server disc for 26.04."))?;
    Ok(UbuntuDisc {
        url: format!("{UBUNTU_RELEASE}{name}"),
        file: paths.parts.join(UBUNTU_FOLDER).join(&name),
        name,
        sha256,
    })
}

/// An Ubuntu disc already downloaded and checked, without asking the network.
pub fn ubuntu_disc_here(paths: &Paths) -> Option<PathBuf> {
    let folder = paths.parts.join(UBUNTU_FOLDER);
    let mut found: Vec<PathBuf> = fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "iso") && checked_marker(path).is_file())
        .collect();
    found.sort();
    found.pop()
}

/// Where Guesthouse's Android image is kept here.
pub fn android_image_file(paths: &Paths) -> PathBuf {
    paths.parts.join(ANDROID_FOLDER).join(ANDROID_IMAGE_NAME)
}

/// Guesthouse's Android image, if it was downloaded and checked already.
pub fn android_image_here(paths: &Paths) -> Option<PathBuf> {
    let file = android_image_file(paths);
    already_checked(&file, ANDROID_IMAGE_SHA256).then_some(file)
}

/// Fetches Guesthouse's Android image unless it's here, and says where it is.
pub fn fetch_android_image(paths: &Paths, step: &dyn Fn(&str)) -> Result<PathBuf> {
    let file = android_image_file(paths);
    let url = format!("{ANDROID_IMAGE_HOST}{ANDROID_IMAGE_NAME}");
    fetch_checked(&url, ANDROID_IMAGE_SHA256, &file, "Downloading Android", step)?;
    Ok(file)
}

#[cfg(test)]
#[path = "downloads_pins.rs"]
pub(crate) mod pins;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;

    #[test]
    fn the_newest_server_disc_is_taken_from_ubuntu_s_list() {
        let sums = "aa *ubuntu-26.04-desktop-amd64.iso\nbb *ubuntu-26.04-live-server-amd64.iso\n\
                    cc *ubuntu-26.04.1-live-server-amd64.iso\ndd *ubuntu-26.04.1-wsl-amd64.wsl\n";
        assert_eq!(
            newest_server_disc(sums),
            Some(("ubuntu-26.04.1-live-server-amd64.iso".to_owned(), "cc".to_owned()))
        );
        assert_eq!(newest_server_disc("aa *ubuntu-26.04-desktop-amd64.iso\n"), None);
    }

    /// The picker on Ubuntu's own list as it was saved, and on the lines it has to tell apart.
    #[test]
    fn the_picker_on_a_saved_list_and_at_its_edges() {
        let saved = include_str!("../tests/catalog/releases-ubuntu-26.04-SHA256SUMS");
        assert_eq!(
            newest_server_disc(saved),
            Some((
                "ubuntu-26.04.1-live-server-amd64.iso".to_owned(),
                "cc8a95cde20f6ced61a322420de00f10cc3c90ced545daa46cb9c1a117f1d927".to_owned()
            ))
        );
        let pick = |sums: &str| newest_server_disc(sums).map(|(name, sum)| format!("{sum} {name}"));
        let ten = "b *ubuntu-26.04.10-live-server-amd64.iso\na *ubuntu-26.04.9-live-server-amd64.iso\n";
        assert_eq!(pick(ten).unwrap(), "b ubuntu-26.04.10-live-server-amd64.iso", "points sort as numbers");
        let plain = "a  ubuntu-26.04-live-server-amd64.iso  \n";
        assert_eq!(pick(plain).unwrap(), "a ubuntu-26.04-live-server-amd64.iso", "no star, spaces trimmed");
        let odd = "a *ubuntu-26.04-live-server-amd64.iso\nb *ubuntu-26.04x-live-server-amd64.iso\n";
        assert_eq!(pick(odd).unwrap(), "a ubuntu-26.04-live-server-amd64.iso", "an odd middle is no disc");
        let same = "a *ubuntu-26.04.1-live-server-amd64.iso\nb *ubuntu-26.04.1-live-server-amd64.iso\n";
        assert_eq!(pick(same).unwrap(), "b ubuntu-26.04.1-live-server-amd64.iso", "the last of a tie");
        let short = "NOTAHASH *ubuntu-26.04-live-server-amd64.iso\n";
        assert_eq!(
            pick(short).unwrap(),
            "NOTAHASH ubuntu-26.04-live-server-amd64.iso",
            "the sum isn't checked"
        );
        assert_eq!(pick("x *ubuntu-26.04-live-server-arm64.iso\nno-space-line\n\n"), None);
        assert_eq!(pick(""), None);
    }

    /// Where each image comes from and what it must hash to, as they were pinned.
    #[test]
    fn the_catalog_is_what_it_was() {
        assert_eq!(UBUNTU_RELEASE, "https://releases.ubuntu.com/26.04/");
        assert_eq!(UBUNTU_SUMS, "SHA256SUMS");
        assert_eq!((UBUNTU_PREFIX, UBUNTU_SUFFIX), ("ubuntu-26.04", "-live-server-amd64.iso"));
        assert_eq!(ANDROID_IMAGE_NAME, "guesthouse-android-17-2026-11-23.qcow2");
        assert_eq!(ANDROID_IMAGE_SHA256, "d547bda57a11624e201bd6c4ea50f3e8da3f63997d5a2ffb575220050ec10917");
        assert_eq!(ANDROID_IMAGE_HOST, "https://files.ruxn.com/p/guesthouse/");
        assert_eq!((UBUNTU_FOLDER, ANDROID_FOLDER), ("ubuntu", "android"));
    }

    #[test]
    fn a_file_is_kept_only_if_it_s_the_one_it_should_be() {
        let scratch = Scratch::new();
        let source = scratch.path("source.iso");
        fs::write(&source, "disc").unwrap();
        let url = format!("file://{}", source.display());
        let wanted = sha256(&source).unwrap();
        let file = scratch.path("parts/ubuntu/disc.iso");
        assert!(fetch_checked(&url, "not-it", &file, "Downloading", &|_| {}).is_err());
        assert!(!file.exists() && !file.with_extension(PARTIAL).exists());
        fetch_checked(&url, &wanted, &file, "Downloading", &|_| {}).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "disc");
        fs::remove_file(&source).unwrap();
        fetch_checked(&url, &wanted, &file, "Downloading", &|_| {}).unwrap();
    }
}
