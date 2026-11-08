//! Ubuntu guests. Their route is its own: Ubuntu's installer runs with no hands, and the host's
//! driver reaches the guest on a read-only image, with the calls forwarded through a QEMU device. If
//! anything about that is in doubt, the GPU stays off and the VM still boots.

pub mod amd;
pub mod amd_image;
pub mod amd_says;
pub mod card;
pub mod driver_files;
pub mod driver_host;
pub mod driver_image;
pub mod driver_versions;
pub mod forwarding;
pub mod gpu_off;
pub mod guest_says;
pub mod layout;
pub mod seed;
mod tables;

use crate::context::Context;
use std::path::PathBuf;

const SENSOR_DAEMON: &str = "nvfwd-sensord";

/// The device's helper, which is built with the device and sits beside its QEMU.
pub fn sensor_daemon(context: &Context) -> PathBuf {
    context.paths.qemu.with_file_name(SENSOR_DAEMON)
}

/// The guests' sources: the program folder's, or else the repo's in a build of it.
pub fn guest_sources(context: &Context) -> PathBuf {
    context.paths.linux_sources()
}
