//! What `prepare` knows of a machine that `boot` needs too, kept in a small file beside its disk:
//! which system it runs, and for a Linux guest, its installer and whether it has installed.

#![allow(clippy::disallowed_methods)] // the host's own files, not a guest's disk

use crate::machine::Guest;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub guest: Guest,
    /// The ISO a Linux guest installs from, attached until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installer: Option<PathBuf>,
    #[serde(default)]
    pub installed: bool,
    /// The machine's network card's address, the same at every boot, so the guest's network
    /// settings and its DHCP lease keep to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// The fingerprint of the remoteman host in a Windows guest, as the guest last said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
}

impl Record {
    pub fn path(disk: &Path) -> PathBuf {
        disk.with_extension("guesthouse.json")
    }

    /// The disk's record. A disk with none is taken for a Windows guest, as every disk was before
    /// there were records.
    pub fn load(disk: &Path) -> Result<Self, String> {
        let path = Record::path(disk);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Record::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn save(&self, disk: &Path) -> Result<(), String> {
        let path = Record::path(disk);
        let text = serde_json::to_string_pretty(self).expect("a record always serialises");
        fs::write(&path, text + "\n").map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// A Linux guest's files disk, beside its own: its installer's boot files and answer file, and the
/// host's GPU files the guest mounts at /usr/lib/wsl.
pub fn files_disk(disk: &Path) -> PathBuf {
    disk.with_extension("files.vhdx")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_reads_back_as_it_was_written() {
        let folder = std::env::temp_dir().join(format!("gh-record-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let disk = folder.join("ubuntu.vhdx");
        assert_eq!(Record::load(&disk).unwrap(), Record::default(), "no record is a Windows guest");
        let record = Record {
            guest: Guest::Linux,
            installer: Some(folder.join("u.iso")),
            installed: false,
            mac: Some("00-15-5D-00-00-01".into()),
            screen: Some("ab".repeat(32)),
        };
        record.save(&disk).unwrap();
        assert_eq!(Record::load(&disk).unwrap(), record);
        assert!(fs::read_to_string(Record::path(&disk)).unwrap().contains("\"guest\": \"linux\""));
        let _ = fs::remove_dir_all(folder);
    }

    #[test]
    fn the_files_disk_sits_beside_the_guests_own() {
        assert_eq!(files_disk(Path::new(r"D:\vms\ubuntu.vhdx")), Path::new(r"D:\vms\ubuntu.files.vhdx"));
    }
}
