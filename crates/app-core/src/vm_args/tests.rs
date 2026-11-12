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
    .standalone(files);
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

#[test]
fn nothing_set_is_the_line_every_machine_had() {
    let (_scratch, plan) = plan(&windows(), Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert_eq!(value_after(&arguments, "-nic"), "user,model=e1000e,hostfwd=tcp:127.0.0.1:18765-:8765");
    assert!(!has(&arguments, "-netdev"), "NAT has one adapter");
}

#[test]
fn no_internet_keeps_the_agent() {
    let machine = Machine { network_mode: Mode::None, ..windows() };
    let (_scratch, plan) = plan(&machine, Boot::Gpu);
    let nic = value_after(&plan.qemu_arguments(), "-nic");
    assert!(nic.starts_with("user,model=e1000e,restrict=on,hostfwd=tcp:127.0.0.1:18765-:8765"));
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
    assert!(value_after(&arguments, "-netdev").ends_with("-:8765,hostfwd=udp:127.0.0.1:5000-:50"));
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
        has(&arguments, "if=none,id=cd0,media=cdrom,readonly=on,file=/iso/a,,b.iso"),
        "commas are doubled"
    );
    assert!(has(&arguments, "if=none,id=cd1,media=cdrom,readonly=on,file=/m/answer.iso"));
    assert!(!has(&arguments, "usb-mouse,bus=xhci.0"), "an install has no GPU yet");
}

#[test]
fn a_comma_in_a_path_cannot_become_an_option() {
    let machine = Machine { disk: "/m,x/disk.qcow2".into(), vars: "/m,x/vars.fd".into(), ..windows() };
    let (_scratch, plan) = plan(&machine, Boot::Gpu);
    let arguments = plan.qemu_arguments();
    assert!(arguments.iter().any(|word| word.contains("file=/m,,x/disk.qcow2,format=qcow2")));
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
