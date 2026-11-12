//! Walks the app's pages on a scratch store, and saves a picture of each. It's for checking pages by
//! eye on a backend that draws in software: see `crates/app/page-shots.sh`. Each check prints PASS or FAIL.
//!
//! Usage: `page_shots FOLDER`, with the store and the XDG folders already pointing somewhere harmless.

mod fake_display;
mod fake_forwarded;
mod fake_frames;
mod fake_qemu;

use adw::prelude::*;
use gtk::glib;
use guesthouse_app::application::application;
use guesthouse_app::window::{MachineHandle, Window};
use guesthouse_app_core::context::Context;
use guesthouse_app_core::library::{NewMachine, create};
use guesthouse_app_core::machine::{Drive, Kind, Machine};
use std::cell::Cell;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

const SETTLE: Duration = Duration::from_millis(900);
const DIALOGS_AT_MOST: u32 = 2;
const LONG: Duration = Duration::from_millis(2500);
/// How many times `SETTLE` a page gets to show what it should.
const PATIENCE: u32 = 20;
const PRETEND_VM_LIFE: Duration = Duration::from_secs(60);

fn check(failures: &Cell<u32>, good: bool, text: &str) {
    println!("{} {text}", if good { "PASS" } else { "FAIL" });
    if !good {
        failures.set(failures.get() + 1);
    }
}

fn walk(root: &gtk::Widget, found: &mut Vec<gtk::Widget>) {
    found.push(root.clone());
    let mut child = root.first_child();
    while let Some(widget) = child {
        walk(&widget, found);
        child = widget.next_sibling();
    }
}

fn all_of<T: IsA<gtk::Widget>>(window: &Window) -> Vec<T> {
    let mut found = Vec::new();
    walk(window.widget().upcast_ref(), &mut found);
    found.into_iter().filter_map(|widget| widget.downcast::<T>().ok()).collect()
}

fn click(window: &Window, label: &str) -> bool {
    let buttons = all_of::<gtk::Button>(window);
    let button =
        buttons.iter().rev().find(|button| button.label().as_deref() == Some(label) && button.is_mapped());
    if let Some(button) = button {
        button.emit_clicked();
    }
    button.is_some()
}

/// The first machine's button, where `click` takes the newest dialog's.
fn click_first(window: &Window, label: &str) -> bool {
    let buttons = all_of::<gtk::Button>(window);
    let button = buttons.iter().find(|button| button.label().as_deref() == Some(label) && button.is_mapped());
    if let Some(button) = button {
        button.emit_clicked();
    }
    button.is_some()
}

fn text_of(window: &Window) -> String {
    let labels = all_of::<gtk::Label>(window);
    labels
        .iter()
        .filter(|label| label.is_mapped())
        .map(|label| label.label().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn shot(window: &Window, folder: &Path, name: &str) {
    let widget = window.widget();
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(widget.width()), f64::from(widget.height()));
    let Some(node) = snapshot.to_node() else { return };
    let Some(renderer) = widget.renderer() else { return };
    let texture = renderer.render_texture(node, None);
    let path = folder.join(format!("{name}.png"));
    println!("shot {name}: {}", texture.save_to_png(&path).is_ok());
}

/// Takes the picture right after a paint, as only then is nothing waiting for layout, and a widget
/// with layout to do draws nothing.
/// Whether the text comes up, for a page that takes its time on a busy PC.
async fn shows(window: &Window, text: &str) -> bool {
    for _ in 0..PATIENCE {
        if text_of(window).contains(text) {
            return true;
        }
        glib::timeout_future(SETTLE).await;
    }
    false
}

async fn shot_after_paint(window: &Window, folder: &Path, name: &str) {
    let Some(clock) = window.widget().frame_clock() else { return };
    let painted = window.widget().clone();
    let (taken, wait) = async_channel::bounded::<()>(1);
    let (window, folder, name) = (window.clone(), folder.to_owned(), name.to_owned());
    let handler = clock.connect_after_paint(move |_| {
        if !taken.is_full() {
            shot(&window, &folder, &name);
            let _ = taken.try_send(());
        }
    });
    painted.queue_draw();
    let _ = wait.recv().await;
    clock.disconnect(handler);
}

/// Turns the settings to one of their pages.
fn show_page(window: &Window, name: &str) {
    if let Some(dialog) = all_of::<adw::PreferencesDialog>(window).last() {
        dialog.set_visible_page_name(name);
    }
}

/// Scrolls the topmost scrolled area until this title is near the top.
fn scroll_to(window: &Window, title: &str) {
    let groups = all_of::<adw::PreferencesGroup>(window);
    let Some(group) = groups.iter().find(|group| group.title() == title) else { return };
    let mut parent = group.parent();
    while let Some(widget) = parent {
        if let Some(area) = widget.downcast_ref::<gtk::ScrolledWindow>() {
            if let Some(bounds) = group.compute_bounds(area) {
                let adjustment = area.vadjustment();
                adjustment.set_value(adjustment.value() + f64::from(bounds.y()) - 12.0);
            }
            return;
        }
        parent = widget.parent();
    }
}

/// Closes every dialog. One that is closing still counts as visible until its animation ends.
async fn close_dialogs(window: &Window) {
    for _ in 0..DIALOGS_AT_MOST {
        if let Some(dialog) = window.widget().visible_dialog() {
            dialog.force_close();
        }
        glib::timeout_future(SETTLE).await;
    }
}

fn make_iso(folder: &Path) -> String {
    let tree = folder.join("tree");
    fs::create_dir_all(tree.join("boot")).unwrap();
    fs::write(tree.join("boot/grub.cfg"), "linux").unwrap();
    let iso = folder.join("tiny-linux.iso");
    let made = Command::new("genisoimage").args(["-quiet", "-J", "-r", "-o"]).arg(&iso).arg(&tree).status();
    assert!(made.unwrap().success());
    iso.display().to_string()
}

async fn wizard_pages(window: Window, folder: PathBuf, failures: Rc<Cell<u32>>) {
    glib::timeout_future(LONG).await;
    check(&failures, text_of(&window).contains("Check This PC"), "setup opens by itself on a new store");
    shot_after_paint(&window, &folder, "01-wizard-check").await;
    click(&window, "Next");
    glib::timeout_future(SETTLE).await;
    check(&failures, text_of(&window).contains("617.42"), "the driver page names the driver");
    shot_after_paint(&window, &folder, "02-wizard-driver").await;
    close_dialogs(&window).await;
}

async fn new_machine(window: Window, folder: PathBuf, failures: Rc<Cell<u32>>) {
    check(&failures, text_of(&window).contains("No Machines"), "an empty library says so");
    shot_after_paint(&window, &folder, "04-library-empty").await;
    window.show_new_machine();
    glib::timeout_future(SETTLE).await;
    let iso = make_iso(&folder);
    window.open_new_machine().unwrap().read_iso(iso);
    glib::timeout_future(LONG).await;
    check(&failures, text_of(&window).contains("a plain VM"), "an ISO that isn't Windows makes a plain VM");
    shot_after_paint(&window, &folder, "05-new-machine").await;
    close_dialogs(&window).await;
}

const FREE_DRIVE: &str = "ata-WDC_WD5000_WD-1234";
const SEAGATE: &str = "usb-Seagate_Expansion_NA9-0:0";
const STICK: &str = "usb-SanDisk_Ultra_4C53-0:0";

/// A disk of the pretend PC: the kernel's folder for it, a file for its node, and its name.
fn pretend_disk(pc: &Path, name: &str, id: &str, (dev, sectors): (&str, u64), model: &str, removable: bool) {
    let write = |path: String, text: String| {
        let path = pc.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write(format!("block/{name}/dev"), format!("{dev}\n"));
    write(format!("block/{name}/size"), format!("{sectors}\n"));
    write(format!("block/{name}/removable"), format!("{}\n", u8::from(removable)));
    write(format!("block/{name}/device/model"), format!("{model}\n"));
    link_name(pc, name, id);
}

fn pretend_partition(pc: &Path, disk: &str, name: &str, id: &str, (dev, sectors): (&str, u64)) {
    let folder = pc.join(format!("block/{disk}/{name}"));
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("partition"), "1\n").unwrap();
    fs::write(folder.join("dev"), format!("{dev}\n")).unwrap();
    fs::write(folder.join("size"), format!("{sectors}\n")).unwrap();
    link_name(pc, name, id);
}

fn link_name(pc: &Path, kernel_name: &str, id: &str) {
    let node = pc.join("dev").join(kernel_name);
    fs::create_dir_all(node.parent().unwrap()).unwrap();
    fs::write(&node, "").unwrap();
    fs::create_dir_all(pc.join("by-id")).unwrap();
    std::os::unix::fs::symlink(node, pc.join("by-id").join(id)).unwrap();
}

/// Five disks: the one this PC runs from, a free one, a USB disk, a stick whose partition is mounted,
/// and one nothing has.
fn pretend_pc(context: &mut Context, pc: &Path) {
    pretend_disk(pc, "sda", "ata-Samsung_SSD_870_S5", ("8:0", 1_953_525_168), "Samsung SSD 870", false);
    pretend_partition(pc, "sda", "sda1", "ata-Samsung_SSD_870_S5-part1", ("8:1", 2_097_152));
    pretend_partition(pc, "sda", "sda2", "ata-Samsung_SSD_870_S5-part2", ("8:2", 1_951_000_000));
    pretend_disk(pc, "sdb", FREE_DRIVE, ("8:16", 976_773_168), "WDC WD5000", false);
    pretend_disk(pc, "sdc", SEAGATE, ("8:32", 3_907_029_168), "Seagate Expansion", false);
    pretend_disk(pc, "sdd", STICK, ("8:48", 62_533_296), "SanDisk Ultra", true);
    pretend_partition(pc, "sdd", "sdd1", "usb-SanDisk_Ultra_4C53-0:0-part1", ("8:49", 62_531_000));
    pretend_disk(pc, "sde", "ata-CT500MX500SSD1_1949", ("8:64", 976_773_168), "CT500MX500SSD1", false);
    fs::write(
        pc.join("mountinfo"),
        "22 1 8:1 / /boot/efi rw - vfat /dev/sda1 rw\n23 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
         40 1 8:49 / /run/media/me/STICK rw - exfat /dev/sdd1 rw\n",
    )
    .unwrap();
    // A user that isn't root can't open it, and gets the way to.
    fs::set_permissions(pc.join("dev/sdb"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::create_dir_all(pc.join("udev")).unwrap();
    fs::write(
        pc.join("udev/b8:16"),
        "E:ID_SERIAL=WDC_WD5000_WD-1234
",
    )
    .unwrap();
    context.block_devices = pc.join("block");
    context.drive_ids = pc.join("by-id");
    context.mount_table = pc.join("mountinfo");
    context.swaps = pc.join("swaps");
    context.udev_data = pc.join("udev");
}

fn library_machines(context: &Arc<Context>, window: &Window, folder: &Path) -> Vec<MachineHandle> {
    let iso = make_iso(folder);
    let new = NewMachine {
        name: "Tiny Linux".into(),
        iso,
        kind: Kind::Plain,
        cores: 2,
        ram_gib: 2,
        disk_gib: 4,
        user: String::new(),
        password: String::new(),
        amd_gpu: None,
    };
    let plain = create(context, &new).unwrap();
    let disk = folder.join("win.qcow2").display().to_string();
    fs::copy(&plain.disk, &disk).unwrap();
    let drive = |id: &str| Drive { id: id.to_owned() };
    let drives = vec![drive(FREE_DRIVE), drive(SEAGATE), drive(STICK)];
    let windows =
        Machine { refresh_hz: 165, drives, ..Machine::new("win", "Windows 11", &disk, &plain.vars) };
    windows.save(context).unwrap();
    vec![window.add_machine(plain), window.add_machine(windows)]
}

async fn settings_pages(window: Window, folder: PathBuf, failures: Rc<Cell<u32>>, machine: MachineHandle) {
    window.show_settings(machine);
    glib::timeout_future(LONG).await;
    check(&failures, shows(&window, "Hardware").await, "settings open");
    shot_after_paint(&window, &folder, "07-settings-hardware").await;
    show_page(&window, "display");
    glib::timeout_future(SETTLE).await;
    check(&failures, text_of(&window).contains("Viewer"), "the display page has the viewer's settings");
    shot_after_paint(&window, &folder, "07b-settings-display").await;
    show_page(&window, "storage");
    glib::timeout_future(SETTLE).await;
    check(&failures, !text_of(&window).contains("Cores"), "one page at a time");
    check(&failures, shows(&window, "WDC WD5000, 500 GB").await, "a drive is shown by make and size");
    let text = text_of(&window);
    check(
        &failures,
        text.contains("In use, as sdd1 is mounted at /run/media/me/STICK"),
        "a disk with a partition the PC has mounted says which and where",
    );
    check(&failures, text.contains("Seagate Expansion, 2.0 TB"), "a disk is shown by make and size");
    check(&failures, !text.contains("Partition"), "no partition is given");
    shot_after_paint(&window, &folder, "08-settings-disk-snapshots").await;
    let add = all_of::<gtk::Button>(&window).into_iter().find(|b| b.label().as_deref() == Some("Add Drive"));
    check(&failures, add.is_some_and(|button| !button.is_sensitive()), "three drives are the most");
    // Only for a user who isn't root, as root opens anything.
    if click(&window, "Give Access") {
        glib::timeout_future(LONG).await;
        check(&failures, window.widget().visible_dialog().is_some(), "the root step opens");
        glib::timeout_future(SETTLE).await;
        shot_after_paint(&window, &folder, "08d-give-access").await;
        click(&window, "Close");
        glib::timeout_future(SETTLE).await;
    }
    // Takes the mounted one away, to have room for another.
    let removes: Vec<gtk::Button> = all_of::<gtk::Button>(&window)
        .into_iter()
        .filter(|button| button.tooltip_text().as_deref() == Some("Remove Drive"))
        .collect();
    removes.last().unwrap().emit_clicked();
    glib::timeout_future(LONG).await;
    check(&failures, click(&window, "Add Drive"), "Add Drive can be pressed");
    check(&failures, shows(&window, "Add a Drive").await, "the list opens");
    glib::timeout_future(SETTLE).await;
    shot_after_paint(&window, &folder, "08c-add-drive").await;
    if let Some(dialog) = window.widget().visible_dialog() {
        dialog.force_close();
    }
    glib::timeout_future(SETTLE).await;
    show_page(&window, "devices");
    glib::timeout_future(SETTLE).await;
    shot_after_paint(&window, &folder, "08b-settings-devices").await;
    show_page(&window, "network");
    glib::timeout_future(SETTLE).await;
    shot_after_paint(&window, &folder, "09-settings-network-nat").await;
    let combos = all_of::<adw::ComboRow>(&window);
    let mode = combos.iter().find(|row| row.title() == "Mode").unwrap();
    mode.set_selected(2);
    glib::timeout_future(SETTLE).await;
    check(&failures, text_of(&window).contains("Bridge status"), "Bridged shows the bridge's status");
    shot_after_paint(&window, &folder, "10-settings-network-bridged").await;
    click(&window, "Show What to Do");
    glib::timeout_future(LONG).await;
    shot_after_paint(&window, &folder, "11-bridge-root-step").await;
    check(&failures, click(&window, "Close"), "the root step can be closed");
    glib::timeout_future(SETTLE).await;
    mode.set_selected(0);
    click(&window, "Add");
    glib::timeout_future(SETTLE).await;
    let entries = all_of::<adw::EntryRow>(&window);
    let host = entries.iter().find(|row| row.title() == "Host port").unwrap();
    let guest = entries.iter().find(|row| row.title() == "Guest port").unwrap();
    host.set_text("80");
    guest.set_text("80");
    glib::timeout_future(SETTLE).await;
    check(&failures, text_of(&window).contains("need root"), "a low port is refused as it's typed");
    shot_after_paint(&window, &folder, "12-forward-refused").await;
    host.set_text("28080");
    glib::timeout_future(SETTLE).await;
    click(&window, "Add");
    glib::timeout_future(SETTLE).await;
    check(&failures, text_of(&window).contains("28080"), "the forward is listed");
    scroll_to(&window, "Port Forwards");
    glib::timeout_future(SETTLE).await;
    shot_after_paint(&window, &folder, "13-forward-listed").await;
    close_dialogs(&window).await;
}

/// A process that passes for this machine's QEMU, which is this program under another name. It only
/// sleeps. The window reads `/proc`, so this is how a page sees a running VM with no VM.
fn pretend_vm(context: &Context, machine: &Machine) -> Child {
    let socket = format!("unix:{}/qmp.sock,server=on", context.run_folder(None).display());
    let drive = format!("file={},format=qcow2", machine.disk);
    Command::new(env::current_exe().unwrap())
        .arg0(context.paths.qemu.as_os_str())
        .args(["-qmp", &socket, "-drive", &drive])
        .spawn()
        .unwrap()
}

async fn running_pages(window: Window, folder: PathBuf, failures: Rc<Cell<u32>>, machine: MachineHandle) {
    glib::timeout_future(LONG).await;
    check(&failures, text_of(&window).contains("Running."), "a running VM shows in the library");
    shot_after_paint(&window, &folder, "14-library-running").await;
    window.show_settings(machine);
    check(&failures, shows(&window, "The VM must be off").await, "settings say what can't change now");
    shot_after_paint(&window, &folder, "20-settings-while-running").await;
    close_dialogs(&window).await;
}

async fn viewer_page(
    window: Window,
    folder: PathBuf,
    failures: Rc<Cell<u32>>,
    machine: MachineHandle,
    released: Arc<AtomicU64>,
) {
    window.show_screen(machine);
    glib::timeout_future(LONG).await;
    let page = window.open_screen().unwrap();
    let before = page.frames_shown();
    glib::timeout_future(LONG).await;
    let after = page.frames_shown();
    check(&failures, after > before, &format!("frames keep coming: {before} then {after}"));
    let came_back = released.load(Ordering::Relaxed);
    check(&failures, came_back > 0, &format!("slots come back when GTK is done: {came_back}"));
    shot_after_paint(&window, &folder, "15-viewer-stand-in").await;
    page.toggle_overlay();
    glib::timeout_future(LONG).await;
    check(&failures, text_of(&window).contains("Frames a second:"), "the overlay shows frames counted");
    check(
        &failures,
        text_of(&window).contains("Host GPU: not available"),
        "with no reader the host's figures say so",
    );
    shot_after_paint(&window, &folder, "16-viewer-overlay").await;
    page.toggle_overlay();
    page.toggle_capture();
    glib::timeout_future(SETTLE).await;
    check(&failures, text_of(&window).contains("cannot lock the pointer"), "a desktop with no lock says so");
    shot_after_paint(&window, &folder, "17-viewer-no-capture").await;
    page.toggle_fullscreen();
    glib::timeout_future(SETTLE).await;
    shot_after_paint(&window, &folder, "18-viewer-fullscreen").await;
    page.toggle_fullscreen();
    glib::timeout_future(SETTLE).await;
    window.pages().pop_to_tag("library");
    glib::timeout_future(LONG).await;
    check(&failures, window.open_screen().is_none(), "leaving the page closes it");
}

/// An Ubuntu machine: its settings say its GPU is a preview, and its own frames reach the viewer.
async fn ubuntu_pages(window: Window, context: Arc<Context>, folder: PathBuf, failures: Rc<Cell<u32>>) {
    let disk = folder.join("ubuntu.qcow2").display().to_string();
    fs::copy(folder.join("win.qcow2"), &disk).unwrap();
    let ubuntu = Machine { kind: Kind::Linux, ..Machine::new("ubuntu", "Ubuntu", &disk, "") };
    ubuntu.save(&context).unwrap();
    let machine = window.add_machine(ubuntu.clone());
    glib::timeout_future(LONG).await;
    check(&failures, text_of(&window).contains("GPU preview"), "the library marks Ubuntu's GPU as a preview");
    window.show_settings(machine.clone());
    check(&failures, shows(&window, "This is a preview").await, "its settings say so plainly");
    check(&failures, !text_of(&window).contains("Port Forwards"), "what it doesn't have isn't shown");
    scroll_to(&window, "Ubuntu");
    shot_after_paint(&window, &folder, "24-ubuntu-settings").await;
    close_dialogs(&window).await;
    fake_forwarded::serve(&context.run_folder(None));
    let mut vm = pretend_vm(&context, &ubuntu);
    glib::timeout_future(LONG).await;
    window.show_screen(machine);
    glib::timeout_future(LONG).await;
    glib::timeout_future(LONG).await;
    let page = window.open_screen().unwrap();
    let before = page.frames_shown();
    glib::timeout_future(LONG).await;
    let after = page.frames_shown();
    check(
        &failures,
        after > before && before > 0,
        &format!("the guest's own frames show: {before} then {after}"),
    );
    shot_after_paint(&window, &folder, "25-ubuntu-frames").await;
    window.pages().pop_to_tag("library");
    let _ = vm.kill();
    let _ = vm.wait();
    glib::timeout_future(LONG).await;
}

/// What the owner will do: start a machine, look at it, and stop it, through the same buttons. A plain
/// machine shows QEMU's display, as firmware and an install do.
async fn whole_flow(window: Window, folder: PathBuf, failures: Rc<Cell<u32>>) {
    glib::timeout_future(LONG).await;
    check(&failures, click_first(&window, "Start"), "Start can be pressed");
    glib::timeout_future(LONG).await;
    glib::timeout_future(LONG).await;
    check(&failures, text_of(&window).contains("Running."), "the machine runs");
    shot_after_paint(&window, &folder, "21-started").await;
    check(&failures, click(&window, "Show Screen"), "Show Screen can be pressed");
    glib::timeout_future(LONG).await;
    glib::timeout_future(LONG).await;
    let page = window.open_screen().unwrap();
    let before = page.frames_shown();
    glib::timeout_future(LONG).await;
    let after = page.frames_shown();
    check(&failures, after > before && before > 0, &format!("QEMU's display shows: {before} then {after}"));
    check(&failures, page.has_input(), "the guest takes input");
    shot_after_paint(&window, &folder, "22-viewer-qemu-display").await;
    window.pages().pop_to_tag("library");
    glib::timeout_future(LONG).await;
    check(&failures, click(&window, "Shut Down"), "Shut Down can be pressed");
    glib::timeout_future(LONG).await;
    glib::timeout_future(LONG).await;
    let off = text_of(&window);
    check(&failures, off.matches("Off.").count() == 3, "every machine is off again");
    shot_after_paint(&window, &folder, "23-stopped").await;
}

/// A pretend install script says there's a newer release, and the library offers it.
async fn update_banner(window: Window, context: Arc<Context>, folder: PathBuf, failures: Rc<Cell<u32>>) {
    let script = context.paths.helper("install.sh");
    fs::write(&script, "echo update-available 0.3.0 9.9.9\nexit 10\n").unwrap();
    window.check_for_updates();
    check(&failures, shows(&window, "Guesthouse 9.9.9 is out").await, "a newer release gets a banner");
    shot_after_paint(&window, &folder, "06b-update-banner").await;
    fs::remove_file(&script).unwrap();
}

async fn walk_pages(window: Window, context: Arc<Context>, folder: PathBuf) {
    let failures = Rc::new(Cell::new(0));
    wizard_pages(window.clone(), folder.clone(), failures.clone()).await;
    new_machine(window.clone(), folder.clone(), failures.clone()).await;
    let machines = library_machines(&context, &window, &folder);
    glib::timeout_future(LONG).await;
    check(&failures, text_of(&window).contains("Plain VM"), "the library lists the machines");
    shot_after_paint(&window, &folder, "06-library").await;
    update_banner(window.clone(), context.clone(), folder.clone(), failures.clone()).await;
    settings_pages(window.clone(), folder.clone(), failures.clone(), machines[1].clone()).await;
    window.show_log();
    glib::timeout_future(SETTLE).await;
    shot_after_paint(&window, &folder, "19-log").await;
    close_dialogs(&window).await;
    let released = Arc::new(AtomicU64::new(0));
    fake_frames::serve(&context.run_folder(None), released.clone());
    let mut vm = pretend_vm(&context, &machines[1].borrow());
    running_pages(window.clone(), folder.clone(), failures.clone(), machines[1].clone()).await;
    viewer_page(window.clone(), folder.clone(), failures.clone(), machines[1].clone(), released).await;
    let _ = vm.kill();
    let _ = vm.wait();
    ubuntu_pages(window.clone(), context.clone(), folder.clone(), failures.clone()).await;
    whole_flow(window.clone(), folder, failures.clone()).await;
    println!("failures: {}", failures.get());
    std::process::exit(i32::try_from(failures.get()).unwrap_or(1));
}

fn main() -> glib::ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.first().is_some_and(|word| word == "-qmp") {
        thread::sleep(PRETEND_VM_LIFE);
        return glib::ExitCode::SUCCESS;
    }
    if arguments.first().is_some_and(|word| word == "-L") {
        fake_qemu::run(&arguments);
    }
    let folder = PathBuf::from(env::args().nth(1).expect("a folder for the pictures"));
    fs::create_dir_all(&folder).unwrap();
    let mut context = Context::from_process().unwrap();
    pretend_pc(&mut context, &folder.join("pc"));
    // This program lives in `examples`, one folder below the app's own.
    let examples = env::current_exe().unwrap().parent().unwrap().to_owned();
    context.paths.binaries = examples.parent().unwrap().to_owned();
    let context = Arc::new(context);
    let walking = context.clone();
    application(context, move |window| {
        // With no browser on the other end, Broadway's frames hardly tick, and a dialog could take
        // longer to animate away than the walk waits for it.
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_enable_animations(false);
        }
        glib::spawn_future_local(walk_pages(window.clone(), walking.clone(), folder.clone()));
    })
    .run_with_args::<&str>(&[])
}
