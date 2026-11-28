// SPDX-License-Identifier: GPL-2.0-or-later
//! QEMU played by hand for the built program: this end of the socket says what QEMU's forwarding
//! device says, and the guest's memory is a shared memory object made here. A guest's driver is
//! played as far as a picture of one colour on the display, swapped to as often as a test likes.
//! It calls Apple's framework through the program, so it is a Mac's, and runs where the build has
//! a profile. Shared by the tests beside it, each of which uses part of it.
#![allow(dead_code)]
#![allow(clippy::disallowed_methods)] // a test's own sockets

#[path = "../../src/wire.rs"]
#[allow(dead_code)]
mod wire;

use guesthouse_pvg_sys::profile::{PAGE_BYTES, PAGE_SHIFT};
use guesthouse_pvg_sys::{Profile, host_build};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
pub use wire::{Access, Answer, BAR_DEVICE, BAR_MAPPER, MESSAGE_BYTES, PROTOCOL, Request};

const MEMORY_AT: u64 = 0x4000_0000;
const MEMORY_BYTES: usize = 32 << 20;
// Where this guest keeps things, as offsets into its memory.
const ROOT_PAGE: u64 = 0x10_0000;
const TASK_ROOT: u64 = 0x12_0000;
const STATE_PAGE: u64 = 0x14_0000;
const MAPPER_RING: u64 = 0x18_0000;
const MAPPER_TABLE: u64 = 0x19_0000;
const DESCRIPTORS: u64 = 0x1A_0000;
const PAGE_LIST: u64 = 0x1A_4000;
const RING: u64 = 0x20_0000;
const SURFACE: u64 = 0x30_0000;

const RING_BYTES: u32 = 0x1_0000;
const COMMANDS_AT: u32 = 0x1000;
/// The bytes commands lie in, after the ring's header.
const RING_ROOM: u32 = RING_BYTES - COMMANDS_AT;
pub const WIDTH: u32 = 320;
pub const HEIGHT: u32 = 200;
pub const COLOUR: u32 = 0xFF10_8040;
pub const PATIENCE: Duration = Duration::from_secs(5);
/// The modes the program is started with; the guest's state page gets as many.
const MODES: &str = "640x480@60,320x200@60";
const MODE_COUNT: u32 = 2;
/// The command that swaps the display to a surface, and the surface's mapping id.
const SWAP: u32 = 8;
const SURFACE_ID: u32 = 1;

/// The guest's memory: a shared memory object nobody can open by name, mapped here.
pub struct Memory {
    pub file: OwnedFd,
    start: *mut u8,
}

impl Memory {
    pub fn new() -> Self {
        let name = std::ffi::CString::new(format!("/pvg-host-test-{}", std::process::id())).expect("a name");
        // SAFETY: the name ends in a zero; the object is new, and unlinked at once.
        let descriptor =
            unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
        assert!(descriptor >= 0, "a shared memory object");
        // SAFETY: the descriptor is open and nothing else holds it.
        let file = unsafe { OwnedFd::from_raw_fd(descriptor) };
        // SAFETY: the name ends in a zero, and the calls touch only the object just made.
        let start = unsafe {
            libc::shm_unlink(name.as_ptr());
            assert_eq!(libc::ftruncate(file.as_raw_fd(), MEMORY_BYTES as libc::off_t), 0);
            let protection = libc::PROT_READ | libc::PROT_WRITE;
            libc::mmap(std::ptr::null_mut(), MEMORY_BYTES, protection, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        assert_ne!(start, libc::MAP_FAILED);
        Self { file, start: start.cast() }
    }

    pub fn put(&self, offset: u64, words: &[u32]) {
        assert!(offset as usize + words.len() * 4 <= MEMORY_BYTES);
        for (index, word) in words.iter().enumerate() {
            // SAFETY: inside the mapping, by the check above; the other process may read it
            // meanwhile, which is the point.
            unsafe { self.start.add(offset as usize + index * 4).cast::<u32>().write_volatile(word.to_le()) };
        }
    }

    pub fn word(&self, offset: u64) -> u32 {
        assert!(offset as usize + 4 <= MEMORY_BYTES);
        // SAFETY: inside the mapping, by the check above.
        u32::from_le(unsafe { self.start.add(offset as usize).cast::<u32>().read_volatile() })
    }
}

fn page(offset: u64) -> u32 {
    ((MEMORY_AT + offset) >> PAGE_SHIFT) as u32
}

/// How the mapper names a page: its number shifted left by two, with the lowest bit for "there".
fn mapper_page(offset: u64) -> u32 {
    page(offset) << 2 | 1
}

/// QEMU's end of the socket.
pub struct Qemu {
    pub stream: UnixStream,
    /// The vectors of the interrupts that came, in order.
    pub interrupts: Vec<u32>,
}

impl Qemu {
    /// Sends a message with a file beside it, as QEMU sends a range of guest memory.
    fn send_with_file(&mut self, message: &[u8; MESSAGE_BYTES], file: &OwnedFd) {
        let mut control = [0u64; 8];
        let mut part = libc::iovec { iov_base: message.as_ptr().cast_mut().cast(), iov_len: MESSAGE_BYTES };
        // SAFETY: all zeroes is a valid `msghdr`; the fields that matter are set below.
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &raw mut part;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        // SAFETY: the control buffer has room for one descriptor's message, which is laid out in
        // it by the system's own macros, and all of it lives through the call.
        let sent = unsafe {
            header.msg_controllen = libc::CMSG_SPACE(4) as _;
            let entry = libc::CMSG_FIRSTHDR(&raw const header);
            (*entry).cmsg_level = libc::SOL_SOCKET;
            (*entry).cmsg_type = libc::SCM_RIGHTS;
            (*entry).cmsg_len = libc::CMSG_LEN(4) as _;
            libc::CMSG_DATA(entry).cast::<libc::c_int>().write_unaligned(file.as_raw_fd());
            libc::sendmsg(self.stream.as_raw_fd(), &raw const header, 0)
        };
        assert_eq!(sent, MESSAGE_BYTES as isize);
    }

    pub fn receive(&mut self) -> Answer {
        let mut message = [0u8; MESSAGE_BYTES];
        self.stream.read_exact(&mut message).expect("the program answers");
        Answer::read(&message).expect("an answer QEMU knows")
    }

    /// Sends a request and waits for its reply, noting the interrupts that come first.
    pub fn call(&mut self, request: Request) -> u64 {
        self.stream.write_all(&request.write()).expect("the program listens");
        loop {
            match self.receive() {
                Answer::Reply(value) => return value,
                Answer::Interrupt(vector) => self.interrupts.push(vector),
                Answer::Ready { .. } => panic!("ready twice"),
            }
        }
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        self.call(Request::Read(Access { bar: BAR_DEVICE, size: 4, offset })) as u32
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        self.call(Request::Write(Access { bar: BAR_DEVICE, size: 4, offset }, u64::from(value)));
    }

    pub fn mapper_read(&mut self, offset: u64) -> u64 {
        self.call(Request::Read(Access { bar: BAR_MAPPER, size: 8, offset }))
    }

    pub fn mapper_write(&mut self, offset: u64, value: u64) {
        self.call(Request::Write(Access { bar: BAR_MAPPER, size: 8, offset }, value));
    }
}

/// A guest's first ring, written into its memory by hand.
struct Ring {
    written: u32,
    stamp: u32,
}

impl Ring {
    /// Sends one command and waits until the device has stamped it.
    fn command(&mut self, memory: &Memory, qemu: &mut Qemu, number: u32, payload: &[u32]) {
        let length = (3 + payload.len() as u32) * 4;
        self.stamp += 1;
        let words = [number, length, self.stamp].into_iter().chain(payload.iter().copied());
        // The written count runs on, and the ring wraps word by word past its room, as a
        // driver's does: a test that swaps for long goes round it many times.
        for (index, word) in words.enumerate() {
            let place = self.written.wrapping_add(index as u32 * 4) % RING_ROOM;
            memory.put(RING + u64::from(COMMANDS_AT + place), &[word]);
        }
        self.written = self.written.wrapping_add(length);
        qemu.write(0x1008, self.written);
        let since = Instant::now();
        while memory.word(RING) != self.stamp {
            assert_eq!(qemu.read(0x102C), 0, "command {number} was refused");
            assert!(since.elapsed() < PATIENCE, "command {number} was never stamped");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Ends the program if the test leaves early, and removes what the test made.
pub struct Program {
    pub child: Child,
    pub folder: PathBuf,
}

impl Drop for Program {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.folder);
    }
}

pub fn wait_for(path: &Path) {
    let since = Instant::now();
    while !path.exists() {
        assert!(since.elapsed() < PATIENCE, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn ask(control: &Path, line: &str) -> String {
    let mut stream = UnixStream::connect(control).expect("the control socket");
    writeln!(stream, "{line}").expect("the line goes out");
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).expect("an answer");
    answer.trim().to_owned()
}

/// The program, QEMU's end of its socket, and the guest's memory and first ring.
pub struct Guest {
    pub qemu: Qemu,
    pub memory: Memory,
    ring: Ring,
    pub control: PathBuf,
    /// Last, so the program ends after QEMU's end of the socket has closed.
    pub program: Program,
}

/// Whether this macOS build has a profile, which the device needs. A test without one has
/// nothing to run.
pub fn runnable() -> bool {
    let known = host_build().is_some_and(|build| Profile::for_build(&build).is_ok());
    if !known {
        eprintln!("this macOS build has no profile: nothing was run");
    }
    known
}

/// Starts the program with these arguments besides its sockets and modes, and plays QEMU and a
/// guest's driver up to a surface of [`COLOUR`] the display can swap to. Nothing is shown yet.
pub fn start(arguments: &[&str]) -> Guest {
    // A socket's path may be 103 bytes, and the system's own temporary folder is long already.
    let folder = PathBuf::from(format!("/tmp/pvg-host-{}", std::process::id()));
    fs::create_dir_all(&folder).expect("a folder for the sockets");
    let (socket, control) = (folder.join("qemu.sock"), folder.join("control.sock"));
    let child = Command::new(env!("CARGO_BIN_EXE_guesthouse-pvg-host"))
        .args(["--socket", socket.to_str().expect("a path"), "--control", control.to_str().expect("a path")])
        .args(["--modes", MODES])
        .args(arguments)
        .spawn()
        .expect("the program starts");
    let program = Program { child, folder };
    wait_for(&socket);

    // QEMU's greeting: one range of guest memory, with its file.
    let memory = Memory::new();
    let mut qemu =
        Qemu { stream: UnixStream::connect(&socket).expect("the program listens"), interrupts: Vec::new() };
    qemu.stream.set_read_timeout(Some(PATIENCE)).expect("a timeout");
    qemu.stream.write_all(&Request::Hello { protocol: PROTOCOL, ranges: 1 }.write()).expect("the greeting");
    let range = Request::Range { guest_address: MEMORY_AT, length: MEMORY_BYTES as u64, file_offset: 0 };
    qemu.send_with_file(&range.write(), &memory.file);
    let Answer::Ready { device_bytes, mapper_bytes } = qemu.receive() else {
        panic!("the program is not ready");
    };
    assert!(
        device_bytes.is_power_of_two() && mapper_bytes.is_power_of_two(),
        "{device_bytes:#x}, {mapper_bytes:#x}"
    );
    qemu.call(Request::Reset);

    // The guest's driver: the mapper's ring, and a surface of one colour given a mapping id.
    qemu.mapper_write(0x1000, MEMORY_AT + MAPPER_RING);
    qemu.mapper_write(0x1008, 0x4010);
    assert_eq!(qemu.mapper_read(0x1008), 0, "a ring length that would end the program is not passed on");
    qemu.mapper_write(0x1008, 1024);
    qemu.mapper_write(0x1010, MEMORY_AT + MAPPER_TABLE);
    let mut guest = Guest { qemu, memory, ring: Ring { written: 0, stamp: 0x100 }, control, program };
    guest.paint(COLOUR);
    let bytes = surface_bytes();
    let memory = &guest.memory;
    let descriptor = DESCRIPTORS + 0x200;
    memory.put(descriptor, &[mapper_page(PAGE_LIST), u32::from_be_bytes(*b"BGRA")]);
    memory.put(descriptor + 0x10, &[bytes as u32, WIDTH << 8 | 1, HEIGHT << 8 | 1, WIDTH * 4, 4]);
    let pages: Vec<u32> =
        (0..bytes / PAGE_BYTES).map(|index| mapper_page(SURFACE + index * PAGE_BYTES)).collect();
    memory.put(PAGE_LIST, &pages);
    memory.put(MAPPER_TABLE, &[mapper_page(DESCRIPTORS)]);
    memory.put(MAPPER_RING, &[1, SURFACE_ID, u32::MAX, 0]);
    let qemu = &mut guest.qemu;
    qemu.mapper_write(0x1018, 1);
    let since = Instant::now();
    while qemu.mapper_read(0x1020) < 1 {
        assert!(since.elapsed() < PATIENCE, "the mapper never took its command");
    }
    assert_eq!(memory.word(MAPPER_RING + 8), 0, "the mapper took the surface");
    // The mapper counts the command as done a moment before its interrupt is on the socket.
    while !qemu.interrupts.contains(&1) {
        qemu.mapper_read(0x1020);
        assert!(since.elapsed() < PATIENCE, "the mapper's interrupt never came on its own vector");
    }

    // The first ring, the kernel task and the display's state page.
    qemu.write(0x1034, 252);
    assert_eq!(qemu.read(0x1034), 252);
    qemu.write(0x101C, page(ROOT_PAGE));
    qemu.write(0x1030, page(RING));
    qemu.write(0x1004, RING_BYTES);
    qemu.write(0x1010, COMMANDS_AT);
    qemu.write(0x1000, 1);
    memory.put(TASK_ROOT, &[0, 1]);
    guest.ring.command(&guest.memory, &mut guest.qemu, 56, &[0, 0x10_0000, 0, page(TASK_ROOT)]);
    guest.ring.command(&guest.memory, &mut guest.qemu, 1, &[0, page(STATE_PAGE)]);
    assert_eq!(
        guest.memory.word(STATE_PAGE + 0x208),
        MODE_COUNT,
        "the device wrote its modes into the guest's page"
    );
    guest
}

/// The surface's bytes, in whole pages.
fn surface_bytes() -> u64 {
    u64::from(WIDTH * HEIGHT * 4).next_multiple_of(PAGE_BYTES)
}

impl Guest {
    /// Fills the guest's surface with one colour, four bytes a pixel, blue first.
    pub fn paint(&mut self, colour: u32) {
        let words = vec![colour; (surface_bytes() / 4) as usize];
        self.memory.put(SURFACE, &words);
    }

    /// Has the display show the surface, as the guest's driver swaps to a new picture.
    pub fn swap(&mut self) {
        self.ring.command(&self.memory, &mut self.qemu, SWAP, &[0, 0, SURFACE_ID, 0, 0, 0, 0, 0, 0, 0]);
    }
}
