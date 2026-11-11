//! The repo's own guest folder: everything the app sends is there, and nothing that must not be.

use std::fs;
use std::path::PathBuf;

const SENT_TO_THE_GUEST: [&str; 16] = [
    "SetupComplete.cmd",
    "agent.ps1",
    "api-check.ps1",
    "autounattend.xml",
    "check.ps1",
    "clipboard-ask.ps1",
    "clipboard-start.ps1",
    "clipboard.ps1",
    "display-remove.ps1",
    "display.ps1",
    "finish.ps1",
    "nv-files.ps1",
    "nv-profiles.ps1",
    "setmode.ps1",
    "setup.ps1",
    "share-start.ps1",
];

fn guest_file(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../guest").join(name)
}

#[test]
fn everything_the_app_sends_is_in_the_repo() {
    for name in SENT_TO_THE_GUEST {
        assert!(guest_file(name).is_file(), "{name} is missing");
    }
}

#[test]
fn the_answer_file_holds_no_password() {
    let answers = fs::read_to_string(guest_file("autounattend.xml")).unwrap();
    assert_eq!(answers.matches("<Value>@PASSWORD@</Value>").count(), 2);
    assert_eq!(answers.matches("<PlainText>true</PlainText>").count(), 2);
}

#[test]
fn the_account_is_named_by_the_host_everywhere_the_scripts_need_it() {
    let answers = fs::read_to_string(guest_file("autounattend.xml")).unwrap();
    assert_eq!(answers.matches("@USER@").count(), 4, "full name, name, display name and the sign-in");
    assert!(!answers.contains(">gpupv<"), "the default name belongs to the app, not the template");
    for name in ["finish.ps1", "check.ps1"] {
        let script = fs::read_to_string(guest_file(name)).unwrap();
        assert!(script.contains("$user = '@USER@'"), "{name} takes the name from the host");
        assert!(!script.contains("gpupv$"), "{name} still matches the default name by hand");
    }
}

/// Windows names its groups in its own language, so a script that asks for one by its English
/// name works on an English Windows only.
#[test]
fn no_script_finds_a_group_by_its_english_name() {
    for name in SENT_TO_THE_GUEST.iter().filter(|name| name.ends_with(".ps1")) {
        let script = fs::read_to_string(guest_file(name)).unwrap();
        for line in script.lines().filter(|line| !line.trim_start().starts_with('#')) {
            assert!(!line.contains("Administrators"), "{name} names the Administrators group: {line}");
        }
    }
    let check = fs::read_to_string(guest_file("check.ps1")).unwrap();
    assert!(check.contains("-SID $administrators"), "the check finds the group by its number");
    assert!(check.contains("$administrators = 'S-1-5-32-544'"));
}

#[test]
fn the_only_key_is_the_published_one_that_activates_nothing() {
    let answers = fs::read_to_string(guest_file("autounattend.xml")).unwrap();
    assert_eq!(answers.matches("<ProductKey>").count(), 1);
    assert!(answers.contains("<Key>W269N-WFGWX-YVC9B-4J6C9-T83GX</Key>"));
}

#[test]
fn only_the_host_reaches_the_agent() {
    let setup = fs::read_to_string(guest_file("setup.ps1")).unwrap();
    assert!(setup.contains("-RemoteAddress 10.0.2.2"));
    let agent = fs::read_to_string(guest_file("agent.ps1")).unwrap();
    assert!(agent.contains("$hostAddress = '10.0.2.2'"));
    assert!(agent.contains("RemoteEndPoint"));
    assert!(agent.contains("ReadTimeout"));
}

#[test]
fn no_token_is_kept_in_the_folder() {
    assert!(!guest_file(".token").exists());
    assert!(!guest_file("token.txt").exists());
}
