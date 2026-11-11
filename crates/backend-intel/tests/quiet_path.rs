//! A steady flow of submissions must not touch the allocator, with every line this backend logs
//! about them formatted, as the daemon does for the first few of a kind and for all of them with
//! `GUESTHOUSE_VERBOSE`. The core's own test holds the core to this; this one holds the Intel
//! backend's reading and logging of each submission to it.

mod common;

use common::*;
use guesthouse_messages::wire::set_u64;
use log::{LevelFilter, Log, Metadata, Record};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: passes everything to the system allocator and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's promises are the ones the system allocator needs.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Formats every line, as the daemon's logger does, into nothing.
struct Formatting;

impl Log for Formatting {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        let _ = writeln!(std::io::sink(), "{:<5} {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

static FORMATTING: Formatting = Formatting;

const WARM_UP: u64 = 100;
const ROUNDS: u64 = 1_000;

// One test, as the counter is the whole program's.
#[test]
fn submissions_do_not_allocate_with_every_line_formatted() {
    log::set_logger(&FORMATTING).unwrap();
    log::set_max_level(LevelFilter::Debug);
    let mut session = guest();
    let context = context(&mut session, 0, &context_data());
    let (queue, _) = queue(&mut session, context);
    let mut out = Vec::with_capacity(4096);
    let mut work = submission(queue, 1, &compact_header());

    let mut run = |round: u64| {
        set_u64(&mut work, 0x20, round + 1);
        let _ = session.handle_message(&work, TICKET, &mut out);
    };
    (0..WARM_UP).for_each(&mut run);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    (WARM_UP..WARM_UP + ROUNDS).for_each(&mut run);
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed) - before, 0);
}
