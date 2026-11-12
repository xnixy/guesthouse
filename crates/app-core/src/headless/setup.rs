//! First-run setup, step by step as the wizard does it: what this PC has, NVIDIA's driver for the guest,
//! and the card.

use crate::context::Context;
use crate::failure::{Failure, Result};
use crate::settings::driver_folder;
use crate::setup::cards::{chosen_card, remember_card, remember_card_at};
use crate::setup::checks::{Check, CheckState, check_pc};
use crate::setup::driver::{Extraction, WINDOWS_DRIVER, check_installer, driver_problem};
use std::cell::{Cell, RefCell};
use std::io::Write;

const CANNOT_RUN: &str = "Windows with GPU-PV cannot run yet. Fix what is marked failed, then run setup again. \
                          Plain VMs and Ubuntu do not need any of this.";
/// The copy says how far it is at each step of this many percent.
const PERCENT_STEP: u32 = 10;

fn state_word(state: CheckState) -> &'static str {
    match state {
        CheckState::Ok => "ok",
        CheckState::Warning => "warning",
        CheckState::Failed => "failed",
    }
}

pub(super) fn setup(
    context: &Context,
    driver: Option<&str>,
    card: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    setup_after(context, &check_pc(context), driver, card, out)
}

/// The steps once the PC has been looked at, so a test can say what was found.
pub(super) fn setup_after(
    context: &Context,
    checks: &[Check],
    driver: Option<&str>,
    card: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    for check in checks {
        writeln!(out, "{:<8} {}: {}", state_word(check.state), check.title, check.text)?;
        if check.state != CheckState::Ok && !check.fix.is_empty() {
            writeln!(out, "         {}", check.fix)?;
        }
    }
    if checks.iter().any(|check| check.state == CheckState::Failed) {
        return Err(Failure::new(CANNOT_RUN));
    }
    if let Some(installer) = driver {
        copy_driver(context, installer, out)?;
    } else if driver_problem(&driver_folder(context)).is_none() {
        writeln!(out, "Driver {WINDOWS_DRIVER} is ready. Its files are already here.")?;
    } else {
        return Err(Failure(format!(
            "NVIDIA's driver {WINDOWS_DRIVER} for Windows is needed. Download it from NVIDIA and run setup with \
             --driver PATH, the installer's .exe."
        )));
    }
    match card {
        Some(pci) => remember_card_at(context, pci)?,
        None => remember_card(context)?,
    }
    if let Some(chosen) = chosen_card(context) {
        writeln!(out, "The card is the {} at {}.", chosen.name, chosen.pci)?;
    }
    Ok(writeln!(out, "Guesthouse is ready. All that is left is a Windows 11 ISO, or an Ubuntu server ISO.")?)
}

fn copy_driver(context: &Context, installer: &str, out: &mut dyn Write) -> Result<()> {
    check_installer(installer)?;
    writeln!(out, "Copying the driver's files")?;
    let (said, out) = (Cell::new(0), RefCell::new(out));
    let folder = Extraction::new(installer).run(context, &|percent| {
        let step = percent / PERCENT_STEP;
        if step > said.get() {
            said.set(step);
            let _ = writeln!(out.borrow_mut(), "{percent}%");
        }
    })?;
    Ok(writeln!(out.borrow_mut(), "The files are in {}.", folder.display())?)
}
