//! Lines read from a peer that is not trusted to keep them short: a helper in a guest, or QEMU.

use std::io::{self, BufRead, ErrorKind, Read};

/// Reads one line as raw bytes, its line end included, of at most `most` bytes besides the end.
/// `Ok(None)` is the end of the stream. A longer line is an `InvalidData` error and nothing more
/// than `most + 1` bytes were held.
#[allow(clippy::disallowed_methods)] // this is the capped read the rest use
pub fn read_capped_bytes(reader: &mut impl BufRead, most: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(most as u64 + 1).read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.last() != Some(&b'\n') && bytes.len() > most {
        return Err(io::Error::new(ErrorKind::InvalidData, "a line longer than allowed"));
    }
    Ok(Some(bytes))
}

/// Reads one line of at most `most` bytes, without its line end. `Ok(None)` is the end of the
/// stream. A longer line, or one that is not UTF-8, is an `InvalidData` error and nothing more than
/// `most` bytes were held, so the caller drops that connection and carries on.
pub fn read_capped_line(reader: &mut impl BufRead, most: usize) -> io::Result<Option<String>> {
    let Some(mut bytes) = read_capped_bytes(reader, most)? else { return Ok(None) };
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "a line that is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// A stream with no end and no line end, which only stops being read when the reader stops.
    struct Endless;

    impl Read for Endless {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            buffer.fill(b'a');
            Ok(buffer.len())
        }
    }

    #[test]
    fn a_line_that_never_ends_is_refused_after_the_limit() {
        let mut reader = BufReader::new(Endless);
        let error = read_capped_line(&mut reader, 1000).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn lines_up_to_the_limit_are_read_one_at_a_time() {
        let mut reader = BufReader::new(&b"abc\r\nxyz\n"[..]);
        assert_eq!(read_capped_line(&mut reader, 5).unwrap().as_deref(), Some("abc\r"));
        assert_eq!(read_capped_line(&mut reader, 3).unwrap().as_deref(), Some("xyz"));
        assert_eq!(read_capped_line(&mut reader, 3).unwrap(), None);
        let mut reader = BufReader::new(&b"abcd\n"[..]);
        assert!(read_capped_line(&mut reader, 3).is_err());
    }
}
