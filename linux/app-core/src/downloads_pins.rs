//! What the Linux host's downloads do today, held before their plain half moves: the names a
//! download's files have at every stage, what is done with a half download left by an earlier try or
//! an earlier version, when a file is trusted without being read again, the words curl is given and
//! every sentence a download says. Nothing here reaches a network: a file is fetched from a
//! `file://` address, or from a stand-in curl that a copy of this test program finds first on its
//! `PATH`, which writes the file in two halves and waits between them until the progress has been
//! seen.

use super::*;
use crate::testing::{Scratch, scratch_paths};
use std::cell::RefCell;
use std::process::Command;
use std::time::{Duration, Instant};

/// Set, to the stand-in curl's folder, in a copy of this test program that runs one test of its own.
const STAND_IN: &str = "GUESTHOUSE_STAND_IN_CURL";

/// A curl that says what it was asked in `asked`, prints `headers` for `--head`, and otherwise adds
/// `first` to its `--output` once `go-first` exists, then `second` once `go` exists, each at once by a
/// rename, so a test sees nothing, then the half, then the whole, and never a part of a half.
const STAND_IN_CURL: &str = r#"#!/bin/sh
here=$(dirname "$0")
printf '%s\n' "$*" >> "$here/asked"
out=""
head=no
prev=""
for word in "$@"; do
    [ "$prev" = "--output" ] && out=$word
    [ "$word" = "--head" ] && head=yes
    prev=$word
done
if [ $head = yes ]; then
    cat "$here/headers" 2>/dev/null
    exit 0
fi
if [ -z "$out" ]; then
    cat "$here/text"
    exit 0
fi
wait_for() {
    n=0
    while [ ! -e "$here/$1" ] && [ $n -lt 600 ]; do sleep 0.1; n=$((n+1)); done
}
put() {
    { if [ -e "$out" ]; then cat "$out"; fi; cat "$here/$1"; } > "$out.next"
    mv "$out.next" "$out"
}
wait_for go-first
put first
wait_for go
put second
exit 0
"#;

/// The stand-in curl's folder in `scratch`, made with nothing asked yet.
pub(crate) fn stand_in_curl(scratch: &Scratch) -> PathBuf {
    scratch.program("curl-bin/curl", STAND_IN_CURL);
    scratch.path("curl-bin")
}

/// Runs the test `name` in a copy of this test program whose `PATH` starts with `bin`, and fails if
/// it fails.
pub(crate) fn run_with_path(name: &str, bin: &Path, rest_of_path: bool) {
    let path = if rest_of_path {
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
    } else {
        bin.display().to_string()
    };
    let output = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PATH", path)
        .env(STAND_IN, bin)
        .output()
        .unwrap();
    let said =
        format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{said}");
    assert!(said.contains("1 passed"), "the copy ran no test: {said}");
}

/// The stand-in's folder, in the copy run by [`run_with_path`]; none in an ordinary run.
pub(crate) fn stand_in_here() -> Option<PathBuf> {
    std::env::var_os(STAND_IN).map(PathBuf::from)
}

/// Readies the stand-in for one download: what `--head` prints and the file's two halves.
pub(crate) fn ready(bin: &Path, headers: &str, first: &[u8], second: &[u8]) {
    for name in ["asked", "go-first", "go"] {
        let _ = fs::remove_file(bin.join(name));
    }
    fs::write(bin.join("headers"), headers).unwrap();
    fs::write(bin.join("first"), first).unwrap();
    fs::write(bin.join("second"), second).unwrap();
}

/// Lets the stand-in write the first half.
pub(crate) fn go_first(bin: &Path) {
    fs::write(bin.join("go-first"), "").unwrap();
}

/// Lets the stand-in write the second half.
pub(crate) fn go(bin: &Path) {
    fs::write(bin.join("go"), "").unwrap();
}

/// Each line the stand-in was asked, its words joined by spaces.
pub(crate) fn asked(bin: &Path) -> Vec<String> {
    fs::read_to_string(bin.join("asked")).unwrap_or_default().lines().map(str::to_owned).collect()
}

/// The names in `folder`, sorted.
pub(crate) fn names_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .map(|entries| {
            entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn sum_of(data: &[u8], scratch: &Scratch) -> String {
    let path = scratch.write("sum-of", data);
    let sum = sha256(&path).unwrap();
    fs::remove_file(path).unwrap();
    sum
}

/// The disc's, the Android image's and remoteman's installer's partial file and checked mark, as the
/// catalog and the remoteman pin name them today: the last extension is replaced, so a name with a
/// version in it keeps all but its last part.
#[test]
fn each_file_s_partial_and_mark_replace_its_last_extension() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let source = scratch.write("source", "the file");
    let wanted = sha256(&source).unwrap();
    let parts = &paths.parts;
    for (folder, name, partial, mark) in [
        (
            "ubuntu",
            "ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-amd64.part",
            "ubuntu-26.04.1-live-server-amd64.sha256",
        ),
        (
            "android",
            "guesthouse-android-17-2026-11-23.qcow2",
            "guesthouse-android-17-2026-11-23.part",
            "guesthouse-android-17-2026-11-23.sha256",
        ),
        (
            "remoteman",
            "remoteman-host-setup-0.1.10.exe",
            "remoteman-host-setup-0.1.10.part",
            "remoteman-host-setup-0.1.10.sha256",
        ),
        ("other", "no-extension", "no-extension.part", "no-extension.sha256"),
        ("other", "archive.tar.xz", "archive.tar.part", "archive.tar.sha256"),
    ] {
        let file = parts.join(folder).join(name);
        // A half download stops with the partial file left, under its name.
        fs::create_dir_all(parts.join(folder)).unwrap();
        fs::write(parts.join(folder).join(partial), "the f").unwrap();
        fetch_checked(&file_url(&source), &wanted, &file, "Downloading", &|_| {}).unwrap();
        let mut whole = names_in(&parts.join(folder));
        whole.retain(|found| found == name || found == mark || found == partial);
        let mut expected = vec![name.to_owned(), mark.to_owned()];
        expected.sort();
        assert_eq!(whole, expected, "the partial went on and became {name}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "the file", "{name} went on from its half");
        assert_eq!(fs::read_to_string(parts.join(folder).join(mark)).unwrap(), wanted, "no line end");
    }
}

/// A partial file with the wrong bytes in it, as a try at another file under the same name leaves,
/// is gone on from, fails the check, is deleted, and the next try starts again and keeps the file.
#[test]
fn a_wrong_half_is_gone_on_from_then_thrown_away_and_the_next_try_works() {
    let scratch = Scratch::new();
    let source = scratch.write("source.iso", "disc");
    let wanted = sha256(&source).unwrap();
    let file = scratch.path("parts/ubuntu/disc.iso");
    scratch.write("parts/ubuntu/disc.part", "XY");
    let error = fetch_checked(&file_url(&source), &wanted, &file, "Downloading Ubuntu", &|_| {}).unwrap_err();
    assert_eq!(error.0, "Downloading Ubuntu gave a file that isn't the one it should be. Try again.");
    assert_eq!(names_in(&scratch.path("parts/ubuntu")), Vec::<String>::new());
    fetch_checked(&file_url(&source), &wanted, &file, "Downloading Ubuntu", &|_| {}).unwrap();
    assert_eq!(names_in(&scratch.path("parts/ubuntu")), ["disc.iso", "disc.sha256"]);
}

/// A download that stops keeps its partial file as it was, to go on from next time.
#[test]
fn a_stopped_download_keeps_its_half_and_says_curls_words() {
    let scratch = Scratch::new();
    let file = scratch.path("parts/ubuntu/disc.iso");
    scratch.write("parts/ubuntu/disc.part", "di");
    let missing = scratch.path("not-there.iso");
    let error = fetch_checked(&file_url(&missing), "aa", &file, "Downloading Ubuntu", &|_| {}).unwrap_err();
    assert!(error.0.starts_with("Downloading Ubuntu stopped: curl: (37) "), "{error}");
    assert_eq!(fs::read_to_string(scratch.path("parts/ubuntu/disc.part")).unwrap(), "di");
    assert_eq!(names_in(&scratch.path("parts/ubuntu")), ["disc.part"]);
}

/// A partial file longer than the file now is can't be gone on from by a `file://` address, and is
/// kept, so each later try stops the same way.
#[test]
fn a_half_longer_than_the_file_stops_each_try_and_is_kept() {
    let scratch = Scratch::new();
    let source = scratch.write("source.iso", "disc");
    let wanted = sha256(&source).unwrap();
    let file = scratch.path("parts/ubuntu/disc.iso");
    scratch.write("parts/ubuntu/disc.part", "an older, longer file");
    for _ in 0..2 {
        let error =
            fetch_checked(&file_url(&source), &wanted, &file, "Downloading Ubuntu", &|_| {}).unwrap_err();
        assert!(error.0.starts_with("Downloading Ubuntu stopped: curl: (36) "), "{error}");
        assert_eq!(
            fs::read_to_string(scratch.path("parts/ubuntu/disc.part")).unwrap(),
            "an older, longer file"
        );
    }
}

/// A file with its mark beside it is trusted without being read or fetched again, when the mark's
/// text, trimmed, is the wanted sum exactly; otherwise it is fetched again over it.
#[test]
fn a_marked_file_is_trusted_by_its_mark_alone() {
    let scratch = Scratch::new();
    let file = scratch.write("parts/x/disc.iso", "not even the file");
    let nowhere = file_url(&scratch.path("nowhere.iso"));
    scratch.write("parts/x/disc.sha256", "abc\n");
    fetch_checked(&nowhere, "abc", &file, "Downloading", &|_| panic!("nothing is fetched")).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "not even the file");
    scratch.write("parts/x/disc.sha256", "  abc  ");
    fetch_checked(&nowhere, "abc", &file, "Downloading", &|_| panic!("nothing is fetched")).unwrap();
    // Another case is another sum.
    scratch.write("parts/x/disc.sha256", "ABC");
    let error = fetch_checked(&nowhere, "abc", &file, "Downloading", &|_| {}).unwrap_err();
    assert!(error.0.starts_with("Downloading stopped: curl: (37) "), "{error}");
    // A mark with no file is no file.
    let gone = scratch.path("parts/y/disc.iso");
    scratch.write("parts/y/disc.sha256", "abc");
    assert!(fetch_checked(&nowhere, "abc", &gone, "Downloading", &|_| {}).is_err());
}

/// The sum is compared as text, exactly: a wanted sum in capitals is never the one sha256sum gives.
#[test]
fn the_sum_is_compared_exactly() {
    let scratch = Scratch::new();
    let source = scratch.write("source.iso", "disc");
    let wanted = sha256(&source).unwrap();
    assert_eq!(wanted.len(), 64);
    assert!(wanted.bytes().all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase()));
    let file = scratch.path("parts/u/disc.iso");
    let error =
        fetch_checked(&file_url(&source), &wanted.to_uppercase(), &file, "Downloading", &|_| {}).unwrap_err();
    assert_eq!(error.0, "Downloading gave a file that isn't the one it should be. Try again.");
    assert_eq!(
        sha256(&scratch.write("empty", "")).unwrap(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// An Ubuntu disc is here when an `.iso` has any mark beside it, whatever the mark says, and the last
/// such name sorted is the one; a partial file is never one.
#[test]
fn an_ubuntu_disc_here_is_any_marked_iso_the_last_by_name() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    assert_eq!(ubuntu_disc_here(&paths), None);
    let folder = paths.parts.join("ubuntu");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("ubuntu-26.04.1-live-server-amd64.part"), "half").unwrap();
    fs::write(folder.join("ubuntu-26.04.1-live-server-amd64.sha256"), "anything").unwrap();
    assert_eq!(ubuntu_disc_here(&paths), None);
    fs::write(folder.join("ubuntu-26.04-live-server-amd64.iso"), "disc").unwrap();
    fs::write(folder.join("ubuntu-26.04-live-server-amd64.sha256"), "").unwrap();
    assert_eq!(ubuntu_disc_here(&paths), Some(folder.join("ubuntu-26.04-live-server-amd64.iso")));
    fs::write(folder.join("ubuntu-26.04.1-live-server-amd64.iso"), "disc").unwrap();
    assert_eq!(ubuntu_disc_here(&paths), Some(folder.join("ubuntu-26.04.1-live-server-amd64.iso")));
    fs::write(folder.join("an-iso-of-my-own.iso"), "disc").unwrap();
    fs::write(folder.join("zz-my-own.iso"), "unmarked").unwrap();
    assert_eq!(ubuntu_disc_here(&paths), Some(folder.join("ubuntu-26.04.1-live-server-amd64.iso")));
    fs::write(folder.join("zz-my-own.sha256"), "").unwrap();
    assert_eq!(ubuntu_disc_here(&paths), Some(folder.join("zz-my-own.iso")));
}

/// The Android image is here only with its mark saying the catalog's sum, and its half under an
/// older image's name is left where it is, never gone on from.
#[test]
fn the_android_image_is_here_by_its_mark_and_an_older_half_is_left() {
    let scratch = Scratch::new();
    let paths = scratch_paths(&scratch);
    let file = android_image_file(&paths);
    assert_eq!(file, paths.parts.join("android/guesthouse-android-17-2026-11-23.qcow2"));
    assert_eq!(android_image_here(&paths), None);
    let folder = file.parent().unwrap().to_owned();
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("guesthouse-android-17-2026-01-01.part"), "an older image's half").unwrap();
    fs::write(&file, "image").unwrap();
    fs::write(folder.join("guesthouse-android-17-2026-11-23.sha256"), "not the sum").unwrap();
    assert_eq!(android_image_here(&paths), None);
    fs::write(
        folder.join("guesthouse-android-17-2026-11-23.sha256"),
        "d547bda57a11624e201bd6c4ea50f3e8da3f63997d5a2ffb575220050ec10917\n",
    )
    .unwrap();
    assert_eq!(android_image_here(&paths), Some(file));
    assert!(folder.join("guesthouse-android-17-2026-01-01.part").exists());
}

/// remoteman's installer: the copy a build shipped when its sum is the pin's, read each time; or one
/// fetched into the parts' `remoteman` folder under the pin's file name, with its mark beside it.
#[test]
fn remoteman_s_installer_is_the_shipped_copy_or_one_fetched_under_the_pin_s_name() {
    use crate::remote_view::installer;
    use guesthouse_remote_view_core::install::Pin;
    let scratch = Scratch::new();
    let context = crate::testing::scratch_context(&scratch);
    let source = scratch.write("source.exe", "installer");
    let pin = Pin {
        version: "0.1.10".to_owned(),
        url: file_url(&source),
        file: "remoteman-host-setup-0.1.10.exe".to_owned(),
        sha256: sha256(&source).unwrap(),
    };
    let shipped = scratch.write("program/remoteman/remoteman-host-setup.exe", "installer");
    assert_eq!(installer(&context, &pin).unwrap(), shipped);
    fs::write(&shipped, "another").unwrap();
    let kept = installer(&context, &pin).unwrap();
    assert_eq!(kept, context.paths.parts.join("remoteman/remoteman-host-setup-0.1.10.exe"));
    assert_eq!(
        names_in(&context.paths.parts.join("remoteman")),
        ["remoteman-host-setup-0.1.10.exe", "remoteman-host-setup-0.1.10.sha256"]
    );
}

/// A file's size from its address, by curl's `--head` on a `file://` address.
#[test]
fn a_size_is_read_from_curls_head() {
    let scratch = Scratch::new();
    let source = scratch.write("source.iso", "four");
    assert_eq!(size_at(&file_url(&source)), Some(4));
    assert_eq!(size_at(&file_url(&scratch.path("nowhere"))), None);
    let error = text_at(&file_url(&scratch.path("nowhere"))).unwrap_err();
    let start = format!("{} could not be read: curl: (37) ", file_url(&scratch.path("nowhere")));
    assert!(error.0.starts_with(&start), "{error}");
    assert_eq!(text_at(&file_url(&source)).unwrap(), "four");
}

/// The size said before a download.
#[test]
fn the_size_before_a_download_is_in_gib_or_mib_rounded_up() {
    assert_eq!(size_text(0), "0 MB");
    assert_eq!(size_text(1), "1 MB");
    assert_eq!(size_text(1 << 20), "1 MB");
    assert_eq!(size_text((1 << 20) + 1), "2 MB");
    assert_eq!(size_text(1023 << 20), "1023 MB");
    assert_eq!(size_text(1 << 30), "1.0 GB");
    assert_eq!(size_text(3 * (1 << 30) + (1 << 29)), "3.5 GB");
}

/// Everything below runs in a copy of this test program with the stand-in curl first on its path.
#[test]
fn with_a_stand_in_curl() {
    let scratch = Scratch::new();
    let bin = stand_in_curl(&scratch);
    run_with_path("downloads::pins::stand_in_curl_pins", &bin, true);
}

#[test]
#[ignore = "run by with_a_stand_in_curl, with the stand-in curl first on PATH"]
fn stand_in_curl_pins() {
    let Some(bin) = stand_in_here() else { return };
    let scratch = Scratch::new();
    let base = "--fail --location --silent --show-error --proto-redir =https --retry 3";

    // With a size: the file's names at every stage, each step said, and curl's words.
    let file = scratch.path("parts/ubuntu/ubuntu-26.04.1-live-server-amd64.iso");
    let folder = file.parent().unwrap().to_owned();
    ready(
        &bin,
        "HTTP/1.1 302 Found\r\nContent-Length: 3\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n",
        b"hello",
        b"world",
    );
    let wanted = sum_of(b"helloworld", &scratch);
    let said = RefCell::new(Vec::<String>::new());
    let during = RefCell::new(Vec::<String>::new());
    fetch_checked("https://example.invalid/disc.iso", &wanted, &file, "Downloading Ubuntu", &|step| {
        said.borrow_mut().push(step.to_owned());
        if step == "Downloading Ubuntu: 0%" {
            go_first(&bin);
        }
        if step == "Downloading Ubuntu: 50%" {
            *during.borrow_mut() = names_in(&folder);
            go(&bin);
        }
    })
    .unwrap();
    let mut said = said.into_inner();
    said.retain(|step| step != "Downloading Ubuntu: 100%");
    assert_eq!(
        said,
        ["Downloading Ubuntu", "Downloading Ubuntu: 0%", "Downloading Ubuntu: 50%", "Checking the download"]
    );
    assert_eq!(during.into_inner(), ["ubuntu-26.04.1-live-server-amd64.part"]);
    assert_eq!(
        names_in(&folder),
        ["ubuntu-26.04.1-live-server-amd64.iso", "ubuntu-26.04.1-live-server-amd64.sha256"]
    );
    let partial = folder.join("ubuntu-26.04.1-live-server-amd64.part");
    assert_eq!(
        asked(&bin),
        [
            format!("{base} --head https://example.invalid/disc.iso"),
            format!("{base} --continue-at - --output {} https://example.invalid/disc.iso", partial.display()),
        ]
    );

    // With no size: how much has come, in MB rounded up.
    let file = scratch.path("parts/android/image.qcow2");
    ready(&bin, "HTTP/1.1 200 OK\r\n\r\n", b"hello", b"world");
    let said = RefCell::new(Vec::<String>::new());
    fetch_checked("https://example.invalid/image", &wanted, &file, "Downloading Android", &|step| {
        said.borrow_mut().push(step.to_owned());
        if step == "Downloading Android: 0 MB" {
            go_first(&bin);
        }
        if step == "Downloading Android: 1 MB" {
            go(&bin);
        }
    })
    .unwrap();
    assert_eq!(
        said.into_inner(),
        [
            "Downloading Android",
            "Downloading Android: 0 MB",
            "Downloading Android: 1 MB",
            "Checking the download"
        ]
    );

    // A wrong file: the sentence, and its partial file gone.
    let file = scratch.path("parts/x/wrong.iso");
    ready(&bin, "content-length: 10\n", b"hello", b"world");
    go_first(&bin);
    go(&bin);
    let error = fetch_checked("https://example.invalid/w", &"0".repeat(64), &file, "Downloading X", &|_| {})
        .unwrap_err();
    assert_eq!(error.0, "Downloading X gave a file that isn't the one it should be. Try again.");
    assert_eq!(names_in(&scratch.path("parts/x")), Vec::<String>::new());

    // The size curl's head gives: the last length, from lines that start with it in any case.
    for (headers, size) in [
        ("HTTP/2 200\r\ncontent-length: 7\r\n\r\n", Some(7)),
        ("HTTP/2 200\r\nCONTENT-LENGTH:7\r\n\r\n", Some(7)),
        ("HTTP/2 301\r\ncontent-length: 1\r\n\r\nHTTP/2 200\r\ncontent-length: 9\r\n\r\n", Some(9)),
        ("HTTP/2 200\r\ncontent-length: 9\r\n\r\nHTTP/2 200\r\ncontent-length: lots\r\n\r\n", Some(9)),
        ("HTTP/2 200\r\n content-length: 7\r\n\r\n", None),
        ("HTTP/2 200\r\ncontent-length : 7\r\n\r\n", None),
        ("HTTP/2 200\r\nx-content-length: 7\r\n\r\n", None),
        ("", None),
    ] {
        ready(&bin, headers, b"", b"");
        assert_eq!(size_at("https://example.invalid/s"), size, "{headers:?}");
    }
    assert_eq!(asked(&bin), [format!("{base} --head https://example.invalid/s")]);

    // A small text, with curl's own words.
    fs::write(bin.join("text"), "aa *ubuntu.iso\n").unwrap();
    ready(&bin, "", b"", b"");
    assert_eq!(text_at("https://example.invalid/SHA256SUMS").unwrap(), "aa *ubuntu.iso\n");
    assert_eq!(asked(&bin), [format!("{base} https://example.invalid/SHA256SUMS")]);
}

/// With no curl on the path at all, each way in says so.
#[test]
fn with_no_curl() {
    let scratch = Scratch::new();
    let empty = scratch.path("empty-bin");
    fs::create_dir_all(&empty).unwrap();
    run_with_path("downloads::pins::no_curl_pins", &empty, false);
}

#[test]
#[ignore = "run by with_no_curl, with nothing on PATH"]
fn no_curl_pins() {
    if stand_in_here().is_none() {
        return;
    }
    let scratch = Scratch::new();
    let started = Instant::now();
    let error = text_at("https://example.invalid/x").unwrap_err();
    assert_eq!(error.0, "curl could not be started: No such file or directory (os error 2)");
    assert_eq!(size_at("https://example.invalid/x"), None);
    let file = scratch.path("parts/x/disc.iso");
    let error = fetch_checked("https://example.invalid/x", "aa", &file, "Downloading", &|_| {}).unwrap_err();
    assert_eq!(error.0, "curl could not be started: No such file or directory (os error 2)");
    assert_eq!(names_in(&scratch.path("parts/x")), Vec::<String>::new());
    assert!(sha256(&file).is_err());
    assert!(started.elapsed() < Duration::from_secs(60));
}
