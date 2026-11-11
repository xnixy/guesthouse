//! What a tester's log holds after a guest's first steps: one line for each kind of exchange, each
//! starting with `intel` and the kind, and a busy kind cut down to its first few lines.

mod common;

use common::*;
use guesthouse_backend_intel::protocol::{escape, gaps, record};
use guesthouse_messages::wire::{set_u32, set_u64};
use guesthouse_messages::{Command, HEADER_SIZE};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::sync::Mutex;

/// Keeps every line at info and above, as the daemon writes them without `GUESTHOUSE_VERBOSE`.
struct Kept(Mutex<Vec<String>>);

impl Log for Kept {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

static KEPT: Kept = Kept(Mutex::new(Vec::new()));

const SUBMISSIONS: u64 = 10;

fn lines_starting(lines: &[String], start: &str) -> usize {
    lines.iter().filter(|line| line.starts_with(start)).count()
}

// One test, as the logger is the whole program's.
#[test]
fn every_kind_of_exchange_has_its_line() {
    log::set_logger(&KEPT).unwrap();
    log::set_max_level(LevelFilter::Info);

    let mut session = guest();
    let mut open = message(Command::OpenAdapter, HEADER_SIZE + 16);
    set_u64(&mut open, HEADER_SIZE + 8, 0x6949);
    send(&mut session, &open);
    query(&mut session, record::QUERY_TYPE, &[0; record::COUNT_SIZE]);
    query(&mut session, record::QUERY_TYPE, &vec![0; record::RECORD_SIZE]);
    query(&mut session, 1, &DIRECT3D_12.to_le_bytes());
    query(&mut session, 77, &[0; 16]);
    let context = context(&mut session, 0, &direct3d_12_context());
    let (queue, _) = queue(&mut session, context);
    allocate(&mut session, &render_target());
    allocate(&mut session, &[0; 0x20]);
    for value in 1..=SUBMISSIONS {
        send(&mut session, &submission(queue, value, &direct3d_12_header()));
    }
    let mut data = vec![0u8; escape::HEADER_SIZE + 8];
    set_u32(&mut data, 0, 8);
    set_u32(&mut data, 8, escape::GMM_CONTROL);
    let mut request = message(Command::DriverEscape, HEADER_SIZE + 24);
    set_u32(&mut request, HEADER_SIZE + 16, data.len() as u32);
    request.extend_from_slice(&data);
    send(&mut session, &request);

    let lines = KEPT.0.lock().unwrap().clone();
    for (start, count) in [
        ("intel card pci=8086:64a0", 1),
        ("intel record product_family=1275 render_core=0xc09", 1),
        ("intel record-gap ", gaps::RECORD.len()),
        ("intel adapter-opened guest_id=0x6949", 1),
        ("intel query kind=0 size=4 wow64=0 answered=adapter-count", 1),
        ("intel query kind=0 size=9588 wow64=0 answered=record", 1),
        ("intel query kind=1 size=4 wow64=0 answered=driver-name driver=igd12umd64.dll", 1),
        ("intel query kind=77 size=16 wow64=0 answered=left-to-the-core", 1),
        ("intel context process=0x40002000 node=0 size=144 type=7 guest_pid=4242", 1),
        ("intel queue id=", 1),
        ("intel allocation size=2352 type=2", 1),
        ("intel refused what=allocation", 1),
        ("intel escape process=0x40002000 length=24 payload_size=8 checksum=0x0 code=0x2", 1),
        // The same submission ten times is one kind, so only its first few lines are at info.
        ("intel submit queue=", 3),
    ] {
        assert_eq!(lines_starting(&lines, start), count, "{start}\n{}", lines.join("\n"));
    }
    let submit_lines: Vec<_> = lines.iter().filter(|line| line.starts_with("intel submit")).collect();
    assert!(submit_lines[0].contains("form=compact stamp=0x23d7 type=7"), "{}", submit_lines[0]);
    assert!(submit_lines[0].contains("answer=finished-unrun"), "{}", submit_lines[0]);
}
