//! Which PCI identity the virtual render device shows Windows. Windows' own virtual render driver
//! only binds to Microsoft's, so every machine starts with that one. Browsers and streaming programs
//! look at the vendor number before they use a GPU, and stay on the CPU when it's Microsoft's. So once
//! a guest holds a driver package for its card maker's number, NVIDIA's or AMD's, written there from
//! its own copy of Windows' driver, every VM after that shows that number. The choice is kept in the
//! machine's folder, as it has to outlive a VM: a guest shown a number without its package would
//! start with no GPU.

use crate::context::Context;
use crate::failure::Result;
use crate::machine::Machine;
use crate::settings::write_json;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const STATE_FILE: &str = "gpu-identity.json";

/// A PCI vendor and device number, as the guest's bus shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciIdentity {
    pub vendor: u16,
    pub device: u16,
}

/// Microsoft's virtual render device, which Windows' own driver binds to. QEMU gives this one by itself.
pub const MICROSOFT_RENDER: PciIdentity = PciIdentity { vendor: 0x1414, device: 0x008E };
/// NVIDIA's vendor number with a device number NVIDIA has never shipped, so NVIDIA's own driver can't
/// claim the device.
pub const NVIDIA_RENDER: PciIdentity = PciIdentity { vendor: 0x10DE, device: 0x2BFE };
/// AMD's vendor number with a device number AMD has never shipped, far from every range it uses.
pub const AMD_RENDER: PciIdentity = PciIdentity { vendor: 0x1002, device: 0x7FFE };

/// The number a machine shows once its guest holds the package: its card maker's. A Windows machine
/// stays with one maker, so this never changes under a guest.
pub fn maker_identity(machine: &Machine) -> PciIdentity {
    if machine.windows_amd_card().is_some() { AMD_RENDER } else { NVIDIA_RENDER }
}

/// The card maker's name, for what the user hears.
pub fn maker_name(machine: &Machine) -> &'static str {
    if machine.windows_amd_card().is_some() { "AMD" } else { "NVIDIA" }
}

impl PciIdentity {
    /// The start of the hardware id Windows gives the device, with the rest left open.
    pub fn windows_pattern(self) -> String {
        format!(r"PCI\VEN_{:04X}&DEV_{:04X}*", self.vendor, self.device)
    }

    /// What the render device's QEMU line needs after its other properties. Nothing for the identity
    /// QEMU gives by itself, so a QEMU from before the properties still starts.
    pub fn qemu_properties(self) -> String {
        if self == MICROSOFT_RENDER {
            return String::new();
        }
        format!(",vendor-id={:#06x},device-id={:#06x}", self.vendor, self.device)
    }
}

/// Which identity a machine's next start shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shown {
    #[default]
    Microsoft,
    /// The card maker's, [`maker_identity`]. Written as "nvidia" before AMD cards came.
    #[serde(alias = "nvidia")]
    Maker,
}

impl Shown {
    pub fn identity(self, machine: &Machine) -> PciIdentity {
        match self {
            Self::Microsoft => MICROSOFT_RENDER,
            Self::Maker => maker_identity(machine),
        }
    }
}

/// What a state from before AMD cards holds a package for.
fn nvidia_vendor() -> u16 {
    NVIDIA_RENDER.vendor
}

/// How a machine stands. Tasks write it in the machine's folder, and the app reads it from there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityState {
    pub shown: Shown,
    /// The guest script that failed to write the package, which a start doesn't run again.
    #[serde(default)]
    pub failed_script: String,
    /// Why Windows stays on Microsoft's number.
    #[serde(default)]
    pub reason: String,
    /// What the user should hear about a machine that's up.
    #[serde(default)]
    pub note: String,
    /// The next start is Windows' first on its maker's number, which ends with Windows restarting once.
    #[serde(default)]
    pub restart_due: bool,
    /// The vendor number of the package Windows holds.
    #[serde(default = "nvidia_vendor")]
    pub package_vendor: u16,
    /// The driver version the package gives the device, which is the card driver's. Empty for a
    /// package from before it carried one, which gives Windows' own driver's.
    #[serde(default)]
    pub package_version: String,
}

impl Default for IdentityState {
    fn default() -> Self {
        Self {
            shown: Shown::default(),
            failed_script: String::new(),
            reason: String::new(),
            note: String::new(),
            restart_due: false,
            package_vendor: nvidia_vendor(),
            package_version: String::new(),
        }
    }
}

impl IdentityState {
    /// Whether this start, the first on its maker's number, has to restart Windows before it's ready.
    pub fn restarts_windows(&self) -> bool {
        self.shown == Shown::Maker && self.restart_due
    }

    /// What the user should hear, if anything. A note is about the machine as it runs now.
    pub fn problem(&self, up: bool) -> &str {
        if self.reason.is_empty() && up { &self.note } else { &self.reason }
    }
}

fn state_file(context: &Context, machine: &Machine) -> PathBuf {
    machine.folder(context).join(STATE_FILE)
}

/// A file that's missing or bad says Microsoft's number, which always works. So does a package for
/// another maker's number than this machine's, which the guest gets again for its own maker.
pub fn read_state(context: &Context, machine: &Machine) -> IdentityState {
    let text = fs::read_to_string(state_file(context, machine)).unwrap_or_default();
    let mut state: IdentityState = serde_json::from_str(&text).unwrap_or_default();
    if state.shown == Shown::Maker && state.package_vendor != maker_identity(machine).vendor {
        state.shown = Shown::Microsoft;
        state.restart_due = false;
    }
    state
}

pub fn write_state(context: &Context, machine: &Machine, state: &IdentityState) -> Result<()> {
    write_json(&state_file(context, machine), state)
}

/// For a new Windows on this machine's disk: what the old one had taken in is gone with it.
pub fn forget_state(context: &Context, machine: &Machine) {
    let _ = fs::remove_file(state_file(context, machine));
}

/// The identity this machine's next start shows.
pub fn shown_identity(context: &Context, machine: &Machine) -> PciIdentity {
    read_state(context, machine).shown.identity(machine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Scratch, scratch_context};

    #[test]
    fn the_patterns_are_how_windows_writes_a_hardware_id() {
        assert_eq!(MICROSOFT_RENDER.windows_pattern(), r"PCI\VEN_1414&DEV_008E*");
        assert_eq!(NVIDIA_RENDER.windows_pattern(), r"PCI\VEN_10DE&DEV_2BFE*");
    }

    #[test]
    fn only_another_identity_than_qemus_own_needs_properties() {
        assert_eq!(MICROSOFT_RENDER.qemu_properties(), "");
        assert_eq!(NVIDIA_RENDER.qemu_properties(), ",vendor-id=0x10de,device-id=0x2bfe");
    }

    #[test]
    fn a_machine_with_nothing_written_shows_microsofts() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let machine = Machine::new("x", "x", "", "");
        assert_eq!(shown_identity(&context, &machine), MICROSOFT_RENDER);
        let state = IdentityState { shown: Shown::Maker, ..IdentityState::default() };
        write_state(&context, &machine, &state).unwrap();
        assert_eq!(shown_identity(&context, &machine), NVIDIA_RENDER);
        assert_eq!(read_state(&context, &machine), state);
    }

    #[test]
    fn a_reason_is_heard_always_and_a_note_only_while_up() {
        let state = IdentityState { note: "later".into(), ..IdentityState::default() };
        assert_eq!(state.problem(true), "later");
        assert_eq!(state.problem(false), "");
        let state = IdentityState { reason: "why".into(), ..state };
        assert_eq!(state.problem(true), "why");
        assert_eq!(state.problem(false), "why");
    }
}
