//! Ubuntu's own settings: its GPU, which is a preview, and the way in if the desktop won't start.

use super::Shared;
use super::gpu_choice::add_gpu_choice;
use adw::prelude::*;
use guesthouse_app_core::linux::seed::password_file;
use guesthouse_app_core::machine::Kind;

const NOTE: &str = "This is a preview, and it has only run on my PC. If it can't start, or with it off, Ubuntu boots \
                    with a plain screen and says why.";

pub struct GpuPreview;

impl GpuPreview {
    pub fn new(shared: &Shared, page: &adw::PreferencesPage) -> Self {
        let machine = shared.machine.borrow().clone();
        if machine.kind != Kind::Linux {
            return Self;
        }
        let group = adw::PreferencesGroup::builder().title("Ubuntu").build();
        page.add(&group);
        let gpu = adw::SwitchRow::builder().title("GPU").subtitle(NOTE).active(machine.gpu_preview).build();
        let saving = shared.clone();
        gpu.connect_active_notify(move |row| {
            saving.machine.borrow_mut().gpu_preview = row.is_active();
            saving.save();
        });
        group.add(&gpu);
        add_gpu_choice(shared, &group);
        let file = password_file(&shared.window.context(), &machine);
        let login = adw::ActionRow::builder()
            .title("Console login")
            .subtitle(format!("The user is desk, and the password is in {}.", file.display()))
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        group.add(&login);
        Self
    }
}
