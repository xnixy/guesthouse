//! The sockets QEMU connects to, and the loop that serves them.
//!
//! One thread does everything. That is what keeps the message path free of locks, and it gives the
//! session one order for the messages of both channels.

use crate::display_note::{self, ScreenNote};
use crate::gpu_note::write_note;
use crate::router::{Channel, FrameSink, Router};
use crate::viewer::ViewerPort;
use guesthouse_backend::{Backend, GuestMemory};
use guesthouse_core::Waiting;
use guesthouse_messages::indirect::Look;
use guesthouse_transport::{FrameHeader, FrameStream};
use std::fs::{self, Permissions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

/// How long to go on looking at fences without sleeping, once the guest is waiting on one. A sleep
/// costs tens of microseconds even at its best, so a wait shorter than this costs a core and no time.
const SPIN_LIMIT: Duration = Duration::from_micros(200);
/// How often to look at fences after that. The GPU gives no signal this can sleep on.
const FENCE_POLL: Duration = Duration::from_micros(20);
/// How often to look when only the GPU's health is being watched.
const BACKGROUND_POLL: Duration = Duration::from_millis(1);
/// How often to log where submissions spend their time, when asked to measure that.
const TIMING_REPORT: Duration = Duration::from_secs(10);
/// How long every channel can stay closed before QEMU counts as gone. A QEMU that's alive connects
/// again within half a second.
const QEMU_GONE_AFTER: Duration = Duration::from_secs(5);
/// The kernel may stretch a sleep by this much, which by default is more than the sleep itself.
const TIMER_SLACK_NANOS: libc::c_ulong = 1;
const NO_DESCRIPTOR: i32 = -1;
/// Each channel has two places in a wait, and so does the viewer, after them.
const CHANNEL_PLACES: usize = 2 * Channel::ALL.len();
const VIEWER_LISTENER: usize = CHANNEL_PLACES;
const VIEWER_SOCKET: usize = CHANNEL_PLACES + 1;
/// The descriptor the GPU's own interrupts post to, when the backend has one. Its readiness needs
/// no dealing-with: whatever it woke the loop for is re-read by the poll below, as every tick
/// re-reads it.
const GPU_WAKE: usize = CHANNEL_PLACES + 2;
const ALL_PLACES: usize = CHANNEL_PLACES + 3;
/// QEMU runs as the same user, and nobody else has business with these.
pub(crate) const OWNER_ONLY: u32 = 0o600;

struct Connection {
    stream: UnixStream,
    frames: FrameStream,
}

struct Port {
    channel: Channel,
    listener: UnixListener,
    connection: Option<Connection>,
}

/// Writes through whatever connections are up. Without one the frame has nowhere to go, as when QEMU
/// has left, and is dropped.
struct Connections<'a> {
    ports: &'a mut [Port],
    scratch: &'a mut Vec<u8>,
}

impl FrameSink for Connections<'_> {
    fn send(&mut self, channel: Channel, header: FrameHeader, payload: &[u8]) -> io::Result<()> {
        let port =
            self.ports.iter_mut().find(|port| port.channel == channel).expect("every channel has a port");
        let Some(connection) = &mut port.connection else { return Ok(()) };
        // One write, so a frame is one system call and the two parts cannot be separated.
        self.scratch.clear();
        self.scratch.extend_from_slice(&header.encode());
        self.scratch.extend_from_slice(payload);
        connection.stream.write_all(self.scratch)
    }
}

/// One VM's sockets and session.
pub struct Server<B, M> {
    router: Router<B, M>,
    ports: Vec<Port>,
    viewers: ViewerPort,
    waiting: Waiting,
    urgent_since: Option<Instant>,
    /// The descriptor the GPU's interrupts post to, when the backend has one.
    wake: Option<i32>,
    scratch: Vec<u8>,
    /// QEMU has connected at some point, so a closed channel means something.
    qemu_seen: bool,
    /// When the last channel closed, if they're all closed.
    all_closed_since: Option<Instant>,
    qemu_gone_after: Duration,
    run_folder: PathBuf,
    /// What the app has been told about the guest's GPU work.
    said_stopped: bool,
    /// What the app has been told about the display driver's screen.
    said_screen: Option<ScreenNote>,
}

impl<B: Backend, M: GuestMemory> Server<B, M> {
    /// Listens in `run_folder`, replacing sockets a previous daemon left behind.
    pub fn bind(run_folder: &Path, router: Router<B, M>) -> io::Result<Self> {
        fs::create_dir_all(run_folder)?;
        let mut ports = Vec::new();
        for channel in Channel::ALL {
            let path = run_folder.join(channel.socket_name());
            match fs::remove_file(&path) {
                Err(error) if error.kind() != ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            let listener = UnixListener::bind(&path)?;
            fs::set_permissions(&path, Permissions::from_mode(OWNER_ONLY))?;
            ports.push(Port { channel, listener, connection: None });
        }
        let viewers = ViewerPort::bind(run_folder)?;
        write_note(run_folder, false)?;
        display_note::write_note(run_folder, None)?;
        Ok(Self {
            router,
            ports,
            viewers,
            waiting: Waiting::Nothing,
            urgent_since: None,
            wake: None,
            scratch: Vec::new(),
            qemu_seen: false,
            all_closed_since: None,
            qemu_gone_after: QEMU_GONE_AFTER,
            run_folder: run_folder.to_owned(),
            said_stopped: false,
            said_screen: None,
        })
    }

    /// Whether QEMU counts as gone at this moment, for a test.
    pub fn qemu_is_gone_now(&mut self) -> bool {
        self.qemu_has_gone(Instant::now())
    }

    /// For a test, which has no reason to wait.
    pub fn set_qemu_gone_after(&mut self, time: Duration) {
        self.qemu_gone_after = time;
    }

    pub fn router(&self) -> &Router<B, M> {
        &self.router
    }

    pub fn router_mut(&mut self) -> &mut Router<B, M> {
        &mut self.router
    }

    /// Serves until QEMU has gone, however it went, or an error nothing can recover from. Ending is
    /// what gives everything back: the guest's memory, and all it held on the GPU.
    pub fn run(&mut self) -> io::Result<()> {
        // SAFETY: this changes one number the kernel keeps for this thread.
        unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, TIMER_SLACK_NANOS) };
        let mut last_report = Instant::now();
        loop {
            let now = Instant::now();
            let timeout = self.next_timeout(now);
            self.step(timeout)?;
            if self.qemu_has_gone(Instant::now()) {
                log::info!("QEMU has gone, so this VM is over");
                return Ok(());
            }
            let timings = self.router.session().timings();
            if timings.is_enabled() && now - last_report > TIMING_REPORT {
                log::info!("where submissions spend their time:\n{timings}");
                log::info!("kept: {:?}", self.router.session().table_sizes());
                log::info!("frames: {:?}", self.router.session().frame_counts());
                last_report = now;
            }
        }
    }

    /// Notes which channels are open, and says whether they've all been closed for long enough.
    fn qemu_has_gone(&mut self, now: Instant) -> bool {
        if self.ports.iter().any(|port| port.connection.is_some()) {
            self.all_closed_since = None;
        } else if self.qemu_seen {
            self.all_closed_since.get_or_insert(now);
        }
        self.all_closed_since.is_some_and(|since| now - since >= self.qemu_gone_after)
    }

    /// How long to wait for the next message: what's waiting on fences, or the end of the wait for QEMU.
    fn next_timeout(&mut self, now: Instant) -> Option<Duration> {
        let for_fences = self.fence_timeout(now);
        let Some(since) = self.all_closed_since else { return for_fences };
        let left = self.qemu_gone_after.saturating_sub(now - since);
        Some(for_fences.map_or(left, |timeout| timeout.min(left)))
    }

    fn fence_timeout(&mut self, now: Instant) -> Option<Duration> {
        match self.waiting {
            Waiting::Nothing => {
                self.urgent_since = None;
                None
            }
            Waiting::Background => {
                self.urgent_since = None;
                Some(BACKGROUND_POLL)
            }
            Waiting::Urgent => {
                let since = *self.urgent_since.get_or_insert(now);
                if now - since < SPIN_LIMIT {
                    return Some(Duration::ZERO);
                }
                // With the GPU's interrupts to wake the loop, a fence is looked for when something
                // says it may have moved, and at this pace otherwise. Without them, the old tight
                // cadence stands.
                Some(if self.wake.is_some() { BACKGROUND_POLL } else { FENCE_POLL })
            }
        }
    }

    /// Waits for something to happen, for at most `timeout` if given, and deals with it.
    pub fn step(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        // Each port has two places, its listener and its connection. Poll skips a negative descriptor.
        let mut descriptors = [readable(NO_DESCRIPTOR); ALL_PLACES];
        for (port, pair) in self.ports.iter().zip(descriptors.as_chunks_mut::<2>().0) {
            pair[0].fd = port.listener.as_raw_fd();
            pair[1].fd = port.connection.as_ref().map_or(NO_DESCRIPTOR, |open| open.stream.as_raw_fd());
        }
        let (listener, viewer, blocked) = self.viewers.wait_on();
        descriptors[VIEWER_LISTENER].fd = listener;
        descriptors[VIEWER_SOCKET].fd = viewer;
        // The copy channel's surface is made lazily, so the descriptor is asked for every step.
        self.wake = self.router.session_mut().wake_descriptor();
        if let Some(fd) = self.wake {
            descriptors[GPU_WAKE].fd = fd;
            descriptors[GPU_WAKE].events = libc::POLLIN | libc::POLLPRI;
        }
        if blocked {
            descriptors[VIEWER_SOCKET].events |= libc::POLLOUT;
        }
        wait(&mut descriptors, timeout)?;
        for (index, pair) in descriptors[..CHANNEL_PLACES].as_chunks::<2>().0.iter().enumerate() {
            if pair[1].revents != 0 {
                self.read_channel(index);
            }
            if pair[0].revents != 0 {
                let (stream, _) = self.ports[index].listener.accept()?;
                log::info!("QEMU connected to {:?}", self.ports[index].channel);
                self.qemu_seen = true;
                self.ports[index].connection = Some(Connection { stream, frames: FrameStream::new() });
            }
        }
        // The guest's channels went first, and the viewer's trouble is only ever the viewer's.
        let session = self.router.session_mut();
        if descriptors[VIEWER_SOCKET].revents != 0 {
            self.viewers.read(session);
        }
        if descriptors[VIEWER_LISTENER].revents != 0 {
            self.viewers.accept(session);
        }
        let mut sink = Connections { ports: &mut self.ports, scratch: &mut self.scratch };
        self.waiting = self.router.poll(Instant::now(), &mut sink)?;
        self.router.session_mut().offer_frames(&mut self.viewers);
        self.keep_the_app_told();
        Ok(())
    }

    /// A note that can't be written costs the user the reason, and nothing else.
    fn keep_the_app_told(&mut self) {
        let stopped = self.router.session().gpu_work_stopped();
        if stopped != self.said_stopped {
            self.said_stopped = stopped;
            if let Err(error) = write_note(&self.run_folder, stopped) {
                log::warn!("writing the GPU note for the app: {error}");
            }
        }
        let screen = self.router.session().indirect_screen().map(|(picture, look, frames)| ScreenNote {
            width: picture.width,
            height: picture.height,
            hdr: look == Look::HDR,
            daemon: std::process::id(),
            frames,
        });
        if screen != self.said_screen {
            self.said_screen = screen;
            if let Err(error) = display_note::write_note(&self.run_folder, screen) {
                log::warn!("writing the display note for the app: {error}");
            }
        }
    }

    /// Reads and handles what a channel has. A channel that fails closes, and QEMU can connect again.
    fn read_channel(&mut self, index: usize) {
        let channel = self.ports[index].channel;
        let Some(connection) = &mut self.ports[index].connection else { return };
        let mut open = match connection.frames.fill_from(&mut connection.stream) {
            Ok(0) => false,
            Ok(_) => true,
            Err(error) => {
                log::warn!("{channel:?}: {error}");
                false
            }
        };
        // Taken out for the loop, as replies need the ports while a frame borrows from this.
        let mut frames = std::mem::take(&mut connection.frames);
        while open && let Some(frame) = frames.next_frame() {
            let mut sink = Connections { ports: &mut self.ports, scratch: &mut self.scratch };
            if let Err(error) = self.router.handle_frame(channel, frame, &mut sink) {
                log::warn!("{channel:?}: {error}");
                open = false;
            }
        }
        match &mut self.ports[index].connection {
            Some(connection) if open => connection.frames = frames,
            connection => {
                log::info!("{channel:?}: connection closed");
                *connection = None;
            }
        }
    }
}

fn readable(fd: i32) -> libc::pollfd {
    libc::pollfd { fd, events: libc::POLLIN, revents: 0 }
}

fn wait(descriptors: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<()> {
    let time = timeout.map(|timeout| libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: libc::c_long::from(timeout.subsec_nanos() as i32),
    });
    let time = time.as_ref().map_or(ptr::null(), ptr::from_ref);
    // SAFETY: both pointers are valid for the call, and the length is the slice's own.
    let count = unsafe {
        libc::ppoll(descriptors.as_mut_ptr(), descriptors.len() as libc::nfds_t, time, ptr::null())
    };
    if count < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}
