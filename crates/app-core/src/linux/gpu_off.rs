//! Why a guest starts without its GPU, said twice. The guest gets one fixed sentence, as it might not
//! be trusted: the detail can hold host paths, a login name or compiler output. The app shows the detail.

pub const GUEST_TURNED_OFF: &str = "The GPU is turned off in this machine's settings.";
pub const GUEST_NO_DEVICE: &str = "This QEMU has no nvfwd device.";
pub const GUEST_NO_ALLOW_LIST: &str = "The host has no allow list for this guest's calls.";
pub const GUEST_NO_JAIL: &str = "The host can't jail QEMU.";
pub const GUEST_UNKNOWN_DRIVER: &str = "The host's NVIDIA driver is one this version doesn't know.";
pub const GUEST_NO_SOURCE: &str = "The host doesn't have NVIDIA's source for its driver.";
pub const GUEST_DRIVER_FILES: &str = "The host's NVIDIA driver files can't be used right now.";
pub const GUEST_TABLES_DIFFER: &str = "The host's driver changes calls the device checks.";
pub const GUEST_NO_IMAGE: &str = "The host could not make the driver image.";
pub const GUEST_NO_CARD: &str = "The host's NVIDIA driver doesn't have the card setup chose.";
pub const GUEST_NO_AMD_CARD: &str = "The host doesn't have the AMD card chosen for this machine.";
pub const GUEST_NO_GL: &str = "This QEMU can't give the guest a GPU on an AMD card.";
/// Added when there's more to say, so someone in the guest knows where to look.
const SEE_THE_HOST: &str = "The app on the host says why.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuOff {
    /// One of the fixed sentences here.
    pub for_guest: &'static str,
    /// What the host's user sees. It never goes to the guest.
    pub detail: String,
}

impl GpuOff {
    pub fn new(for_guest: &'static str, detail: impl Into<String>) -> Self {
        Self { for_guest, detail: detail.into() }
    }

    /// What goes in the file the guest reads.
    pub fn guest_text(&self) -> String {
        if self.detail == self.for_guest {
            return format!("{}\n", self.for_guest);
        }
        format!("{} {SEE_THE_HOST}\n", self.for_guest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detail_stays_out_of_the_guests_text() {
        let off = GpuOff::new(GUEST_NO_IMAGE, "/home/someone/store: gcc said no");
        assert_eq!(off.guest_text(), format!("{GUEST_NO_IMAGE} {SEE_THE_HOST}\n"));
        assert!(!off.guest_text().contains("someone"));
    }

    #[test]
    fn a_sentence_with_nothing_behind_it_stands_alone() {
        let off = GpuOff::new(GUEST_TURNED_OFF, GUEST_TURNED_OFF);
        assert_eq!(off.guest_text(), format!("{GUEST_TURNED_OFF}\n"));
    }
}
