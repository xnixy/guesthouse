use super::*;
use crate::machine::load;
use crate::network::Mode;

fn refused(context: &Context, words: &str) -> String {
    match said(context, words).1 {
        Err(Problem::Failed(failure)) => failure.0,
        other => panic!("{words:?} gave {other:?}"),
    }
}

#[test]
fn a_setting_is_changed_and_kept() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    assert_eq!(said(&context, "set work cores 1"), ("cores is now 1.\n".to_owned(), Ok(())));
    assert_eq!(said(&context, "set work ram 2").1, Ok(()));
    let machine = load(&context, "work").unwrap();
    assert_eq!((machine.cores, machine.ram_gib), (1, 2));
}

#[test]
fn a_setting_out_of_range_is_refused_with_the_range() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    assert!(refused(&context, "set work cores 0").starts_with("Cores must be from 1 to "));
    assert!(refused(&context, "set work ram 9999").starts_with("RAM in GiB must be from 1 to "));
    assert_eq!(refused(&context, "set work cores lots"), "cores must be a whole number.");
    assert_eq!(load(&context, "work").unwrap().cores, 8, "a refusal changes nothing");
}

#[test]
fn what_the_app_greys_out_is_refused_on_a_running_machine() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = make(&context, "work", "Office PC", Kind::Plain);
    running(&context, &machine, 40);
    for words in [
        "set work cores 1",
        "set work ram 2",
        "set work network none",
        "set work share none",
        "forward work add tcp 8080 80",
    ] {
        assert_eq!(refused(&context, words), "The VM must be off to change this.", "{words}");
    }
}

#[test]
fn a_setting_a_kind_of_machine_does_not_have_is_refused() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    make(&context, "desk", "Desk", Kind::Linux);
    assert_eq!(refused(&context, "set work vram 1"), "vram is only for a Windows machine with GPU-PV.");
    assert_eq!(
        refused(&context, "set work screen 1920x1080"),
        "screen is only for a Windows machine with GPU-PV."
    );
    assert_eq!(
        refused(&context, "set desk network none"),
        "An Ubuntu machine's network is fixed, with its SSH port forwarded."
    );
    assert_eq!(
        refused(&context, "forward desk add tcp 8080 80"),
        "An Ubuntu machine's network is fixed, with its SSH port forwarded."
    );
    assert!(
        refused(&context, "set work colour red")
            .starts_with("There is no setting called colour. They are cores, ram,")
    );
}

#[test]
fn the_network_the_bridge_and_the_shared_folder_are_set_by_the_pages_calls() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    assert_eq!(said(&context, "set work network bridged").1, Ok(()));
    let machine = load(&context, "work").unwrap();
    assert_eq!(machine.network_mode, Mode::Bridged);
    assert!(!machine.mac.is_empty(), "a bridged machine gets an address of its own");
    assert_eq!(said(&context, "set work bridge br1").1, Ok(()));
    assert_eq!(load(&context, "work").unwrap().bridge, "br1");
    assert_eq!(refused(&context, "set work network wifi"), "The network is nat, none or bridged.");
    assert_eq!(refused(&context, "set work passt on"), "passt is for the NAT network.");
    let folder = scratch.path("home/shared");
    fs::create_dir_all(&folder).unwrap();
    assert_eq!(said(&context, &format!("set work share {}", folder.display())).1, Ok(()));
    assert_eq!(load(&context, "work").unwrap().share, folder.display().to_string());
    assert_eq!(said(&context, "set work share none").1, Ok(()));
    assert_eq!(load(&context, "work").unwrap().share, "");
    assert!(
        refused(&context, &format!("set work share {}", scratch.path("home").display()))
            .starts_with("This folder can't be shared.")
    );
}

#[test]
fn the_screen_and_the_rate_of_a_windows_machine_are_set() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "win", "Win", Kind::Windows);
    assert_eq!(said(&context, "set win screen 2560x1440").1, Ok(()));
    assert_eq!(said(&context, "set win refresh 100").1, Ok(()));
    let machine = load(&context, "win").unwrap();
    assert_eq!((machine.screen.as_str(), machine.refresh_hz), ("2560x1440", 100));
    assert_eq!(
        refused(&context, "set win screen 800x600"),
        "The screen size must be one of 1920x1080, 2560x1440, 3840x2160."
    );
    assert_eq!(refused(&context, "set win refresh fast"), crate::machine::refresh_limits());
}

#[test]
fn a_port_is_forwarded_listed_and_taken_back() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    assert_eq!(
        said(&context, "forward work add tcp 28389 3389"),
        ("Forwarded TCP 28389 to 3389.\n".to_owned(), Ok(()))
    );
    assert!(said(&context, "show work").0.contains("forwards  TCP 28389 to 3389\n"));
    assert!(refused(&context, "forward work add tcp 28389 80").contains("already forwarded"));
    assert_eq!(refused(&context, "forward work add icmp 1 2"), "The protocol is tcp or udp.");
    assert_eq!(refused(&context, "forward work remove tcp 28389 80"), "TCP 28389 to 80 is not forwarded.");
    assert_eq!(said(&context, "forward work remove tcp 28389 3389").1, Ok(()));
    assert!(said(&context, "show work").0.contains("forwards  none\n"));
    assert!(usage_of("forward work toggle tcp 1 2").contains("forward is add or remove, not toggle."));
}

#[test]
fn show_lists_what_the_kind_of_machine_has() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    make(&context, "work", "Office PC", Kind::Plain);
    make(&context, "win", "Win", Kind::Windows);
    let plain = said(&context, "show work").0;
    assert!(plain.starts_with("name      Office PC\nid        work\nfolder    "), "{plain}");
    assert!(
        plain.contains("network   NAT\npasst     off\nshare     none\n") && !plain.contains("vram"),
        "{plain}"
    );
    let windows = said(&context, "show win").0;
    assert!(
        windows.contains("vram      no cap\nscreen    1920x1080\nrefresh   144 Hz\nsound     on\n"),
        "{windows}"
    );
}
