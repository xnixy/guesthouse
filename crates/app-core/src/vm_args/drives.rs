//! The host's drives a guest has beside its disk, on the controller's free ports.

use super::arguments::words;
use super::{Boot, VmPlan};
use crate::host_drives::FIRST_DRIVE_FD;
use crate::machine::{Drive, Machine};
use crate::text::qemu_value;

/// The disk has the controller's first port, and the two discs the next two.
const FIRST_FREE_PORT: usize = 3;

impl VmPlan {
    /// The drives this start gives the guest. Not during an install, so Windows setup can't offer a
    /// drive of the host's as a place to put Windows. Ubuntu's machine line has no controller for them.
    pub(super) fn drives_for(machine: &Machine, boot: &Boot) -> Vec<Drive> {
        let has_controller = matches!(boot, Boot::Plain { .. } | Boot::Gpu | Boot::OwnLinux { .. });
        if machine.installing || !has_controller { Vec::new() } else { machine.drives.clone() }
    }

    /// Each drive is a file the app handed over, which QEMU finds in a set of its own by number, or
    /// for a QEMU the app didn't start, a path. Last in the line, so every device a guest was
    /// installed with keeps its place.
    pub(super) fn drive_arguments(&self) -> Vec<String> {
        let mut arguments = Vec::new();
        for at in 0..self.drives.len() {
            let (number, fd) = (at + 1, FIRST_DRIVE_FD + at as i32);
            let device = format!("ide-hd,bus=ahci.{},drive=drive{number}", FIRST_FREE_PORT + at);
            let file = match &self.drives_by_path {
                Some(folder) => qemu_value(&folder.join(&self.drives[at].id).to_string_lossy()),
                None => format!("/dev/fdset/{fd}"),
            };
            let node = format!(
                "driver=host_device,node-name=drive{number},filename={file},cache.direct=on,discard=unmap"
            );
            if self.drives_by_path.is_none() {
                arguments.extend(words(&["-add-fd", &format!("fd={fd},set={fd}")]));
            }
            arguments.extend(words(&["-blockdev", &node, "-device", &device]));
        }
        arguments
    }
}
