//! What has to be true of this PC for QEMU to start with the forwarded GPU. Each of these would stop
//! QEMU dead, and that would cost the VM, so they're asked first and cost only the GPU. Its jail is
//! asked about with the rest of the start, as for Windows: see `jailing`.

use super::gpu_off::{GUEST_NO_ALLOW_LIST, GUEST_NO_DEVICE, GUEST_NO_GL, GpuOff};
use std::path::Path;
use std::process::{Command, Stdio};

const CHECK_SECONDS: &str = "10";
/// QEMU exits with 0 either way. It only lists options on stdout for a device it knows.
const DEVICE_QUESTION: &str = "nvfwd,help";
const DEVICE_ANSWER: &str = "nvfwd options:";
/// An AMD card's GPU is the GL screen, with native context, which only a QEMU with virglrenderer has.
const GL_QUESTION: &str = "virtio-vga-gl,help";
const GL_ANSWER: &str = "drm_native_context=";
/// The namespaces the jail uses, which a PC can refuse, with nothing else in the way.
const SANDBOX_QUESTION: [&str; 11] = [
    "--unshare-user",
    "--unshare-ipc",
    "--unshare-uts",
    "--ro-bind",
    "/usr",
    "/usr",
    "--symlink",
    "usr/lib64",
    "/lib64",
    "--",
    "/usr/bin/true",
];

fn answers_yes(command: &mut Command) -> bool {
    let quiet = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    quiet.status().is_ok_and(|status| status.success())
}

fn knows_device(qemu: &Path, question: &str, answer: &str) -> bool {
    let mut asking = Command::new("timeout");
    asking.arg(CHECK_SECONDS).arg(qemu).args(["-device", question]).stdin(Stdio::null());
    let said = asking.output().map(|said| String::from_utf8_lossy(&said.stdout).into_owned());
    said.is_ok_and(|text| text.contains(answer))
}

fn has_device(qemu: &Path) -> bool {
    knows_device(qemu, DEVICE_QUESTION, DEVICE_ANSWER)
}

/// Says why QEMU can't start with a GPU on an AMD card, or `None` if it can.
pub fn amd_problem(qemu: &Path) -> Option<GpuOff> {
    (!knows_device(qemu, GL_QUESTION, GL_ANSWER)).then(|| {
        let detail = format!(
            "{} has no GL screen with native context. It needs a build with OpenGL and virglrenderer.",
            qemu.display()
        );
        GpuOff::new(GUEST_NO_GL, detail)
    })
}

/// Whether bubblewrap can make namespaces here. Ubuntu 24.04 can refuse it, through AppArmor.
pub fn sandbox_runs() -> bool {
    answers_yes(Command::new("timeout").arg(CHECK_SECONDS).arg("bwrap").args(SANDBOX_QUESTION))
}

/// Says why QEMU can't start with the forwarded GPU, or `None` if it can.
pub fn forwarding_problem(qemu: &Path, allow_list: &Path) -> Option<GpuOff> {
    if !has_device(qemu) {
        let detail =
            format!("{} has no nvfwd device. It needs a build with the device added.", qemu.display());
        return Some(GpuOff::new(GUEST_NO_DEVICE, detail));
    }
    (!allow_list.exists()).then(|| {
        let detail = format!("{} is missing, so no call could reach the host.", allow_list.display());
        GpuOff::new(GUEST_NO_ALLOW_LIST, detail)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;

    /// As the real one does, it exits with 0 whether or not it knows the device.
    fn fake_qemu(scratch: &Scratch, answer: &str) -> std::path::PathBuf {
        scratch.program("qemu", &format!("#!/bin/sh\necho '{answer}'\nexit 0\n"))
    }

    #[test]
    fn a_qemu_with_no_device_is_named() {
        let scratch = Scratch::new();
        let said = forwarding_problem(&fake_qemu(&scratch, ""), Path::new("/nothing")).unwrap();
        assert_eq!(said.for_guest, GUEST_NO_DEVICE);
    }

    #[test]
    fn a_qemu_that_is_not_there_counts_as_having_no_device() {
        let said = forwarding_problem(Path::new("/nothing/qemu"), Path::new("/nothing")).unwrap();
        assert_eq!(said.for_guest, GUEST_NO_DEVICE);
    }

    #[test]
    fn a_qemu_with_no_gl_screen_has_no_gpu_for_an_amd_card() {
        let scratch = Scratch::new();
        assert_eq!(amd_problem(&fake_qemu(&scratch, "nvfwd options:")).unwrap().for_guest, GUEST_NO_GL);
        assert_eq!(amd_problem(&fake_qemu(&scratch, "  drm_native_context=<bool>")), None);
    }

    #[test]
    fn a_missing_allow_list_is_named() {
        let scratch = Scratch::new();
        let said =
            forwarding_problem(&fake_qemu(&scratch, DEVICE_ANSWER), &scratch.path("allow.txt")).unwrap();
        assert!(said.detail.contains("allow.txt"));
        assert!(!said.guest_text().contains('/'));
    }
}
