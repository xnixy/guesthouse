//! How far a machine's cores and RAM may go on this PC, for the settings page and the command line.

use crate::context::Context;
use crate::host::{RamField, physical_cores};
use crate::machine::{Kind, MIN_CORES, MIN_RAM_GIB, Machine};
use crate::tasks::slot_of;

/// The least and most cores it may have. What it has stays allowed, so a setting that no longer fits
/// isn't refused behind the user's back.
pub fn cores_range(context: &Context, machine: &Machine) -> (u32, u32) {
    let least = if machine.kind == Kind::Plain { 1 } else { MIN_CORES };
    (least, physical_cores(&context.cpus).max(machine.cores))
}

/// How its RAM is offered, with what it has as the wish.
pub fn ram_field(context: &Context, machine: &Machine) -> RamField {
    let wanted = machine.ram_gib.into();
    if machine.has_gpu() {
        let slot = slot_of(context, machine).unwrap_or(context.slot);
        RamField::for_gpu(context, slot, MIN_RAM_GIB.into(), wanted)
    } else {
        RamField::for_plain(context, if machine.kind == Kind::Plain { 1 } else { MIN_RAM_GIB.into() }, wanted)
    }
}
