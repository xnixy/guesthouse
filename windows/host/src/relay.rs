//! `guesthouse-serial-relay`: reads what a guest writes to its serial port on standard input, and
//! writes it to standard output as printable lines. `boot` runs it in the jail, as it's the one
//! part of Guesthouse on a Windows host that reads bytes a guest chose. It needs nothing else: no
//! files, no network and no other process.

use guesthouse_host_windows::printable::{LINE_LIMIT, printable};
use std::io::{self, BufRead, Read, Write};

#[allow(clippy::disallowed_methods)] // each read is taken to LINE_LIMIT, and a longer line goes on as the next
fn main() {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        // `take` so a guest that never ends a line can't make this hold more than the limit.
        let read = (&mut input).take(LINE_LIMIT as u64 + 1).read_until(b'\n', &mut line);
        match read {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if writeln!(output, "{}", printable(&line)).and_then(|()| output.flush()).is_err() {
                    return;
                }
            }
        }
    }
}
