use super::*;
use crate::gpu_identity::NVIDIA_RENDER;
use crate::machine::Kind;
use crate::network::Protocol;
use crate::slots;
use crate::testing::{Scratch, scratch_context};

fn plan(machine: &Machine, boot: Boot) -> (Scratch, VmPlan) {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let plan = VmPlan::new(&context, machine, boot, slots::first(), 0);
    (scratch, plan)
}

fn windows() -> Machine {
    Machine::new("win", "Win", "/m/disk.qcow2", "/m/vars.fd")
}

fn plain_machine() -> Machine {
    Machine { kind: Kind::Plain, ..windows() }
}

fn plain_boot() -> Boot {
    Boot::Plain { iso: String::new() }
}

fn value_after(arguments: &[String], flag: &str) -> String {
    let at = arguments.iter().position(|word| word == flag).unwrap();
    arguments[at + 1].clone()
}

fn has(arguments: &[String], word: &str) -> bool {
    arguments.iter().any(|found| found == word)
}

/// The guest has seen this order, so a change here is a change for every machine.
#[test]
fn a_gpu_vms_arguments_stay_word_for_word_the_same() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let run_folder = PathBuf::from("/x/run");
    let plan = VmPlan {
        qemu_files: Some("/x/p/qemu/pc-bios".into()),
        firmware: Firmware::fedora(),
        run_folder,
        ..plan
    };
    // The file has the window where a wide host puts it. A narrow host puts it lower.
    let base = format!("{:#x}", guesthouse_daemon::guest_memory::window_base());
    let expected: Vec<String> =
        include_str!("gpu-arguments.txt").lines().map(|line| line.replace("0x10000000000", &base)).collect();
    assert_eq!(plan.qemu_arguments(), expected);
}

/// A QEMU that opens the memory by path has the same machine, and only these two words change.
#[test]
fn memory_by_path_changes_the_two_paths_and_nothing_else() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let files = memory_files_in(Path::new("/dev/shm"), slots::first());
    let plan = VmPlan {
        qemu_files: Some("/x/p/qemu/pc-bios".into()),
        firmware: Firmware::fedora(),
        run_folder: PathBuf::from("/x/run"),
        ..plan
    }
    .standalone(files, Path::new("/dev/disk/by-id"));
    let base = format!("{:#x}", guesthouse_daemon::guest_memory::window_base());
    let expected: Vec<String> = include_str!("gpu-arguments.txt")
        .lines()
        .map(|line| line.replace("0x10000000000", &base))
        .map(|line| line.replace("/proc/self/fd/12", "/dev/shm/gpupv-window.standalone"))
        .map(|line| line.replace("/proc/self/fd/13", "/dev/shm/gpupv-ram.standalone"))
        .collect();
    assert_eq!(plan.qemu_arguments(), expected);
    assert_eq!(plan.qemu_command()[0], plan.qemu.to_string_lossy(), "nothing goes in front of QEMU");
}

#[test]
fn a_gpu_vm_gets_the_time_sync_channel_after_its_bus() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let arguments = plan.qemu_arguments();
    let at = |word: &str| arguments.iter().position(|argument| argument.starts_with(word));
    assert!(at("vmbus-bridge") < at("hv-timesync"));
    assert!(at("hv-timesync").is_some());
}

#[test]
fn a_plain_vm_has_no_gpu_devices() {
    let (_scratch, plan) = plan(&plain_machine(), plain_boot());
    let arguments = plan.qemu_arguments();
    assert!(!arguments.iter().any(|word| word.contains("gpupv-") || word.contains("vmbus")));
    assert!(has(&arguments, "if=none,id=cd0,media=cdrom,readonly=on"), "an empty tray");
    assert!(!has(&arguments, "usb-mouse,bus=xhci.0"));
}

/// Firmware, an install and a plain VM have no GPU frames, so QEMU's own display is all they have.
#[test]
fn every_kind_of_boot_has_the_display_the_viewer_reads() {
    let iso = String::new();
    let install = Boot::Install { iso: iso.clone(), answer: "/m/answer.iso".into() };
    for (machine, boot) in [(plain_machine(), plain_boot()), (windows(), install), (windows(), Boot::Gpu)] {
        let (_scratch, plan) = plan(&machine, boot);
        let arguments = plan.qemu_arguments();
        assert_eq!(value_after(&arguments, "-display"), "dbus,p2p=yes");
        assert!(arguments.iter().any(|word| word.contains("/run/display.sock,server=on")));
        assert!(!has(&arguments, "-vnc"));
    }
}

#[test]
fn a_gpu_vm_has_its_devices() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert!(arguments.iter().any(|word| word.starts_with("gpupv-render,")));
    assert!(has(&arguments, "usb-mouse,bus=xhci.0"), "for games");
    assert!(
        arguments.iter().any(|word| word.contains("mem-path=/proc/self/fd/13,")),
        "the memfd it's handed"
    );
}

/// A machine shows Microsoft's identity until its folder says it holds the package for NVIDIA's.
#[test]
fn the_render_device_shows_the_identity_the_machine_is_ready_for() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let render = |plan: &VmPlan| {
        plan.qemu_arguments().into_iter().find(|word| word.starts_with("gpupv-render,")).unwrap()
    };
    assert!(!render(&plan).contains("vendor-id"), "QEMU's own identity needs no properties");
    let plan = VmPlan { render_identity: NVIDIA_RENDER, ..plan };
    assert!(render(&plan).ends_with(",vendor-id=0x10de,device-id=0x2bfe"));
}

#[test]
fn the_slot_sets_every_name() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let plan = VmPlan::new(&context, &windows(), Boot::Gpu, slots::number(2), 0);
    let arguments = plan.qemu_arguments();
    assert!(value_after(&arguments, "-nic").contains("hostfwd=tcp:127.0.0.1:18767-:8765"));
    assert!(arguments.iter().any(|word| word.contains("/run-2/qmp.sock,")));
}

/// The helper for dragged files is Windows', and only a Windows guest has the agent that starts it.
#[test]
fn only_a_windows_guest_gets_the_forward_for_dragged_files() {
    let (_scratch, plan) = plan(&plain_machine(), plain_boot());
    assert_eq!(plan.files_port, None);
    assert!(!value_after(&plan.qemu_arguments(), "-nic").contains("8767"));
}

#[test]
fn nothing_set_is_the_line_every_machine_had() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert_eq!(
        value_after(&arguments, "-nic"),
        "user,model=e1000e,hostfwd=tcp:127.0.0.1:18765-:8765,hostfwd=tcp:127.0.0.1:18865-:8767,\
         hostfwd=udp:127.0.0.1:19065-:7791"
    );
    assert!(!has(&arguments, "-netdev"), "NAT has one adapter");
}

#[test]
fn no_internet_keeps_the_agent() {
    let machine = Machine { network_mode: Mode::None, ..windows() };
    let (_scratch, plan) = plan(&machine, Boot::Gpu);
    let nic = value_after(&plan.qemu_arguments(), "-nic");
    assert!(nic.starts_with(
        "user,model=e1000e,restrict=on,hostfwd=tcp:127.0.0.1:18765-:8765,hostfwd=tcp:127.0.0.1:18865-:8767"
    ));
}

#[test]
fn bridged_goes_through_the_helper_and_adds_the_private_link_last() {
    let machine = Machine {
        network_mode: Mode::Bridged,
        mac: "52:54:00:aa:bb:cc".into(),
        forwards: vec![Forward::new(Protocol::Udp, 5000, 50)],
        ..windows()
    };
    let (_scratch, plan) = plan(&machine, Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert_eq!(
        value_after(&arguments, "-nic"),
        format!(
            "bridge,model=e1000e,mac=52:54:00:aa:bb:cc,br=br0,helper={}",
            crate::network::bridge_helper()
        )
    );
    assert!(
        value_after(&arguments, "-netdev")
            .ends_with("-:8767,hostfwd=udp:127.0.0.1:19065-:7791,hostfwd=udp:127.0.0.1:5000-:50")
    );
    assert_eq!(arguments.last().unwrap(), "e1000e,netdev=link,mac=52:54:00:67:68:01,bus=pcie.0,addr=0x1e");
}

#[test]
fn passt_has_its_own_addresses_and_the_private_link() {
    let (_scratch, mut plan) = plan(&windows(), Boot::Gpu);
    plan.network.passt = true;
    plan.network.resolver = Some("192.168.1.1".into());
    let arguments = plan.qemu_arguments();
    let nic = value_after(&arguments, "-nic");
    assert!(nic.starts_with("passt,model=e1000e,mtu=1500,address=10.0.3.15,"));
    assert!(nic.ends_with("dns-forward=10.0.3.3,dns-host=192.168.1.1"));
    assert!(has(&arguments, "-netdev"));
    let words = plan.passt_words();
    assert!(words.windows(2).any(|two| two == ["--dns-host", "192.168.1.1"]), "{words:?}");
    assert!(words.windows(2).any(|two| two == ["--map-host-loopback", "none"]), "{words:?}");
}

fn held_to(interface: &str, ipv6: bool) -> NetworkPlan {
    let through = OnlyThrough { interface: interface.into(), ipv6 };
    NetworkPlan { passt: true, only_through: Some(through), ..plan(&windows(), Boot::Gpu).1.network }
}

#[test]
fn an_interface_with_ipv6_binds_both_families_to_it() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let plan = VmPlan { network: held_to("wg0", true), ..plan };
    let words = plan.passt_words();
    assert!(words.windows(2).any(|two| two == ["--outbound-if4", "wg0"]), "{words:?}");
    assert!(words.windows(2).any(|two| two == ["--outbound-if6", "wg0"]), "{words:?}");
    assert!(!words.contains(&"--ipv4-only".to_owned()));
}

#[test]
fn an_interface_without_ipv6_makes_the_guest_ipv4_only() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let plan = VmPlan { network: held_to("tun0", false), ..plan };
    let words = plan.passt_words();
    assert!(words.windows(2).any(|two| two == ["--outbound-if4", "tun0"]), "{words:?}");
    assert!(words.contains(&"--ipv4-only".to_owned()));
    assert!(!words.contains(&"--outbound-if6".to_owned()));
}

#[test]
fn a_machine_that_is_not_held_to_an_interface_has_none_of_these_words() {
    let (_scratch, mut plan) = plan(&windows(), Boot::Gpu);
    plan.network.passt = true;
    let words = plan.passt_words();
    assert!(!words.iter().any(|word| word.starts_with("--outbound") || word == "--ipv4-only"), "{words:?}");
}

#[test]
fn the_setting_takes_passt_whatever_the_switch_says_and_only_in_nat() {
    let held = Machine { only_through: "wg0".into(), dns: "10.64.0.1".into(), ..windows() };
    let (_scratch, nat) = plan(&held, Boot::Gpu);
    assert!(nat.network.passt, "the switch is off");
    assert_eq!(nat.network.only_through.as_ref().map(|found| found.interface.as_str()), Some("wg0"));
    assert_eq!(nat.network.resolver.as_deref(), Some("10.64.0.1"), "the typed server is the guest's");
    let bridged = Machine { network_mode: Mode::Bridged, ..held };
    let (_scratch, bridged) = plan(&bridged, Boot::Gpu);
    assert_eq!(bridged.network.only_through, None);
    assert!(!bridged.network.passt);
}

#[test]
fn the_launchers_passt_is_never_swapped_for_qemus_own_network() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let plan = VmPlan { jail: None, network: held_to("wg0", true), ..plan };
    let arguments = plan.qemu_arguments();
    let nic = value_after(&arguments, "-nic");
    assert_eq!(nic, format!("socket,model=e1000e,fd={PASST_FD}"));
}

#[test]
fn passt_without_loopback_none_is_given_no_map_gw_by_qemu_and_the_launcher() {
    let (_scratch, mut plan) = plan(&windows(), Boot::Gpu);
    plan.network.passt = true;
    plan.network.resolver = Some("127.0.0.53".into());
    plan.network.passt_age = PasstAge::NoLoopbackNone;
    let nic = value_after(&plan.qemu_arguments(), "-nic");
    assert!(nic.contains(",gateway=10.0.3.2,param=--no-map-gw,dns=10.0.3.3,"), "{nic}");
    assert!(nic.ends_with("dns-forward=10.0.3.3,dns-host=127.0.0.53") && !nic.contains("map-host-loopback"));
    let words = plan.passt_words();
    assert!(words.contains(&"--no-map-gw".to_owned()) && !words.contains(&"--map-host-loopback".to_owned()));
    assert!(words.windows(2).any(|two| two == ["--dns-host", "127.0.0.53"]), "{words:?}");
}

#[test]
fn passt_without_dns_host_tells_the_guest_the_hosts_resolver() {
    let (_scratch, mut plan) = plan(&windows(), Boot::Gpu);
    plan.network.passt = true;
    plan.network.resolver = Some("192.168.1.1".into());
    plan.network.passt_age = PasstAge::NoDnsHost;
    let nic = value_after(&plan.qemu_arguments(), "-nic");
    assert!(nic.ends_with(",gateway=10.0.3.2,param=--no-map-gw,dns=192.168.1.1"), "{nic}");
    let words = plan.passt_words();
    assert_eq!(&words[words.len() - 3..], ["--no-map-gw", "--dns", "192.168.1.1"]);
    assert!(!words.iter().any(|word| word.starts_with("--dns-")), "{words:?}");
}

#[test]
fn each_age_is_asked_with_its_own_words() {
    let current = passt_probe_words(PasstAge::Current);
    assert!(current.windows(2).any(|two| two == ["--map-host-loopback", "none"]), "{current:?}");
    assert!(current.contains(&"--dns-host".to_owned()));
    let middle = passt_probe_words(PasstAge::NoLoopbackNone);
    assert!(middle.contains(&"--no-map-gw".to_owned()) && middle.contains(&"--dns-host".to_owned()));
    let oldest = passt_probe_words(PasstAge::NoDnsHost);
    assert!(oldest.contains(&"--no-map-gw".to_owned()));
    assert!(!oldest.iter().any(|word| word == "--dns-host" || word == "--map-host-loopback"), "{oldest:?}");
}

#[test]
fn the_disk_is_on_nvme_only_for_a_machine_on_it_and_the_cds_stay_on_sata() {
    let (_scratch, on_nvme) = plan(&Machine { nvme: true, ..windows() }, Boot::Gpu);
    assert!(on_nvme.nvme, "the plan has it from the machine");
    let (_scratch, sata) = plan(&windows(), Boot::Gpu);
    // The same plan but for the disk, as two plans have their own folders.
    let nvme = VmPlan { nvme: true, ..sata.clone() };
    let (sata, nvme) = (sata.qemu_arguments(), nvme.qemu_arguments());
    assert!(has(&sata, "ide-hd,bus=ahci.0,drive=hd0,bootindex=1"));
    assert!(has(&nvme, "nvme,serial=guesthouse,drive=hd0,bootindex=1"));
    assert!(!nvme.iter().any(|word| word.starts_with("ide-hd")));
    assert!(has(&nvme, "ahci,id=ahci") && has(&nvme, "ide-cd,bus=ahci.2,drive=cd1"));
    let changed: Vec<_> = sata.iter().zip(&nvme).filter(|(old, new)| old != new).collect();
    assert_eq!(changed.len(), 1, "nothing else moves: {changed:?}");
}

#[test]
fn a_tpm_brings_its_firmware_and_hides_nested_virtualization() {
    let machine = Machine { tpm: true, ..windows() };
    let (_scratch, plan) = plan(&machine, Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert!(value_after(&arguments, "-cpu").ends_with(",-svm,-vmx"));
    assert!(arguments.iter().any(|word| word.starts_with("if=pflash,format=qcow2,unit=0")));
    assert!(has(&arguments, "tpm-crb,tpmdev=tpm0"));
    let swtpm = plan.swtpm_arguments().unwrap().join(" ");
    assert!(swtpm.contains("--tpm2") && swtpm.contains("/run/tpm.sock"));
}

#[test]
fn with_no_tpm_there_is_none_of_that() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert!(arguments.iter().any(|word| word.starts_with("if=pflash,format=raw,unit=0")));
    assert!(!value_after(&arguments, "-cpu").contains("-svm"));
    assert_eq!(plan.swtpm_arguments(), None);
}

#[test]
fn the_vga_memory_is_the_power_of_two_that_fits_the_screen() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    assert_eq!(plan.vga_memory_mib(), 8);
    let big = VmPlan { screen: "3840x2160".into(), ..plan };
    assert_eq!(big.vga_memory_mib(), 32);
}

#[test]
fn an_install_boots_its_disc_with_the_answers_beside_it() {
    let boot = Boot::Install { iso: "/iso/a,b.iso".into(), answer: "/m/answer.iso".into() };
    let (_scratch, plan) = plan(&Machine { installing: true, ..windows() }, boot);
    let arguments = plan.qemu_arguments();
    assert!(
        has(&arguments, "if=none,id=cd0,format=raw,media=cdrom,readonly=on,file=/iso/a,,b.iso"),
        "commas are doubled"
    );
    assert!(has(&arguments, "if=none,id=cd1,format=raw,media=cdrom,readonly=on,file=/m/answer.iso"));
    assert!(!has(&arguments, "usb-mouse,bus=xhci.0"), "an install has no GPU yet");
}

#[test]
fn a_comma_in_a_path_cannot_become_an_option() {
    let machine = Machine { disk: "/m,x/disk.qcow2".into(), vars: "/m,x/vars.fd".into(), ..windows() };
    let (_scratch, plan) = plan(&machine, Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert!(arguments.iter().any(|word| word.contains(r#""filename":"/m,x/disk.qcow2""#)));
    assert!(arguments.iter().any(|word| word.ends_with("file=/m,,x/vars.fd")));
}

#[test]
fn a_share_starts_our_program_with_the_folder_in_its_environment() {
    let machine = Machine { share: "/home/me/shared".into(), ..plain_machine() };
    let (_scratch, plan) = plan(&machine, plain_boot());
    let nic = value_after(&plan.qemu_arguments(), "-nic");
    assert!(nic.contains(",guestfwd=tcp:10.0.2.4:80-cmd:") && nic.contains("guesthouse-share"));
    assert_eq!(plan.qemu_environment(), [("SHARE".to_owned(), "/home/me/shared".to_owned())]);
}

#[test]
fn no_share_means_nothing_in_the_environment() {
    let (_scratch, plan) = plan(&plain_machine(), plain_boot());
    assert!(plan.qemu_environment().is_empty());
    assert_eq!(plan.share, None);
}

#[test]
fn pinning_runs_qemu_through_taskset() {
    let (_scratch, mut plan) = plan(&windows(), Boot::Gpu);
    plan.pin = Some("0-7,16-23".into());
    let command = plan.qemu_command();
    assert_eq!(&command[..3], ["taskset", "-c", "0-7,16-23"]);
    assert_eq!(command[3], plan.qemu.display().to_string());
}

#[test]
fn sound_is_only_asked_for_when_it_can_work() {
    let (_scratch, mut plan) = plan(&windows(), Boot::Gpu);
    assert_eq!(value_after(&plan.qemu_arguments(), "-audiodev"), "none,id=silent");
    assert!(has(&plan.qemu_arguments(), "hda-output,audiodev=silent"));
    plan.sound = Some(Sound { sink: Some("speakers".into()) });
    assert_eq!(value_after(&plan.qemu_arguments(), "-audiodev"), "pipewire,out.name=speakers,id=sound");
    assert!(has(&plan.qemu_arguments(), "hda-output,audiodev=sound"));
}

#[test]
fn a_vram_cap_is_given_in_the_daemons_words() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    assert_eq!(VmPlan::new(&context, &windows(), Boot::Gpu, slots::first(), 8).vram.as_deref(), Some("8G"));
    assert_eq!(VmPlan::new(&context, &windows(), Boot::Gpu, slots::first(), 0).vram, None);
}

fn with_drives(machine: Machine, drives: &[&str]) -> Machine {
    let drives = drives.iter().map(|id| Drive { id: (*id).to_owned() });
    Machine { drives: drives.collect(), ..machine }
}

#[test]
fn a_drive_is_handed_over_and_put_on_the_first_free_port() {
    let machine = with_drives(plain_machine(), &["ata-Disk_1"]);
    let (_scratch, plan) = plan(&machine, plain_boot());
    let arguments = plan.qemu_arguments();
    let at = arguments.iter().position(|word| word == "-add-fd").unwrap();
    assert_eq!(
        arguments[at..],
        [
            "-add-fd",
            "fd=40,set=40",
            "-blockdev",
            "driver=host_device,node-name=drive1,filename=/dev/fdset/40,cache.direct=on,discard=unmap",
            "-device",
            "ide-hd,bus=ahci.3,drive=drive1",
        ]
    );
}

#[test]
fn three_drives_have_their_own_numbers_and_ports() {
    let machine = with_drives(plain_machine(), &["a", "b", "c"]);
    let (_scratch, plan) = plan(&machine, plain_boot());
    let arguments = plan.qemu_arguments();
    for (number, port) in [(1, 3), (2, 4), (3, 5)] {
        assert!(has(&arguments, &format!("fd={0},set={0}", 39 + number)));
        assert!(has(&arguments, &format!("ide-hd,bus=ahci.{port},drive=drive{number}")));
    }
    let nodes: Vec<&String> = arguments.iter().filter(|word| word.contains("host_device")).collect();
    assert!(nodes.iter().all(|node| node.ends_with("cache.direct=on,discard=unmap")));
    assert!(nodes[2].contains("node-name=drive3,filename=/dev/fdset/42,"));
}

#[test]
fn drives_come_last_and_change_nothing_before_them() {
    let drives = with_drives(plain_machine(), &["a", "b"]).drives;
    for (machine, boot) in [(plain_machine(), plain_boot()), (windows(), Boot::Gpu)] {
        let (_scratch, plain) = plan(&machine, boot);
        let driven = VmPlan { drives: drives.clone(), ..plain.clone() };
        let (before, after) = (plain.qemu_arguments(), driven.qemu_arguments());
        assert_eq!(after[..before.len()], before[..]);
        assert_eq!(after.len(), before.len() + 12);
        assert!(!has(&before, "-add-fd") && !before.iter().any(|word| word.contains("fdset")));
    }
}

/// Windows setup would offer a host drive as a place to put Windows.
#[test]
fn an_install_and_ubuntu_are_given_no_drives() {
    let machine = with_drives(windows(), &["a"]);
    let install = Boot::Install { iso: String::new(), answer: "/m/answer.iso".into() };
    let (_scratch, installing) = plan(&machine, install);
    assert!(installing.drives.is_empty() && !has(&installing.qemu_arguments(), "-add-fd"));
    let (_scratch, unfinished) = plan(&Machine { installing: true, ..machine.clone() }, plain_boot());
    assert!(unfinished.drives.is_empty());
    let (_scratch, running) = plan(&machine, Boot::Gpu);
    assert_eq!(running.drives, machine.drives);
    let ubuntu = Machine { kind: Kind::Linux, ..machine };
    let (_scratch, ubuntu_plan) = plan(&ubuntu, Boot::Ubuntu(UbuntuBoot::NoGpu { reason_file: "/r".into() }));
    assert!(ubuntu_plan.drives.is_empty() && !has(&ubuntu_plan.qemu_arguments(), "-add-fd"));
}

#[test]
fn standalone_keeps_the_drives_and_opens_them_by_name() {
    let machine = with_drives(windows(), &["a", "b"]);
    let (_scratch, plan) = plan(&machine, Boot::Gpu);
    let files = memory_files_in(Path::new("/dev/shm"), slots::first());
    let plan = plan.standalone(files, Path::new("/ids"));
    assert_eq!(plan.drives, machine.drives);
    let arguments = plan.qemu_arguments();
    assert!(has(
        &arguments,
        "driver=host_device,node-name=drive2,filename=/ids/b,cache.direct=on,discard=unmap"
    ));
    assert!(!arguments.iter().any(|word| word == "-add-fd" || word.contains("fdset")));
}

#[test]
fn a_program_for_a_cleared_environment_is_given_by_where_path_finds_it() {
    let shell = program_word("sh");
    assert!(shell.starts_with('/') && shell.ends_with("/sh"), "{shell}");
    assert_eq!(program_word("no-such-program-here"), "no-such-program-here");
}

#[test]
fn the_first_card_has_the_machines_own_address_in_every_network_mode() {
    let mac = "52:54:00:a2:1c:6d";
    for machine in [windows(), plain_machine()] {
        for (mode, passt, bridge) in [
            (Mode::Nat, false, ""),
            (Mode::Nat, true, ""),
            (Mode::None, false, ""),
            (Mode::Bridged, false, "br0"),
        ] {
            let machine = Machine {
                network_mode: mode,
                passt,
                bridge: bridge.into(),
                mac: mac.into(),
                ..machine.clone()
            };
            let boot = if machine.kind == Kind::Plain { plain_boot() } else { Boot::Gpu };
            let (_scratch, plan) = plan(&machine, boot);
            let card = value_after(&plan.qemu_arguments(), "-nic");
            assert!(card.contains(&format!(",mac={mac},")), "{mode:?} passt {passt}: {card}");
        }
    }
}

fn with_app_windows(context: &Context, machine: &Machine, on: bool) {
    fs::create_dir_all(machine.folder(context)).unwrap();
    crate::app_windows::set_on(context, machine, on).unwrap();
}

#[test]
fn app_windows_on_adds_one_forward_for_its_helper_after_the_files_helpers() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    with_app_windows(&context, &windows(), true);
    let plan = VmPlan::new(&context, &windows(), Boot::Gpu, slots::first(), 0);
    assert_eq!(plan.app_windows_port, Some(18965));
    assert_eq!(
        value_after(&plan.qemu_arguments(), "-nic"),
        "user,model=e1000e,hostfwd=tcp:127.0.0.1:18765-:8765,hostfwd=tcp:127.0.0.1:18865-:8767,\
         hostfwd=tcp:127.0.0.1:18965-:8769,hostfwd=udp:127.0.0.1:19065-:7791"
    );
    let second = VmPlan::new(&context, &windows(), Boot::Gpu, slots::number(2), 0);
    assert!(
        value_after(&second.qemu_arguments(), "-nic")
            .ends_with("hostfwd=tcp:127.0.0.1:18967-:8769,hostfwd=udp:127.0.0.1:19067-:7791")
    );
}

#[test]
fn with_app_windows_off_or_never_set_qemu_gets_the_words_it_always_did() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = windows();
    let never = VmPlan::new(&context, &machine, Boot::Gpu, slots::first(), 0);
    with_app_windows(&context, &machine, true);
    with_app_windows(&context, &machine, false);
    let off = VmPlan::new(&context, &machine, Boot::Gpu, slots::first(), 0);
    assert_eq!(never.qemu_arguments(), off.qemu_arguments());
    assert_eq!((never.app_windows_port, off.app_windows_port), (None, None));
    assert!(!never.qemu_arguments().join(" ").contains("8769"));
}

#[test]
fn only_a_windows_machine_can_have_the_forward_whatever_a_file_says() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = plain_machine();
    fs::create_dir_all(machine.folder(&context)).unwrap();
    fs::write(machine.folder(&context).join(crate::app_windows::SWITCH_FILE), "{\"on\":true}").unwrap();
    assert_eq!(VmPlan::new(&context, &machine, plain_boot(), slots::first(), 0).app_windows_port, None);
}

#[test]
fn every_windows_machine_forwards_remotemans_port_to_its_slots_loopback_port_once() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let machine = Machine { forwards: vec![Forward::new(Protocol::Udp, 7791, 7791)], ..windows() };
    let plan = VmPlan::new(&context, &machine, Boot::Gpu, slots::number(2), 0);
    assert_eq!(plan.remote_view_port, Some(19067));
    let nic = value_after(&plan.qemu_arguments(), "-nic");
    assert_eq!(nic.matches("-:7791").count(), 1, "QEMU answers from the first forward to a port: {nic}");
    assert!(nic.contains("hostfwd=udp:127.0.0.1:19067-:7791"));
    assert!(!nic.contains(":7791-"), "nothing on this PC's own 7791, which remoteman's host may hold");
}

#[test]
fn a_machine_that_is_not_windows_has_no_remote_view_forward() {
    let scratch = Scratch::new();
    let context = scratch_context(&scratch);
    let plan = VmPlan::new(&context, &plain_machine(), plain_boot(), slots::first(), 0);
    assert_eq!(plan.remote_view_port, None);
    assert!(!plan.qemu_arguments().join(" ").contains("7791"));
}

fn disk_drive(plan: &VmPlan) -> String {
    plan.qemu_arguments().into_iter().find(|word| word.contains(r#""node-name":"hd0""#)).unwrap()
}

/// A machine in use keeps the guest's flushes, so a power cut of the host can't leave NTFS or the
/// qcow2 damaged. Only a disk nothing is kept from drops them: Windows setup, and a throwaway run.
#[test]
fn only_a_disk_nothing_is_kept_from_drops_the_guests_flushes() {
    for (machine, boot) in [(windows(), Boot::Gpu), (plain_machine(), plain_boot())] {
        let (_scratch, plan) = plan(&machine, boot);
        assert!(
            disk_drive(&plan).contains(r#""cache":{"direct":false,"no-flush":false}"#),
            "{}",
            disk_drive(&plan)
        );
    }
    let install = Boot::Install { iso: "/iso/w.iso".into(), answer: "/m/answer.iso".into() };
    let (_scratch, setup) = plan(&Machine { installing: true, ..windows() }, install);
    assert!(
        disk_drive(&setup).contains(r#""cache":{"direct":false,"no-flush":true}"#),
        "{}",
        disk_drive(&setup)
    );
    for (machine, boot) in [(windows(), Boot::Gpu), (plain_machine(), plain_boot())] {
        let (_scratch, plan) = plan(&Machine { throwaway: true, ..machine.clone() }, boot);
        let run = VmPlan { throwaway: Some(machine.disk.clone()), disk: throwaway_disk(), ..plan };
        assert!(
            disk_drive(&run).contains(r#""cache":{"direct":false,"no-flush":true}"#)
                && disk_drive(&run).contains(r#""filename":"/dev/fdset/50""#),
            "{}",
            disk_drive(&run)
        );
    }
}

/// QEMU guesses a disc's format from its first bytes unless told, and a disc is read as it is, so
/// every drive with a disc in it is raw. An empty tray has nothing to name a format for.
#[test]
fn every_disc_is_read_as_raw() {
    let install = UbuntuInstall {
        iso: "/iso/ubuntu.iso".into(),
        seed: "/m/seed/cidata.iso".into(),
        kernel: "/m/boot/vmlinuz".into(),
        initrd: "/m/boot/initrd".into(),
    };
    let boots = [
        (plain_machine(), Boot::Plain { iso: "/iso/plain.iso".into() }),
        (windows(), Boot::Install { iso: "/iso/win.iso".into(), answer: "/m/answer.iso".into() }),
        (windows(), Boot::Gpu),
        (windows(), Boot::UbuntuInstall(install)),
    ];
    let mut discs = 0;
    for (machine, boot) in boots {
        let (_scratch, mut plan) = plan(&machine, boot);
        plan.driver_disc = Some("/m/driver.iso".into());
        for word in plan.qemu_arguments().iter().filter(|word| word.contains("media=cdrom")) {
            if word.contains("file=") {
                discs += 1;
                assert!(word.split(',').any(|part| part == "format=raw"), "{word}");
            } else {
                assert!(!word.contains("format="), "{word}");
            }
        }
    }
    assert_eq!(discs, 6, "the plain disc, the install's two, the driver disc, Ubuntu's two");
}

/// QEMU is given the files a disk is built on, as the settings name them, and none after the last,
/// so it never takes a name from the disk's header.
#[test]
fn the_disk_is_given_with_the_backing_files_its_settings_name() {
    use crate::disk_image::backing::BackingFile;
    let base = BackingFile { path: "/b/base.qcow2".into(), format: "qcow2".into() };
    let node_of = |arguments: &[String], disk: &str| -> serde_json::Value {
        let at = arguments.iter().position(|word| word.starts_with('{') && word.contains(disk));
        let at = at.unwrap_or_else(|| panic!("no -blockdev for {disk}: {arguments:?}"));
        assert_eq!(arguments[at - 1], "-blockdev");
        serde_json::from_str(&arguments[at]).unwrap()
    };
    let (_scratch, plain) = plan(&plain_machine(), plain_boot());
    let node = node_of(&plain.qemu_arguments(), "/m/disk.qcow2");
    assert_eq!(node["node-name"], "hd0");
    assert_eq!(node["backing"], serde_json::Value::Null);
    let layered = Machine { backing_files: vec![base], ..windows() };
    let (_scratch, gpu) = plan(&layered, Boot::Gpu);
    let node = node_of(&gpu.qemu_arguments(), "/m/disk.qcow2");
    assert_eq!(node["backing"]["file"]["filename"], "/b/base.qcow2");
    assert_eq!(node["backing"]["backing"], serde_json::Value::Null);
    // A throwaway run's overlay is on the machine's disk, and that on its base.
    let run = VmPlan { throwaway: Some("/m/disk.qcow2".into()), disk: throwaway_disk(), ..gpu };
    let node = node_of(&run.qemu_arguments(), "/dev/fdset/50");
    assert_eq!(node["backing"]["file"]["filename"], "/m/disk.qcow2");
    assert_eq!(node["backing"]["backing"]["file"]["filename"], "/b/base.qcow2");
    assert_eq!(node["cache"]["no-flush"], true);
}

/// Every kind of boot gives QEMU its files by the rules in `drive_rules`.
#[test]
fn every_boot_gives_its_files_by_the_drive_rules() {
    use crate::linux::amd::{AmdCard, AmdMode};
    let install = UbuntuInstall {
        iso: "/iso/ubuntu.iso".into(),
        seed: "/m/seed/cidata.iso".into(),
        kernel: "/m/boot/vmlinuz".into(),
        initrd: "/m/boot/initrd".into(),
    };
    let amd = AmdBoot {
        card: AmdCard {
            pci: "0000:7c:00.0".into(),
            render_node: "/dev/dri/renderD128".into(),
            name: "AMD".into(),
            device: 0x13C0,
        },
        mode: AmdMode::Native,
        setup_image: Some("/m/setup.img".into()),
    };
    let layered = Machine {
        backing_files: vec![crate::disk_image::backing::BackingFile {
            path: "/b/base.qcow2".into(),
            format: "qcow2".into(),
        }],
        ..windows()
    };
    let boots = [
        (plain_machine(), plain_boot()),
        (plain_machine(), Boot::Plain { iso: "/iso/plain.iso".into() }),
        (windows(), Boot::Install { iso: "/iso/win.iso".into(), answer: "/m/answer.iso".into() }),
        (windows(), Boot::Gpu),
        (layered, Boot::Gpu),
        (with_drives(windows(), &["a", "b"]), Boot::Gpu),
        (with_drives(plain_machine(), &["a"]), plain_boot()),
        (windows(), Boot::UbuntuInstall(install)),
        (windows(), Boot::Ubuntu(UbuntuBoot::NoGpu { reason_file: "/r/gpu-off-reason".into() })),
        (windows(), Boot::Ubuntu(UbuntuBoot::Amd(amd))),
    ];
    for (machine, boot) in boots {
        let (_scratch, mut plan) = plan(&machine, boot);
        plan.driver_disc = Some("/m/driver.iso".into());
        drive_rules::check(&plan.qemu_arguments());
        let files = memory_files_in(Path::new("/dev/shm"), slots::first());
        drive_rules::check(&plan.clone().standalone(files, Path::new("/ids")).qemu_arguments());
        let run = VmPlan { throwaway: Some(machine.disk.clone()), disk: throwaway_disk(), ..plan };
        drive_rules::check(&run.qemu_arguments());
    }
}
