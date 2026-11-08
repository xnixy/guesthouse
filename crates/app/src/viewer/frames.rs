//! What the screen needs from wherever its frames come from.

use super::guest_cursor::CursorPicture;
use gtk::gdk;
use gtk::prelude::*;
use guesthouse_app_core::machine::Machine;
use guesthouse_app_core::tasks::Status;
use guesthouse_frame_socket::CursorMove;

const BYTES_PER_PIXEL: usize = 4;

/// Where a machine's own frames come from, besides QEMU's display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRoute {
    /// Only QEMU's display: firmware, an install, or a machine with no GPU.
    QemuDisplay,
    /// The frame socket, for a guest on GPU-PV.
    FrameSocket,
    /// The nvfwd device, for a Linux guest whose desktop is on the GPU.
    Forwarded,
}

impl FrameRoute {
    pub fn of(machine: &Machine) -> Self {
        if machine.has_gpu() {
            Self::FrameSocket
        } else if machine.amd_card().is_some() {
            // QEMU's display reads the AMD card's frames back for the viewer.
            Self::QemuDisplay
        } else if machine.wants_forwarded_gpu() || machine.own_linux_gpu() {
            Self::Forwarded
        } else {
            Self::QemuDisplay
        }
    }

    /// The route while it runs. A Linux guest that says its desktop is on the plain screen this boot
    /// has no GPU frames coming, and may have shown some before it went back there.
    pub fn now(machine: &Machine, status: &Status) -> Self {
        match Self::of(machine) {
            Self::Forwarded if status.plain_desktop => Self::QemuDisplay,
            route => route,
        }
    }
}

pub trait FrameSource {
    /// A new texture of the newest frame, if there's a new one. A still screen has none to give. It's
    /// always a new texture, because GTK can keep a copy of one that's reused and go on showing it.
    fn newest(&mut self) -> Option<gdk::Texture>;
    /// Whether frames can still come. One that has gone is dropped, and looked for again.
    fn alive(&self) -> bool;
    /// How often the source has heard each kind of call, by name, for the page checks.
    fn calls(&self) -> Vec<(String, u64)> {
        Vec::new()
    }
    /// How bright the guest wants SDR white, while it's showing an HDR picture.
    fn sdr_white_nits(&self) -> Option<u32> {
        None
    }
    /// The guest's cursor and where it is, for a viewer that has to draw it itself.
    fn drawn_cursor(&self) -> Option<(CursorPicture, CursorMove)> {
        None
    }
    /// The pointer to show over the screen, when the guest has changed it.
    fn new_cursor(&mut self) -> Option<gdk::Cursor> {
        None
    }
}

/// Pixels in memory, as blue, green, red and alpha, for a thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pixels {
    width: usize,
    height: usize,
    stride: usize,
    data: Vec<u8>,
}

impl Pixels {
    /// A smaller copy of this width. Every pixel is picked, not blended: it only has to say which
    /// machine this is.
    fn shrunk(&self, width: usize) -> Self {
        let width = width.min(self.width).max(1);
        let height = (self.height * width / self.width).max(1);
        let mut data = Vec::with_capacity(width * height * BYTES_PER_PIXEL);
        for y in 0..height {
            let row = y * self.height / height * self.stride;
            for x in 0..width {
                let at = row + x * self.width / width * BYTES_PER_PIXEL;
                data.extend_from_slice(&self.data[at..at + BYTES_PER_PIXEL]);
            }
        }
        Self { width, height, stride: width * BYTES_PER_PIXEL, data }
    }
}

/// A small PNG of a texture.
pub fn thumbnail(texture: &gdk::Texture, width: usize) -> Vec<u8> {
    let mut downloader = gdk::TextureDownloader::new(texture);
    downloader.set_format(gdk::MemoryFormat::B8g8r8a8);
    let (bytes, stride) = downloader.download_bytes();
    let pixels = Pixels {
        width: texture.width() as usize,
        height: texture.height() as usize,
        stride,
        data: bytes.to_vec(),
    };
    let small = pixels.shrunk(width);
    let memory = gdk::MemoryTexture::new(
        small.width as i32,
        small.height as i32,
        gdk::MemoryFormat::B8g8r8a8,
        &gtk::glib::Bytes::from(&small.data),
        small.stride,
    );
    memory.save_to_png_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linux_guest_on_the_gpu_shows_forwarded_frames_until_it_says_its_desktop_is_plain() {
        use guesthouse_app_core::machine::Kind;
        let plain = Machine { kind: Kind::Plain, ..Machine::new("fedora", "Fedora", "/d", "/v") };
        let own = Machine { linux_gpu: true, ..plain.clone() };
        let running = Status::default();
        let back_to_plain = Status { plain_desktop: true, ..Status::default() };
        assert_eq!(FrameRoute::now(&plain, &running), FrameRoute::QemuDisplay);
        assert_eq!(FrameRoute::now(&own, &running), FrameRoute::Forwarded);
        assert_eq!(FrameRoute::now(&own, &back_to_plain), FrameRoute::QemuDisplay);
        let windows = Machine { kind: Kind::Windows, ..plain };
        assert_eq!(FrameRoute::now(&windows, &back_to_plain), FrameRoute::FrameSocket);
        let on_amd = Machine { amd_gpu: "0000:7c:00.0".into(), ..own.clone() };
        assert_eq!(FrameRoute::now(&on_amd, &running), FrameRoute::QemuDisplay);
    }

    #[test]
    fn a_thumbnail_keeps_the_shape_of_the_picture() {
        let stride = 640 * BYTES_PER_PIXEL + 64;
        let pixels = Pixels { width: 640, height: 360, stride, data: vec![0; stride * 360] };
        let small = pixels.shrunk(160);
        assert_eq!((small.width, small.height), (160, 90));
        assert_eq!(small.data.len(), small.stride * small.height);
    }
}
