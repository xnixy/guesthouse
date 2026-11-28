//! The file a machine is carried to another PC in: a line that says what it is, then each file as a
//! line of JSON with its path and size, its bytes, and a line with their SHA-256. A last line says
//! that's all, so a file cut short is told from a whole one.
//!
//! It's read as untrusted. Each line has a most it can be, each path must be a plain one inside the
//! machine's folder, and each file's bytes must be as many as it said and hash to what it said.
//!
//! The Linux host exports and imports machines in it. Nothing here calls the system: it reads from
//! and writes to whatever it is given, so any host can build on it.

pub mod failure;

pub use crate::failure::{Failure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};

/// The first line, which says what the file is and which way it's written.
pub const MAGIC: &[u8] = b"GUESTHOUSE-MACHINE 1\n";
/// Far more than a line of ours, and little enough to read whole.
const MOST_LINE_BYTES: u64 = 4096;
const MOST_PATH_BYTES: usize = 200;
const MOST_PATH_DEPTH: usize = 4;
const CHUNK_BYTES: usize = 1 << 20;
const NOT_OURS: &str = "This is not a machine exported from Guesthouse, or a newer Guesthouse wrote it.";
const CUT_SHORT: &str = "The file ends early. It may not have finished copying.";

/// What comes before each file, and the line that ends them all.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    end: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trailer {
    sha256: String,
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn line(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut text = serde_json::to_vec(value).map_err(|error| Failure(error.to_string()))?;
    text.push(b'\n');
    Ok(text)
}

/// A name in the file that is safe to make in a folder: plain parts, none hidden, in letters, digits
/// and a few marks, not too long and not too deep.
pub fn safe_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    let fits =
        !name.is_empty() && name.len() <= MOST_PATH_BYTES && path.components().count() <= MOST_PATH_DEPTH;
    let plain_part = |part: &str| {
        !part.starts_with('.')
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
    };
    let plain = path.components().all(|part| match part {
        Component::Normal(part) => part.to_str().is_some_and(plain_part),
        _ => false,
    });
    (fits && plain && !name.ends_with('/') && !name.contains("//")).then(|| path.to_owned())
}

pub struct Writer<W: Write> {
    out: W,
}

impl<W: Write> Writer<W> {
    pub fn new(mut out: W) -> Result<Self> {
        out.write_all(MAGIC)?;
        Ok(Self { out })
    }

    /// One file: exactly `size` bytes from `from`.
    pub fn file(&mut self, path: &str, size: u64, from: &mut impl Read) -> Result<()> {
        if safe_path(path).is_none() {
            return Err(Failure(format!("{path} can't go in an export.")));
        }
        let header = Header { path: Some(path.to_owned()), size: Some(size), end: None };
        self.out.write_all(&line(&header)?)?;
        let mut hash = Sha256::new();
        let mut left = size;
        let mut chunk = vec![0; CHUNK_BYTES];
        while left > 0 {
            let want = chunk.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let got = from.read(&mut chunk[..want])?;
            if got == 0 {
                return Err(Failure(format!("{path} got shorter while it was written.")));
            }
            hash.update(&chunk[..got]);
            self.out.write_all(&chunk[..got])?;
            left -= got as u64;
        }
        self.out.write_all(&line(&Trailer { sha256: hex(&hash.finalize()) })?)?;
        Ok(())
    }

    pub fn end(mut self) -> Result<W> {
        self.out.write_all(&line(&Header { end: Some(true), ..Header::default() })?)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// A file in an export, whose bytes come next.
#[derive(Debug, PartialEq, Eq)]
pub struct Member {
    pub path: PathBuf,
    pub size: u64,
}

pub struct Reader<R: BufRead> {
    input: R,
}

impl<R: BufRead> Reader<R> {
    pub fn new(mut input: R) -> Result<Self> {
        let mut magic = vec![0; MAGIC.len()];
        if input.read_exact(&mut magic).is_err() || magic != MAGIC {
            return Err(Failure::new(NOT_OURS));
        }
        Ok(Self { input })
    }

    #[allow(clippy::disallowed_methods)] // the read is taken to the limit
    fn read_line(&mut self) -> Result<Vec<u8>> {
        let mut text = Vec::new();
        (&mut self.input).take(MOST_LINE_BYTES).read_until(b'\n', &mut text)?;
        if text.pop() != Some(b'\n') {
            return Err(Failure::new(CUT_SHORT));
        }
        Ok(text)
    }

    /// The next file, or `None` at the end.
    pub fn next_file(&mut self) -> Result<Option<Member>> {
        let text = self.read_line()?;
        let header: Header = serde_json::from_slice(&text).map_err(|_| Failure::new(NOT_OURS))?;
        match header {
            Header { end: Some(true), path: None, size: None } => Ok(None),
            Header { path: Some(path), size: Some(size), end: None } => match safe_path(&path) {
                Some(path) => Ok(Some(Member { path, size })),
                None => Err(Failure(format!("The export names a file that can't be made safely: {path:?}"))),
            },
            _ => Err(Failure::new(NOT_OURS)),
        }
    }

    /// The member's bytes, into `to`, checked against its hash.
    pub fn copy_to(&mut self, member: &Member, to: &mut impl Write) -> Result<()> {
        let mut hash = Sha256::new();
        let mut left = member.size;
        let mut chunk = vec![0; CHUNK_BYTES];
        while left > 0 {
            let want = chunk.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let got = self.input.read(&mut chunk[..want])?;
            if got == 0 {
                return Err(Failure::new(CUT_SHORT));
            }
            hash.update(&chunk[..got]);
            to.write_all(&chunk[..got])?;
            left -= got as u64;
        }
        let text = self.read_line()?;
        let trailer: Trailer = serde_json::from_slice(&text).map_err(|_| Failure::new(NOT_OURS))?;
        if trailer.sha256 != hex(&hash.finalize()) {
            return Err(Failure(format!(
                "{} is damaged: it isn't what was exported.",
                member.path.display()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn written(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = Writer::new(Vec::new()).unwrap();
        for (path, data) in files {
            writer.file(path, data.len() as u64, &mut &data[..]).unwrap();
        }
        writer.end().unwrap()
    }

    fn read_all(data: Vec<u8>) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        let mut reader = Reader::new(Cursor::new(data))?;
        let mut found = Vec::new();
        while let Some(member) = reader.next_file()? {
            let mut bytes = Vec::new();
            reader.copy_to(&member, &mut bytes)?;
            found.push((member.path, bytes));
        }
        Ok(found)
    }

    #[test]
    fn what_is_written_reads_back() {
        let data = written(&[("machine.json", b"{}"), ("tpm/tpm2-00.permall", b"sealed"), ("empty", b"")]);
        let found = read_all(data).unwrap();
        let paths: Vec<&Path> = found.iter().map(|(path, _)| path.as_path()).collect();
        assert_eq!(paths, [Path::new("machine.json"), Path::new("tpm/tpm2-00.permall"), Path::new("empty")]);
        assert_eq!(found[1].1, b"sealed");
    }

    #[test]
    fn only_plain_names_inside_the_folder_are_safe() {
        for good in ["disk.qcow2", "snapshots/guesthouse-0a1b2c3d.fd", "seed/desk-password"] {
            assert!(safe_path(good).is_some(), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "tpm/../../x",
            "./disk",
            ".lock",
            "a/.hidden",
            "a//b",
            "a/",
            "name with space",
            "nul\0",
            "ünï",
            "a/b/c/d/e",
        ] {
            assert!(safe_path(bad).is_none(), "{bad:?}");
        }
        assert!(safe_path(&"x".repeat(MOST_PATH_BYTES + 1)).is_none());
    }

    #[test]
    fn a_file_that_is_damaged_cut_short_or_not_ours_is_refused() {
        let data = written(&[("vars.fd", b"firmware settings")]);
        let mut damaged = data.clone();
        let at = damaged.windows(8).position(|part| part == b"firmware").unwrap();
        damaged[at] = b'F';
        assert!(read_all(damaged).unwrap_err().0.contains("damaged"));
        let cut = data[..data.len() - 5].to_vec();
        assert_eq!(read_all(cut).unwrap_err().0, CUT_SHORT);
        assert_eq!(read_all(b"PK\x03\x04 a zip".to_vec()).unwrap_err().0, NOT_OURS);
        let mut evil = MAGIC.to_vec();
        evil.extend_from_slice(b"{\"path\":\"../../.bashrc\",\"size\":1}\nx");
        assert!(read_all(evil).unwrap_err().0.contains("can't be made safely"));
        let mut long = MAGIC.to_vec();
        long.extend(std::iter::repeat_n(b'x', 10_000));
        assert_eq!(read_all(long).unwrap_err().0, CUT_SHORT);
        let mut extra = MAGIC.to_vec();
        extra.extend_from_slice(b"{\"path\":\"a\",\"size\":0,\"mode\":4095}\n");
        assert_eq!(read_all(extra).unwrap_err().0, NOT_OURS);
    }
}
