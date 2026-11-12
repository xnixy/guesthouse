//! Cores, memory, pinning and sound.

use super::{Shared, is_off};
use crate::rows::number_row;
use adw::prelude::*;
use guesthouse_app_core::host::{cpu_list, physical_cores, vcache_cpus};
use guesthouse_app_core::limits::{cores_range, ram_field};
use guesthouse_app_core::machine::Kind;
use guesthouse_app_core::settings::max_vram_gib;
use guesthouse_app_core::tasks::Status;

const VM_MUST_BE_OFF: &str = "The VM must be off to change these.";
const SOUND_NOTE: &str = "Through PipeWire. Off, the guest's sound card plays nothing, and QEMU gets no way to your \
                          audio server or any other socket of your desktop's.";

pub struct Hardware {
    group: adw::PreferencesGroup,
    cores: adw::SpinRow,
    ram: adw::SpinRow,
    vram: adw::SpinRow,
    pin: adw::SwitchRow,
    pin_possible: bool,
    sound: adw::SwitchRow,
}

impl Hardware {
    pub fn new(shared: &Shared, page: &adw::PreferencesPage) -> Self {
        let context = shared.window.context();
        let machine = shared.machine.borrow().clone();
        let group = adw::PreferencesGroup::builder().title("Hardware").build();
        page.add(&group);
        let host_cores = physical_cores(&context.cpus);
        let (least_cores, most_cores) = cores_range(&context, &machine);
        let cores = number_row("Cores", machine.cores.into(), least_cores.into(), most_cores.into());
        cores.set_subtitle(&format!("This PC has {host_cores} cores."));
        group.add(&cores);
        let wanted = machine.ram_gib.into();
        let field = ram_field(&context, &machine);
        // What it has stays, so a setting that no longer fits isn't changed behind the user's back.
        let ram = number_row(
            "RAM in GiB",
            machine.ram_gib.into(),
            field.lower as f64,
            field.upper.max(wanted) as f64,
        );
        ram.set_subtitle(&field.subtitle);
        group.add(&ram);
        let vram = number_row(
            "Video memory cap in GiB",
            machine.vram_gib.into(),
            0.0,
            max_vram_gib(&context).into(),
        );
        vram.set_subtitle(
            "0 is no cap. A program that goes past it loses its GPU device, and others carry on.",
        );
        vram.set_visible(machine.kind == Kind::Windows);
        group.add(&vram);
        let cpus = vcache_cpus(&context.cpus);
        let pin = adw::SwitchRow::builder().title("V-Cache pinning").active(machine.vcache_pin).build();
        pin.set_subtitle(&if cpus.is_empty() {
            "This CPU has no V-Cache cores.".to_owned()
        } else {
            format!("Keeps the VM on CPUs {}.", cpu_list(&cpus))
        });
        group.add(&pin);
        // Only a GPU machine has ever had sound.
        let sound = adw::SwitchRow::builder()
            .title("Sound")
            .subtitle(SOUND_NOTE)
            .active(machine.sound)
            .visible(machine.kind == Kind::Windows)
            .build();
        group.add(&sound);
        let this = Self { group, cores, ram, vram, pin, pin_possible: !cpus.is_empty(), sound };
        this.connect(shared);
        this
    }

    fn connect(&self, shared: &Shared) {
        let saving = shared.clone();
        self.cores.connect_value_notify(move |row| {
            saving.machine.borrow_mut().cores = row.value() as u32;
            saving.save();
        });
        let saving = shared.clone();
        self.ram.connect_value_notify(move |row| {
            saving.machine.borrow_mut().ram_gib = row.value() as u32;
            saving.save();
        });
        let saving = shared.clone();
        self.vram.connect_value_notify(move |row| {
            saving.machine.borrow_mut().vram_gib = row.value() as u32;
            saving.save();
        });
        let saving = shared.clone();
        self.pin.connect_active_notify(move |row| {
            saving.machine.borrow_mut().vcache_pin = row.is_active();
            saving.save();
        });
        let saving = shared.clone();
        self.sound.connect_active_notify(move |row| {
            saving.machine.borrow_mut().sound = row.is_active();
            saving.save();
        });
    }

    pub fn set_status(&self, status: &Status) {
        let off = is_off(status);
        self.cores.set_sensitive(off);
        self.ram.set_sensitive(off);
        self.vram.set_sensitive(off);
        self.pin.set_sensitive(off && self.pin_possible);
        self.sound.set_sensitive(off);
        self.group.set_description((!off).then_some(VM_MUST_BE_OFF));
    }
}
