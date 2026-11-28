//! Just enough HTTP/1.1 for one client on a pipe.

use crate::share::listing::http_date;
use guesthouse_basics::capped_line::read_capped_bytes;
use jiff::Timestamp;
use std::io::{self, BufRead, Read, Write, sink};

const SERVER: &str = "Guesthouse";
/// Longer than any length a file can have, and what an honest client sends fits.
const MOST_LENGTH_DIGITS: usize = 19;
const MOST_LINE: usize = 1 << 16;
/// Far more than Windows' WebDAV client sends, and what keeps a request's memory small.
const MOST_HEADERS: usize = 64;
const MOST_HEADER_BYTES: usize = 256 << 10;

pub struct Request {
    pub method: String,
    pub target: String,
    headers: Vec<(String, String)>,
    /// The client said this is its last request.
    pub close: bool,
}

fn read_line(input: &mut impl BufRead) -> io::Result<String> {
    let line = read_capped_bytes(input, MOST_LINE)?.unwrap_or_default();
    Ok(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_owned())
}

impl Request {
    /// The next request, or `None` when the client has gone or isn't speaking HTTP.
    pub fn read(input: &mut impl BufRead, output: &mut impl Write) -> io::Result<Option<Self>> {
        let line = read_line(input)?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let [method, target, version] = words[..] else { return Ok(None) };
        let (method, target) = (method.to_owned(), target.to_owned());
        let mut headers = Vec::new();
        let mut bytes = 0;
        loop {
            let line = read_line(input)?;
            let Some((name, value)) = line.split_once(':') else { break };
            bytes += line.len();
            if headers.len() == MOST_HEADERS || bytes > MOST_HEADER_BYTES {
                return Ok(None);
            }
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
        let mut request = Self { method, target, headers, close: version != "HTTP/1.1" };
        match request.header("Connection").map(str::to_ascii_lowercase).as_deref() {
            Some("close") => request.close = true,
            Some("keep-alive") => request.close = false,
            _ => {}
        }
        if request.header("Expect").is_some_and(|text| text.eq_ignore_ascii_case("100-continue")) {
            output.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            output.flush()?;
        }
        Ok(Some(request))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// What follows a length that isn't one can't be told from a body.
    pub fn length_is_valid(&self) -> bool {
        self.header("Content-Length").is_none_or(|text| {
            !text.is_empty()
                && text.len() <= MOST_LENGTH_DIGITS
                && text.bytes().all(|byte| byte.is_ascii_digit())
        })
    }

    pub fn length(&self) -> u64 {
        self.header("Content-Length").and_then(|text| text.parse().ok()).unwrap_or(0)
    }

    pub fn discard_body(&self, input: &mut impl BufRead) -> io::Result<()> {
        io::copy(&mut input.by_ref().take(self.length()), &mut sink()).map(|_| ())
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        _ => "Not Implemented",
    }
}

/// The status line and headers.
pub fn headers(output: &mut impl Write, code: u16, fields: &[(&str, &str)]) -> io::Result<()> {
    write!(output, "HTTP/1.1 {code} {}\r\n", reason(code))?;
    write!(output, "Server: {SERVER}\r\nDate: {}\r\n", http_date(Timestamp::now().as_second()))?;
    for (name, value) in fields {
        write!(output, "{name}: {value}\r\n")?;
    }
    output.write_all(b"\r\n")
}

/// A whole reply.
pub fn reply(output: &mut impl Write, code: u16, body: &[u8], fields: &[(&str, &str)]) -> io::Result<()> {
    let size = body.len().to_string();
    let mut all = fields.to_vec();
    all.push(("Content-Length", &size));
    headers(output, code, &all)?;
    output.write_all(body)
}
