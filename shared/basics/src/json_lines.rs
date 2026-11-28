//! JSON lines: one value a line, read up to a limit and written compact. Each host keeps its own
//! limit and its own answer when a line isn't one it knows; the Windows host's app and service speak
//! this way on their pipe, with `control`'s `REQUEST_LIMIT`.

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Read, Write};

/// Reads one line of JSON, at most `limit` bytes long, as a `T`. Nothing read is `None`. A longer
/// line is cut at the limit, which leaves the rest to be read, and fails as bad data.
#[allow(clippy::disallowed_methods)] // the read is taken to the limit
pub fn read_line_within<T: for<'de> Deserialize<'de>>(
    reader: &mut impl BufRead,
    limit: usize,
) -> io::Result<Option<T>> {
    let mut line = String::new();
    let read = reader.by_ref().take(limit as u64).read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    serde_json::from_str(line.trim_end())
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Writes `value` as one line of JSON, and flushes it so the other end has it at once.
pub fn write_line<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests;
