//! The rules every QEMU line follows for the files it opens, held by the tests of each kind of boot:
//!
//! - A `-drive` with a file names its `format=`, so QEMU never works it out from the file's bytes.
//! - A disk QEMU writes is a `-blockdev` node. A node in a format that has backing files gives its
//!   `backing` explicitly, even when it has none, so QEMU never takes one from the file's header.
//! - A `-drive` that is written is firmware's flash. Every other `-drive` is read only or keeps
//!   nothing it is written.
//! - The short flags for disks (`-hda`, `-cdrom` and the like) are not used.

use serde_json::Value;

/// The short flags that open a file with no `format=` to name.
const SHORT_FLAGS: [&str; 10] =
    ["-hda", "-hdb", "-hdc", "-hdd", "-cdrom", "-fda", "-fdb", "-mtdblock", "-sd", "-pflash"];

/// The node drivers that are all data, so they have no backing files to name.
const FLAT_DRIVERS: [&str; 3] = ["raw", "file", "host_device"];

/// Panics with what the line does that the rules do not allow.
pub fn check(arguments: &[String]) {
    for (at, word) in arguments.iter().enumerate() {
        assert!(
            !SHORT_FLAGS.contains(&word.as_str()),
            "{word} opens a file with no format named: {arguments:?}"
        );
        let Some(value) = arguments.get(at + 1) else { continue };
        match word.as_str() {
            "-drive" => check_drive(value),
            "-blockdev" => check_blockdev(value),
            _ => {}
        }
    }
}

/// The options of a `-drive` word, with QEMU's doubled comma kept inside its value.
fn options(word: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut at) = (0, 0);
    while at < word.len() {
        if word[at..].starts_with(",,") {
            at += 2;
        } else if word[at..].starts_with(',') {
            parts.push(&word[start..at]);
            at += 1;
            start = at;
        } else {
            at += 1;
        }
    }
    parts.push(&word[start..]);
    parts
}

fn check_drive(word: &str) {
    let options = options(word);
    let has = |name: &str| options.iter().any(|option| option.split('=').next() == Some(name));
    let is = |option: &str| options.contains(&option);
    if !has("file") {
        return;
    }
    assert!(has("format"), "a drive with a file names its format: {word}");
    let kept_nothing = is("readonly=on") || is("snapshot=on");
    assert!(
        kept_nothing || is("if=pflash"),
        "a disk that is written is a -blockdev node, not a -drive: {word}"
    );
}

fn check_blockdev(word: &str) {
    if word.starts_with('{') {
        let node: Value = serde_json::from_str(word).unwrap_or_else(|error| panic!("{error}: {word}"));
        assert!(node["node-name"].is_string(), "a node is named: {word}");
        check_node(&node, word);
    } else {
        let driver = options(word).into_iter().find_map(|option| option.strip_prefix("driver="));
        let driver = driver.unwrap_or_else(|| panic!("a node names its driver: {word}"));
        assert!(
            FLAT_DRIVERS.contains(&driver),
            "a driver with backing files is given as a json node: {word}"
        );
    }
}

fn check_node(node: &Value, word: &str) {
    let driver = node["driver"].as_str().unwrap_or_else(|| panic!("a node names its driver: {word}"));
    if !FLAT_DRIVERS.contains(&driver) {
        assert!(node.get("backing").is_some(), "a {driver} node gives its backing, null if none: {word}");
    }
    for below in ["file", "backing"] {
        if node[below].is_object() {
            check_node(&node[below], word);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::check;

    fn line(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn a_line_that_follows_the_rules_passes() {
        check(&line(&[
            "-drive",
            "if=pflash,format=raw,unit=1,file=/m/a,,b.fd",
            "-drive",
            "if=none,id=cd0,format=raw,media=cdrom,readonly=on",
            "-drive",
            "id=i,if=none,format=raw,snapshot=on,file=/m/i.img",
            "-blockdev",
            "driver=host_device,node-name=drive1,filename=/dev/fdset/40",
            "-blockdev",
            r#"{"driver":"qcow2","node-name":"hd0","file":{"driver":"file","filename":"/m/d"},"backing":null}"#,
        ]));
    }

    #[test]
    #[should_panic(expected = "names its format")]
    fn a_drive_with_a_file_and_no_format_is_refused() {
        check(&line(&["-drive", "if=none,id=cd0,media=cdrom,readonly=on,file=/m/a.iso"]));
    }

    #[test]
    #[should_panic(expected = "names its format")]
    fn a_format_inside_a_path_is_not_a_format() {
        check(&line(&["-drive", "if=none,readonly=on,file=/m/format=raw"]));
    }

    #[test]
    #[should_panic(expected = "-blockdev node")]
    fn a_written_drive_that_is_not_flash_is_refused() {
        check(&line(&["-drive", "if=none,id=hd,format=qcow2,file=/m/disk.qcow2"]));
    }

    #[test]
    #[should_panic(expected = "no format named")]
    fn the_short_flags_are_refused() {
        check(&line(&["-cdrom", "/m/a.iso"]));
    }

    #[test]
    #[should_panic(expected = "gives its backing")]
    fn a_qcow2_node_with_no_backing_given_is_refused() {
        check(&line(&[
            "-blockdev",
            r#"{"driver":"qcow2","node-name":"hd0","file":{"driver":"file","filename":"/m/d"}}"#,
        ]));
    }

    #[test]
    #[should_panic(expected = "gives its backing")]
    fn a_backing_node_with_no_backing_given_is_refused() {
        check(&line(&[
            "-blockdev",
            r#"{"driver":"qcow2","node-name":"hd0","file":{"driver":"file","filename":"/m/d"},
            "backing":{"driver":"qcow2","file":{"driver":"file","filename":"/m/b"}}}"#,
        ]));
    }

    #[test]
    #[should_panic(expected = "json node")]
    fn a_qcow2_node_in_the_short_form_is_refused() {
        check(&line(&["-blockdev", "driver=qcow2,node-name=hd0,file.driver=file,file.filename=/m/d"]));
    }
}
