//! The Linux host's downloads, as `app-core` fetches with curl: Ubuntu's disc, the Android image and
//! remoteman's installer through one way ([`partial_file`], [`checked_marker`] and the rest), and the
//! macOS part through a way of its own ([`macos_part`]).
//!
//! A file is downloaded to a partial file beside it, whose name replaces the file's last extension
//! with `part`, going on from what an earlier try left there (curl's `--continue-at -`). Once curl has
//! ended, the partial file's SHA-256 is compared with the one wanted, as text and exactly. A file that
//! isn't the one wanted is deleted; one that is takes the file's name, and a mark beside it, whose
//! name replaces the last extension with `sha256`, holds the sum it was checked against. A later use
//! trusts a file with its mark when the mark's text, trimmed, is the wanted sum, without reading the
//! file again. A download that stops keeps its partial file as it was, for the next try.
//!
//! curl follows redirects (`--location`), and only to https. What keeps a wrong file out is the
//! SHA-256.

use std::fmt::Display;
use std::path::{Path, PathBuf};

pub mod macos_part;

/// The partial download beside where the file goes.
pub const PARTIAL: &str = "part";
/// The SHA-256 a finished file was checked against, beside it, so a later use needn't read it again.
pub const CHECKED: &str = "sha256";

/// The words curl is given before anything else, for a download, a file's size or a small text.
pub const CURL_WORDS: [&str; 8] =
    ["--fail", "--location", "--silent", "--show-error", "--proto-redir", "=https", "--retry", "3"];
/// The words that ask curl for the answer's headers only, before the address, for a file's size.
pub const HEAD_WORD: &str = "--head";
/// The words a download gives curl after [`CURL_WORDS`]: go on from what an earlier try left in the
/// file, which follows.
pub const GO_ON_WORDS: [&str; 3] = ["--continue-at", "-", "--output"];

/// What is said while a download is checked.
pub const CHECKING: &str = "Checking the download";

/// The partial file `file` is downloaded to: its last extension replaced, so
/// `ubuntu-26.04.1-live-server-amd64.iso` comes as `ubuntu-26.04.1-live-server-amd64.part`.
pub fn partial_file(file: &Path) -> PathBuf {
    file.with_extension(PARTIAL)
}

/// The mark beside `file` that says what it was checked against: its last extension replaced, so
/// `disc.iso` is marked by `disc.sha256`.
pub fn checked_marker(file: &Path) -> PathBuf {
    file.with_extension(CHECKED)
}

/// Whether a mark's text says its file was checked against `wanted`: the text trimmed, and the same
/// exactly, case and all.
pub fn marker_says(marker: &str, wanted: &str) -> bool {
    marker.trim() == wanted
}

/// Whether the SHA-256 `found` is the one `wanted`: the same text exactly, case and all. sha256sum
/// says it in lower case.
pub fn sum_is_wanted(found: &str, wanted: &str) -> bool {
    found == wanted
}

/// The SHA-256 in what sha256sum printed: its first word.
pub fn sha256sum_sum(printed: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(printed);
    text.split_whitespace().next().map(str::to_owned)
}

/// What a program said on its standard error, trimmed.
pub fn said(errors: &[u8]) -> String {
    String::from_utf8_lossy(errors).trim().to_owned()
}

/// A file's size from the headers `curl --head` printed. After redirects, the last length given is
/// the file's. Only a line that starts with `content-length:`, in any case, gives one.
pub fn content_length(headers: &[u8]) -> Option<u64> {
    let headers = String::from_utf8_lossy(headers).to_ascii_lowercase();
    headers
        .lines()
        .filter_map(|line| line.strip_prefix("content-length:"))
        .filter_map(|value| value.trim().parse().ok())
        .next_back()
}

/// How far a download of `total` bytes is with `have` of them here, in whole percent, never more
/// than 100.
pub fn percent(have: u64, total: u64) -> u64 {
    (have * 100 / total.max(1)).min(100)
}

/// A size in GiB or MiB, as said before and while a download comes: every host's formatters are in
/// `guesthouse_basics::words`.
pub use guesthouse_basics::words::size_in_gb_or_mb_up as size_text;

/// How far a download is, as "`what`: N%" when its size is known, or with how much has come when it
/// isn't.
pub fn progress(what: &str, have: u64, total: Option<u64>) -> String {
    match total {
        Some(total) => format!("{what}: {}%", percent(have, total)),
        None => format!("{what}: {}", size_text(have)),
    }
}

/// What is said when curl couldn't be started.
pub fn not_started(error: impl Display) -> String {
    format!("curl could not be started: {error}")
}

/// What is said when a small text at `url` couldn't be read, with curl's words.
pub fn unreadable(url: &str, said: &str) -> String {
    format!("{url} could not be read: {said}")
}

/// What is said when the download `what` stopped, with curl's words.
pub fn stopped(what: &str, said: &str) -> String {
    format!("{what} stopped: {said}")
}

/// What is said when the download `what` gave a file whose SHA-256 isn't the one wanted.
pub fn wrong_file(what: &str) -> String {
    format!("{what} gave a file that isn't the one it should be. Try again.")
}

#[cfg(test)]
mod tests;
