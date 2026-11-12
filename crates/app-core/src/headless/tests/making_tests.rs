use super::*;
use crate::host::{RamField, physical_cores};
use crate::iso::stand_in::{make_iso, wim};
use crate::machine::load;
use crate::settings::driver_folder;
use crate::settings::read_setup;
use crate::setup::cards::stand_in::{NAMES, fake_pci};
use crate::setup::checks::{Check, CheckState};
use crate::setup::driver::stand_in::good_installer;
use crate::setup::driver::{WINDOWS_DRIVER, driver_problem};
use setup::setup_after;
use std::path::Path;

fn plain_disc(scratch: &Scratch) -> String {
    make_iso(scratch, "tiny", &[("readme.txt", b"hello")])
}

fn failure_of(result: std::result::Result<(), Problem>) -> String {
    match result {
        Err(Problem::Failed(failure)) => failure.0,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_plain_machine_is_made_with_the_dialogs_defaults() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let (out, result) = said(&context, &format!("new Tiny --iso {}", plain_disc(&scratch)));
    assert_eq!(result, Ok(()));
    assert!(out.starts_with("Not Windows: a plain VM. You install it by hand."), "{out}");
    assert!(out.ends_with("Start it with: guesthouse-vm start tiny\n"), "{out}");
    let machine = load(&context, "tiny").unwrap();
    assert_eq!((machine.kind, machine.name.as_str()), (Kind::Plain, "Tiny"));
    assert_eq!(machine.cores, 4.min(physical_cores(&context.cpus)));
    assert_eq!(u64::from(machine.ram_gib), RamField::for_plain(&context, 1, 4).value);
    assert!(Path::new(&machine.disk).exists() && !machine.installing);
}

#[test]
fn what_is_asked_for_replaces_the_default() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let disc = plain_disc(&scratch);
    let (_, result) =
        said(&context, &format!("new Small --iso {disc} --kind plain --cores 1 --ram 1 --disk 2"));
    assert_eq!(result, Ok(()));
    let machine = load(&context, "small").unwrap();
    assert_eq!((machine.cores, machine.ram_gib), (1, 1));
}

#[test]
fn a_new_machine_is_refused_in_the_dialogs_words() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let disc = plain_disc(&scratch);
    let refused = |words: &str| failure_of(said(&context, words).1);
    assert_eq!(refused("new Tiny"), "new needs --iso PATH, the disc to install from.");
    assert_eq!(
        refused(&format!("new Tiny --iso {disc} --kind windows")),
        "This disc makes a plain VM here, and not Windows with GPU-PV."
    );
    assert_eq!(
        refused(&format!("new Tiny --iso {disc} --kind nonsense")),
        "The kind is windows, ubuntu or plain, not nonsense."
    );
    assert_eq!(refused(&format!("new Tiny --iso {disc} --cores many")), "--cores must be a whole number.");
    assert_eq!(
        refused(&format!("new Tiny --iso {disc} --cores 0")),
        "Cores must be a whole number, at least 1."
    );
    assert_eq!(
        refused(&format!("new Tiny --iso {disc} --disk 0")),
        "The disk must be between 1 and 16384 GiB."
    );
    assert_eq!(
        refused(&format!("new Tiny --iso {disc} --user ana")),
        "A plain VM has no account to name. It is installed by hand."
    );
    assert_eq!(
        refused(&format!("new Tiny --iso {disc} --amd-card 0000:7c:00.0")),
        "Only a Windows machine chooses its card here."
    );
    assert_eq!(refused("new Tiny --iso /nowhere.iso"), refused("new Tiny --iso /nowhere.iso"));
    assert!(load_all(&context).0.is_empty(), "nothing was made");
}

#[test]
fn a_windows_disc_says_what_it_is_and_what_stops_it() {
    let scratch = Scratch::new();
    let mut context = scratch_context(&scratch);
    // The room an install needs depends on the disk this test runs on.
    context.install_room = Some(0);
    let data = wim(&["Windows 11 Pro"], 26300, 9);
    let disc = make_iso(&scratch, "win", &[("sources/install.wim", &data)]);
    let (out, result) = said(&context, &format!("new Win --iso {disc}"));
    assert!(out.starts_with("Windows 11 Pro, build 26300: Windows with GPU-PV."), "{out}");
    let refusal = failure_of(result);
    assert!(refusal.contains("driver"), "{refusal}");
}

#[test]
fn a_machine_with_nothing_to_install_has_no_install_to_run() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    assert_eq!(failure_of(said(&context, "install work").1), "Office PC has no install to run.");
}

#[test]
fn a_machine_is_only_deleted_when_it_is_said_twice() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let (_, made) =
        said(&context, &format!("new Tiny --iso {} --cores 1 --ram 1 --disk 1", plain_disc(&scratch)));
    assert_eq!(made, Ok(()));
    let asked = failure_of(said(&context, "remove tiny").1);
    assert!(asked.starts_with("This deletes Tiny, its disk and everything on it, in "), "{asked}");
    assert!(asked.ends_with("It cannot be undone. Run it again with --yes to go on."), "{asked}");
    assert!(load(&context, "tiny").is_ok());
    let machine = load(&context, "tiny").unwrap();
    running(&context, &machine, 40);
    assert_eq!(failure_of(said(&context, "remove tiny --yes").1), "Shut the VM down first.");
    fs::remove_dir_all(context.processes.join("40")).unwrap();
    assert_eq!(said(&context, "remove tiny --yes"), ("Removed Tiny.\n".to_owned(), Ok(())));
    assert!(load_all(&context).0.is_empty());
}

#[test]
fn a_machine_whose_disk_is_elsewhere_is_only_forgotten() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let elsewhere = scratch.write("disks/work.qcow2", "disk");
    let machine =
        Machine { disk: elsewhere.display().to_string(), ..make(&context, "work", "Office PC", Kind::Plain) };
    machine.save(&context).unwrap();
    let asked = failure_of(said(&context, "remove work").1);
    assert!(asked.starts_with("Only the entry of Office PC goes. Its disk stays at "), "{asked}");
    assert_eq!(said(&context, "remove work --yes").1, Ok(()));
    assert!(elsewhere.exists());
}

fn passing(title: &str) -> Check {
    Check { title: title.into(), state: CheckState::Ok, text: "fine".into(), fix: String::new() }
}

fn with_cards(scratch: &Scratch, cards: &[(&str, u32)]) -> Context {
    let mut context = scratch_context(scratch);
    context.pci_devices = scratch.path("pci");
    context.pci_names = scratch.write("pci.ids", NAMES);
    for (address, device) in cards {
        fake_pci(scratch, address, *device, "0x10de", "0x030000");
    }
    context
}

#[test]
fn setup_stops_at_a_check_that_failed_and_says_which() {
    let scratch = Scratch::new();
    let context = with_cards(&scratch, &[]);
    let failed = Check { state: CheckState::Failed, fix: "Plug one in.".into(), ..passing("NVIDIA card") };
    let mut out = Vec::new();
    let result = setup_after(&context, &[passing("KVM"), failed], None, None, &mut out);
    assert!(result.unwrap_err().0.starts_with("Windows with GPU-PV cannot run yet."));
    let said = String::from_utf8(out).unwrap();
    assert_eq!(said, "ok       KVM: fine\nfailed   NVIDIA card: fine\n         Plug one in.\n");
}

#[test]
fn setup_wants_the_driver_installer_when_it_has_no_files() {
    let scratch = Scratch::new();
    let context = with_cards(&scratch, &[("0000:01:00.0", 0x2B85)]);
    let mut out = Vec::new();
    let refusal = setup_after(&context, &[passing("KVM")], None, None, &mut out).unwrap_err().0;
    assert!(
        refusal.contains(&format!("driver {WINDOWS_DRIVER}")) && refusal.contains("--driver PATH"),
        "{refusal}"
    );
    assert!(!read_setup(&context).contains_key("card"), "nothing is written down before it is all there");
}

#[test]
fn setup_copies_the_driver_and_writes_down_the_card_and_is_quiet_the_second_time() {
    let scratch = Scratch::new();
    let context = with_cards(&scratch, &[("0000:01:00.0", 0x2B85)]);
    let installer = good_installer(&scratch);
    let mut out = Vec::new();
    setup_after(&context, &[passing("KVM")], Some(&installer), None, &mut out).unwrap();
    let said = String::from_utf8(out).unwrap();
    assert!(said.contains("Copying the driver's files\n"), "{said}");
    assert!(said.contains("The files are in "), "{said}");
    assert!(said.contains("GeForce RTX 5090 at 0000:01:00.0."), "{said}");
    assert!(said.ends_with("Windows 11 ISO, or an Ubuntu server ISO.\n"));
    assert!(driver_problem(&driver_folder(&context)).is_none());
    assert_eq!(read_setup(&context)["card"]["pci"], "0000:01:00.0");
    let mut again = Vec::new();
    setup_after(&context, &[passing("KVM")], None, None, &mut again).unwrap();
    assert!(String::from_utf8(again).unwrap().starts_with("ok       KVM: fine\nDriver 617.42 is ready."));
}

#[test]
fn setup_takes_the_card_it_was_told_on_a_pc_with_two() {
    let scratch = Scratch::new();
    let context = with_cards(&scratch, &[("0000:01:00.0", 0x2B85), ("0000:02:00.0", 0x2204)]);
    let installer = good_installer(&scratch);
    let mut out = Vec::new();
    setup_after(&context, &[passing("KVM")], Some(&installer), Some("0000:02:00.0"), &mut out).unwrap();
    assert_eq!(read_setup(&context)["card"]["pci"], "0000:02:00.0");
    assert!(String::from_utf8(out).unwrap().contains("at 0000:02:00.0."));
    let refusal = setup_after(&context, &[passing("KVM")], None, Some("0000:09:00.0"), &mut Vec::new());
    assert_eq!(refusal.unwrap_err().0, "There is no NVIDIA card at 0000:09:00.0.");
}
