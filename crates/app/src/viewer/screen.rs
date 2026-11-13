//! The guest's screen in a widget, and the keyboard and mouse going the other way.
//!
//! Frames come from a [`FrameSource`], with a new texture for each one.

use super::capture_state::Capture;
use super::cursor_layer::CursorLayer;
use super::dbus_frames::DbusFrames;
use super::forwarded_frames::ForwardedFrames;
use super::frames::{FrameRoute, FrameSource, thumbnail};
use super::overlay::PerformanceOverlay;
use super::pointer_lock::{PointerLock, pointer_lock};
use super::socket_frames::SocketFrames;
use adw::prelude::*;
use gtk::{gdk, glib};
use guesthouse_app_core::forwarded_input::ForwardedInput;
use guesthouse_app_core::input::{Guest, INPUT_SOCKET, MOUSE, TABLET};
use guesthouse_app_core::vm_args::{DISPLAY_SOCKET, FORWARDED_INPUT_SOCKET};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A tick callback is no use here. A tick that finds nothing draws nothing, and with nothing drawn the
/// compositor sends no frame event, so GTK falls back to a 60 Hz timer and frames wait for it.
const POLL: Duration = Duration::from_millis(1);
const FOLLOW_GUEST: Duration = Duration::from_secs(1);
const SPINNER_SIZE: i32 = 48;
const FIRST_RATIO: f32 = 16.0 / 9.0;
const STYLE: &str = ".guest-screen { background: #000000; color: #ffffff; }
.performance-overlay { background: rgba(0, 0, 0, 0.65); color: white; padding: 6px 10px; border-radius: 6px;
  font-size: smaller; font-feature-settings: \"tnum\"; }";

pub(super) struct Inner {
    pub(super) root: gtk::Overlay,
    pub(super) picture: gtk::Picture,
    pub(super) aspect: gtk::AspectFrame,
    pub(super) spinner: gtk::Spinner,
    /// The guest's cursor as the host's pointer, once it has one.
    pub(super) guest_pointer: RefCell<Option<gdk::Cursor>>,
    /// The guest's cursor, while there's no host pointer to show it.
    pub(super) cursor_layer: CursorLayer,
    /// No pointer at all, over a guest that draws its own into its frames.
    pub(super) no_pointer: Option<gdk::Cursor>,
    pub(super) run_folder: RefCell<PathBuf>,
    /// The guest's frames from the GPU, when it has any.
    pub(super) gpu: RefCell<Option<Box<dyn FrameSource>>>,
    pub(super) route: Cell<FrameRoute>,
    /// The guest's card doesn't draw this desktop, so its frames come in memory.
    pub(super) memory_frames: Cell<bool>,
    /// An Ubuntu guest's keyboard and pointer, once it's showing its own frames.
    pub(super) forwarded: RefCell<Option<ForwardedInput>>,
    /// QEMU's display, for whatever has no GPU frames to show.
    pub(super) qemu: RefCell<Option<Box<dyn FrameSource>>>,
    /// A GPU guest has no frames of its own until its driver is up, and again after a reset.
    pub(super) booting: Cell<bool>,
    pub(super) input_socket: RefCell<PathBuf>,
    pub(super) guest: RefCell<Option<Guest>>,
    pub(super) latest: RefCell<Option<gdk::Texture>>,
    /// A frame was handed to GTK and hasn't been drawn yet.
    pub(super) waiting: Cell<bool>,
    pub(super) frames: Cell<u64>,
    pub(super) keys_down: RefCell<BTreeSet<u32>>,
    pub(super) buttons_down: RefCell<BTreeSet<&'static str>>,
    pub(super) pointer: Cell<Option<(i64, i64)>>,
    pub(super) lock: RefCell<Option<Box<dyn PointerLock>>>,
    pub(super) capture: Cell<Capture>,
    pub(super) capture_input: RefCell<Vec<gtk::EventController>>,
    pub(super) capture_changed: RefCell<Box<dyn Fn()>>,
    pub(super) overlay_wanted: Cell<bool>,
    pub(super) overlay: RefCell<Option<PerformanceOverlay>>,
    pub(super) poll_source: RefCell<Option<glib::SourceId>>,
    pub(super) followed_at: Cell<Instant>,
}

#[derive(Clone)]
pub struct Screen(pub(super) Rc<Inner>);

impl Screen {
    /// `memory_frames` is whether to ask for frames in memory. It has to be known before the first
    /// connection, which can be made here, when the daemon already has a ring.
    pub fn new(
        run_folder: PathBuf,
        route: FrameRoute,
        memory_frames: bool,
        show_overlay: bool,
        capture_changed: impl Fn() + 'static,
    ) -> Self {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Fill)
            .can_shrink(true)
            .focusable(true)
            .build();
        let aspect = gtk::AspectFrame::builder().child(&picture).obey_child(false).ratio(FIRST_RATIO).build();
        let spinner = gtk::Spinner::builder().spinning(true).build();
        spinner.set_size_request(SPINNER_SIZE, SPINNER_SIZE);
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        let root = gtk::Overlay::new();
        root.set_child(Some(&aspect));
        root.add_overlay(&spinner);
        let cursor_layer = CursorLayer::new();
        root.add_overlay(cursor_layer.widget());
        root.add_css_class("guest-screen");
        let style = gtk::CssProvider::new();
        style.load_from_string(STYLE);
        gtk::style_context_add_provider_for_display(
            &WidgetExt::display(&root),
            &style,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let lock = pointer_lock(&WidgetExt::display(&root));
        let this = Self(Rc::new(Inner {
            root,
            picture,
            aspect,
            spinner,
            guest_pointer: RefCell::default(),
            cursor_layer,
            no_pointer: gdk::Cursor::from_name("none", None),
            gpu: RefCell::default(),
            qemu: RefCell::default(),
            booting: Cell::new(true),
            route: Cell::new(route),
            memory_frames: Cell::new(memory_frames),
            forwarded: RefCell::default(),
            input_socket: RefCell::new(run_folder.join(INPUT_SOCKET)),
            run_folder: RefCell::new(run_folder),
            guest: RefCell::default(),
            latest: RefCell::default(),
            waiting: Cell::new(false),
            frames: Cell::new(0),
            keys_down: RefCell::default(),
            buttons_down: RefCell::default(),
            pointer: Cell::new(None),
            lock: RefCell::new(lock),
            capture: Cell::default(),
            capture_input: RefCell::default(),
            capture_changed: RefCell::new(Box::new(capture_changed)),
            overlay_wanted: Cell::new(show_overlay),
            overlay: RefCell::default(),
            poll_source: RefCell::default(),
            followed_at: Cell::new(Instant::now()),
        }));
        this.add_input();
        this.connect_life();
        this.follow_guest();
        this
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.0.root
    }

    pub fn focus(&self) {
        self.0.picture.grab_focus();
    }

    pub fn has_frames(&self) -> bool {
        self.0.latest.borrow().is_some()
    }

    pub fn has_input(&self) -> bool {
        self.0.guest.borrow().as_ref().is_some_and(Guest::knows_mice)
    }

    pub fn captured(&self) -> bool {
        self.0.capture.get().held()
    }

    pub fn capture_asked(&self) -> bool {
        self.0.capture.get().asked()
    }

    /// Whether the next connection asks for frames in memory. One already made keeps what it has.
    pub fn set_memory_frames(&self, in_memory: bool) {
        self.0.memory_frames.set(in_memory);
    }

    /// Follows the machine to another slot, or to its GPU, as an install does when it ends in its first
    /// boot.
    pub fn follow_machine(&self, run_folder: PathBuf, route: FrameRoute) {
        let socket = run_folder.join(INPUT_SOCKET);
        if self.0.route.replace(route) != route || *self.0.input_socket.borrow() != socket {
            *self.0.run_folder.borrow_mut() = run_folder;
            self.0.gpu.borrow_mut().take();
            self.0.qemu.borrow_mut().take();
            self.0.booting.set(true);
            // A still screen sends nothing new, and the last one shown here was the other route's.
            self.forget_picture();
            *self.0.input_socket.borrow_mut() = socket;
            self.close_guest();
            self.follow_guest();
        }
    }

    fn forget_picture(&self) {
        self.0.latest.borrow_mut().take();
        self.0.picture.set_paintable(None::<&gdk::Paintable>);
        self.0.spinner.set_visible(true);
    }

    fn connect_life(&self) {
        let this = self.clone();
        self.0.root.connect_realize(move |_| this.on_realize());
        let this = self.clone();
        self.0.root.connect_unrealize(move |_| this.on_unrealize());
        let picture = self.0.picture.clone();
        self.0.root.connect_map(move |_| {
            picture.grab_focus();
        });
    }

    fn on_realize(&self) {
        if let Some(clock) = self.0.root.frame_clock() {
            let fresh = self.clone();
            clock.connect_before_paint(move |_| fresh.freshen());
            let waiting = self.clone();
            clock.connect_after_paint(move |_| waiting.0.waiting.set(false));
        }
        let this = self.clone();
        let source = glib::timeout_add_local(POLL, move || {
            this.poll();
            glib::ControlFlow::Continue
        });
        *self.0.poll_source.borrow_mut() = Some(source);
        self.follow_overlay();
    }

    fn on_unrealize(&self) {
        if let Some(source) = self.0.poll_source.borrow_mut().take() {
            source.remove();
        }
        self.0.overlay.borrow_mut().take();
        self.let_go();
        self.close_guest();
        self.0.gpu.borrow_mut().take();
        self.0.qemu.borrow_mut().take();
        self.forget_picture();
    }

    fn poll(&self) {
        self.follow_capture();
        if let Some(guest) = self.0.guest.borrow_mut().as_mut() {
            guest.flush();
        }
        if !self.0.waiting.get()
            && let Some(texture) = self.next_texture()
        {
            self.show(texture);
        }
        if self.0.followed_at.get().elapsed() >= FOLLOW_GUEST {
            self.follow_guest();
            self.0.followed_at.set(Instant::now());
        }
    }

    /// GTK is about to draw, which can be a whole refresh after it was handed its frame: it draws when
    /// the compositor asks, and no sooner. A frame that came since is the one to draw, or everything
    /// on screen is a refresh older than it has to be.
    fn freshen(&self) {
        if self.0.waiting.get()
            && let Some(texture) = self.next_texture()
        {
            self.show(texture);
        }
    }

    /// The guest's own frames win, and QEMU's are only looked at until they come.
    fn next_texture(&self) -> Option<gdk::Texture> {
        let from_gpu = self.0.gpu.borrow_mut().as_mut().and_then(|source| source.newest());
        self.follow_gpu_cursor();
        if from_gpu.is_some() {
            self.0.booting.set(false);
            self.0.qemu.borrow_mut().take();
            return from_gpu;
        }
        let mut qemu = self.0.qemu.borrow_mut();
        let source = qemu.as_mut()?;
        if let Some(cursor) = source.new_cursor() {
            self.0.picture.set_cursor(Some(&cursor));
        }
        source.newest()
    }

    /// A cursor moving makes no frame, so this is looked at whether a frame came or not.
    fn follow_gpu_cursor(&self) {
        let mut gpu = self.0.gpu.borrow_mut();
        let Some(source) = gpu.as_mut() else {
            // QEMU's display sets its own, and only the hidden one is this side's to take back.
            if self.0.no_pointer.is_some() && self.0.picture.cursor() == self.0.no_pointer {
                self.0.picture.set_cursor(None);
            }
            return;
        };
        if let Some(cursor) = source.new_cursor() {
            *self.0.guest_pointer.borrow_mut() = Some(cursor);
        }
        // A pointer set here would show through the capture, which hides the root's.
        let captured = self.captured();
        let pointer = if self.guest_draws_pointer() {
            self.0.no_pointer.clone()
        } else {
            self.0.guest_pointer.borrow().clone().filter(|_| !captured)
        };
        if self.0.picture.cursor() != pointer {
            self.0.picture.set_cursor(pointer.as_ref());
        }
        let drawn = source.drawn_cursor().filter(|_| captured);
        let place = drawn.as_ref().and_then(|(_, position)| self.on_widget(position.x, position.y));
        match (drawn, place) {
            (Some((cursor, _)), Some((x, y, scale))) => self.0.cursor_layer.show(&cursor, x, y, scale),
            _ => self.0.cursor_layer.hide(),
        }
    }

    /// The stand-in for nvidia-modeset has no cursor plane, so a Linux desktop on the GPU draws its pointer
    /// into its frames. The host's would be a second one, ahead of it.
    fn guest_draws_pointer(&self) -> bool {
        self.0.route.get() == FrameRoute::Forwarded && !self.0.booting.get()
    }

    /// A point in the guest's pixels as a point on the screen widget, and the scale between them.
    fn on_widget(&self, x: i32, y: i32) -> Option<(f64, f64, f64)> {
        let width = self.0.latest.borrow().as_ref()?.width();
        let bounds = self.0.picture.compute_bounds(&self.0.root)?;
        let scale = f64::from(bounds.width()) / f64::from(width.max(1));
        Some((
            f64::from(bounds.x()) + f64::from(x) * scale,
            f64::from(bounds.y()) + f64::from(y) * scale,
            scale,
        ))
    }

    fn show(&self, texture: gdk::Texture) {
        self.0.picture.set_paintable(Some(&texture));
        self.0.aspect.set_ratio(texture.width() as f32 / texture.height() as f32);
        self.0.spinner.set_visible(false);
        // One that takes the place of a frame still waiting to be drawn isn't one more on screen.
        if !self.0.waiting.replace(true) {
            self.0.frames.set(self.0.frames.get() + 1);
        }
        *self.0.latest.borrow_mut() = Some(texture);
    }

    /// The newest frame as a small PNG, or `None` if nothing has been shown.
    pub fn thumbnail(&self, width: usize) -> Option<Vec<u8>> {
        Some(thumbnail(self.0.latest.borrow().as_ref()?, width))
    }

    /// What kind of texture is on screen, for the page checks.
    pub fn texture_kind(&self) -> String {
        self.0.latest.borrow().as_ref().map_or_else(String::new, |texture| texture.type_().name().to_owned())
    }

    /// What QEMU's display has called, for the page checks.
    pub fn display_calls(&self) -> Vec<(String, u64)> {
        self.0.qemu.borrow().as_ref().map(|source| source.calls()).unwrap_or_default()
    }

    /// How bright the guest wants SDR white, if it's showing an HDR picture.
    pub fn sdr_white_nits(&self) -> Option<u32> {
        self.0.gpu.borrow().as_ref().and_then(|source| source.sdr_white_nits())
    }

    pub fn frames_shown(&self) -> u64 {
        self.0.frames.get()
    }

    fn monitor_refresh(&self) -> u32 {
        let native = self.0.root.native().and_then(|native| native.surface());
        let monitor = native.and_then(|surface| surface.display().monitor_at_surface(&surface));
        monitor.map_or(0, |monitor| monitor.refresh_rate() as u32)
    }

    /// Picks up the guest's frames once they're on offer, and a restarted daemon's.
    fn follow_gpu(&self) {
        let mut gpu = self.0.gpu.borrow_mut();
        if gpu.as_ref().is_some_and(|source| !source.alive()) {
            *gpu = None;
        }
        if gpu.is_none() {
            let (folder, display) = (self.0.run_folder.borrow(), WidgetExt::display(&self.0.root));
            *gpu = match self.0.route.get() {
                FrameRoute::QemuDisplay => None,
                FrameRoute::FrameSocket => {
                    SocketFrames::connect(
                        &folder,
                        &display,
                        self.monitor_refresh(),
                        self.0.memory_frames.get(),
                    )
                }
                .ok()
                .map(|source| Box::new(source) as Box<dyn FrameSource>),
                FrameRoute::Forwarded => ForwardedFrames::connect(&folder, &display)
                    .ok()
                    .map(|source| Box::new(source) as Box<dyn FrameSource>),
            };
        }
    }

    /// An Ubuntu guest's session takes its input down its own port. A Linux the user installed has no
    /// such port, and QEMU's own keyboard and pointer reach its compositor through the guest's kernel.
    fn follow_forwarded(&self) {
        let mut forwarded = self.0.forwarded.borrow_mut();
        if forwarded.as_ref().is_some_and(|input| !input.alive()) {
            *forwarded = None;
        }
        let wanted = self.0.route.get() == FrameRoute::Forwarded && !self.0.booting.get();
        if !wanted {
            *forwarded = None;
        } else if forwarded.is_none() {
            let socket = self.0.run_folder.borrow().join(FORWARDED_INPUT_SOCKET);
            *forwarded = ForwardedInput::connect(&socket).ok();
        }
    }

    /// Picks up QEMU's display once it's listening, and a restarted one.
    fn follow_qemu(&self) {
        let mut qemu = self.0.qemu.borrow_mut();
        if qemu.as_ref().is_some_and(|source| !source.alive()) {
            *qemu = None;
        }
        let wanted = self.0.booting.get() || self.0.route.get() == FrameRoute::QemuDisplay;
        if qemu.is_none() && wanted {
            let monitor = self.0.run_folder.borrow().join(DISPLAY_SOCKET);
            if monitor.exists() {
                *qemu = Some(Box::new(DbusFrames::connect(monitor)));
            }
        }
    }

    fn follow_guest(&self) {
        let mut guest = self.0.guest.borrow_mut();
        if guest.as_ref().is_some_and(|found| !found.alive()) {
            *guest = None;
            self.0.booting.set(true);
        }
        if guest.is_none() {
            *guest = Guest::connect(&self.0.input_socket.borrow()).ok();
        }
        if let Some(guest) = guest.as_mut() {
            if guest.take_resets() > 0 {
                self.0.booting.set(true);
            }
            // The guest's driver puts its device first when it starts, and that can be at any time.
            guest.point_with(if self.captured() { MOUSE } else { TABLET });
        }
        drop(guest);
        self.follow_gpu();
        self.follow_qemu();
        self.follow_forwarded();
    }

    fn close_guest(&self) {
        self.0.guest.borrow_mut().take();
    }

    pub fn set_overlay(&self, wanted: bool) {
        self.0.overlay_wanted.set(wanted);
        self.follow_overlay();
    }

    /// Only a realized screen has one, as nothing else would stop it again.
    fn follow_overlay(&self) {
        let wanted = self.0.overlay_wanted.get() && self.0.root.is_realized();
        let mut overlay = self.0.overlay.borrow_mut();
        if wanted && overlay.is_none() {
            *overlay = Some(PerformanceOverlay::new(self));
        } else if !wanted {
            overlay.take();
        }
    }
}
