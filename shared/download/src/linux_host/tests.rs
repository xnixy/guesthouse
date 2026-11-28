use super::*;

#[test]
fn a_partial_file_and_a_mark_replace_the_last_extension() {
    for (file, partial, marker) in [
        (
            "ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-amd64.part",
            "ubuntu-26.04.1-live-server-amd64.sha256",
        ),
        (
            "guesthouse-android-17-2026-11-23.qcow2",
            "guesthouse-android-17-2026-11-23.part",
            "guesthouse-android-17-2026-11-23.sha256",
        ),
        (
            "remoteman-host-setup-0.1.10.exe",
            "remoteman-host-setup-0.1.10.part",
            "remoteman-host-setup-0.1.10.sha256",
        ),
        ("archive.tar.xz", "archive.tar.part", "archive.tar.sha256"),
        ("no-extension", "no-extension.part", "no-extension.sha256"),
    ] {
        assert_eq!(partial_file(Path::new(file)), Path::new(partial));
        assert_eq!(checked_marker(Path::new(file)), Path::new(marker));
    }
}

#[test]
fn a_mark_and_a_sum_are_read_exactly() {
    assert!(marker_says("abc", "abc"));
    assert!(marker_says("  abc\n", "abc"));
    assert!(!marker_says("ABC", "abc"));
    assert!(!marker_says("", "abc"));
    assert!(sum_is_wanted("abc", "abc"));
    assert!(!sum_is_wanted("abc", "ABC"));
    assert!(!sum_is_wanted("abc ", "abc"));
    assert_eq!(sha256sum_sum(b"e3b0  /a/file\n").as_deref(), Some("e3b0"));
    assert_eq!(sha256sum_sum(b"  \n"), None);
    assert_eq!(said(b"  curl: (6) Could not resolve host\n"), "curl: (6) Could not resolve host");
}

#[test]
fn curl_is_given_these_words() {
    assert_eq!(
        CURL_WORDS,
        ["--fail", "--location", "--silent", "--show-error", "--proto-redir", "=https", "--retry", "3"]
    );
    assert_eq!(HEAD_WORD, "--head");
    assert_eq!(GO_ON_WORDS, ["--continue-at", "-", "--output"]);
}

#[test]
fn the_size_is_the_last_length_a_head_gives() {
    let head = |text: &str| content_length(text.as_bytes());
    assert_eq!(
        head("HTTP/2 301\r\ncontent-length: 1\r\n\r\nHTTP/2 200\r\nContent-Length: 9\r\n\r\n"),
        Some(9)
    );
    assert_eq!(head("HTTP/2 200\r\nCONTENT-LENGTH:7\r\n"), Some(7));
    assert_eq!(head("content-length: 9\r\ncontent-length: lots\r\n"), Some(9));
    assert_eq!(head(" content-length: 7\r\n"), None);
    assert_eq!(head("content-length : 7\r\n"), None);
    assert_eq!(head(""), None);
}

#[test]
fn how_far_a_download_is() {
    assert_eq!(percent(0, 10), 0);
    assert_eq!(percent(5, 10), 50);
    assert_eq!(percent(9, 10), 90);
    assert_eq!(percent(20, 10), 100);
    assert_eq!(percent(3, 0), 100);
    assert_eq!(percent(0, 0), 0);
    assert_eq!(progress("Downloading Ubuntu", 5, Some(10)), "Downloading Ubuntu: 50%");
    assert_eq!(progress("Downloading Android", 0, None), "Downloading Android: 0 MB");
    assert_eq!(progress("Downloading Android", 5, None), "Downloading Android: 1 MB");
    assert_eq!(progress("Downloading Android", 3 << 30, None), "Downloading Android: 3.0 GB");
}

#[test]
fn every_sentence() {
    assert_eq!(CHECKING, "Checking the download");
    assert_eq!(not_started("gone"), "curl could not be started: gone");
    assert_eq!(unreadable("https://a/b", "curl: (22) 404"), "https://a/b could not be read: curl: (22) 404");
    assert_eq!(stopped("Downloading Ubuntu", "curl: (56)"), "Downloading Ubuntu stopped: curl: (56)");
    assert_eq!(
        wrong_file("Downloading Ubuntu"),
        "Downloading Ubuntu gave a file that isn't the one it should be. Try again."
    );
}

#[test]
fn the_macos_part_has_its_own_names_and_sentences() {
    use macos_part::*;
    let into = Path::new("guesthouse-macos-part-1");
    assert_eq!(archive_file(into), Path::new("guesthouse-macos-part-1.part"));
    assert_eq!(unpacked_folder(into), Path::new("guesthouse-macos-part-1.new"));
    assert_eq!(size_text(22_435_448), "22 MiB");
    assert_eq!(size_text(1 << 20), "1 MiB");
    assert_eq!(progress(percent(22_435_448 / 2, 22_435_448)), "Downloading macOS support: 50%");
    assert_eq!(DOWNLOADING, "Downloading macOS support");
    assert_eq!(UNPACKING, "Unpacking macOS support");
    assert_eq!(WRONG_FILE, "The macOS support that came is not the file it should be. Try again.");
    assert_eq!(not_downloaded("curl: (6)"), "macOS support could not be downloaded: curl: (6)");
    assert_eq!(not_unpacked(""), "macOS support could not be unpacked. ");
}
