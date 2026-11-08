//! The forwarded GPU for a plain machine that holds a Linux the user installed. It's a preview, and
//! the guest needs setting up once from the driver image the app then attaches.

use super::gpu_choice::add_gpu_choice;
use super::{Shared, is_off};
use adw::prelude::*;
use guesthouse_app_core::machine::Kind;
use guesthouse_app_core::tasks::Status;

const NOTE: &str = "For a Linux you installed here yourself, on Fedora, Debian, Ubuntu, Arch or openSUSE. The VM \
                    gets the driver image and runs in the GPU's jail. The desktop stays on this screen, and the GPU \
                    is for programs. A preview: so far it has only run on my PC.";
const SETUP: &str = "Once, as root in the guest: mount -o ro LABEL=NVFWD_DRIVER /mnt, then bash \
                     /mnt/setup/setup-own.sh, then reboot. nvfwd-check says what works.";
const VM_MUST_BE_OFF: &str = "The VM must be off to change this.";

pub struct LinuxGpu {
    group: Option<adw::PreferencesGroup>,
    switch: Option<adw::SwitchRow>,
    card: Option<adw::ComboRow>,
}

impl LinuxGpu {
    pub fn new(shared: &Shared, page: &adw::PreferencesPage) -> Self {
        let machine = shared.machine.borrow().clone();
        if machine.kind != Kind::Plain {
            return Self { group: None, switch: None, card: None };
        }
        let group = adw::PreferencesGroup::builder().title("Linux GPU").build();
        page.add(&group);
        let switch = adw::SwitchRow::builder().title("GPU").subtitle(NOTE).active(machine.linux_gpu).build();
        let saving = shared.clone();
        switch.connect_active_notify(move |row| {
            saving.machine.borrow_mut().linux_gpu = row.is_active();
            saving.save();
        });
        group.add(&switch);
        let card = add_gpu_choice(shared, &group);
        let setup = adw::ActionRow::builder()
            .title("Setting up the guest")
            .subtitle(SETUP)
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        group.add(&setup);
        Self { group: Some(group), switch: Some(switch), card }
    }

    pub fn set_status(&self, status: &Status) {
        let (Some(group), Some(switch)) = (&self.group, &self.switch) else { return };
        let off = is_off(status);
        switch.set_sensitive(off);
        if let Some(card) = &self.card {
            card.set_sensitive(off);
        }
        group.set_description((!off).then_some(VM_MUST_BE_OFF));
    }
}
