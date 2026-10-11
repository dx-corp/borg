//! Health evidence after an update.
//!
//! A running daemon writes a small record after every successful heartbeat
//! (drone) or reconciliation pass (queen). The updater polls it to learn that
//! the new binary came up and is working; no new endpoint is needed and the
//! updater needs no credentials.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRecord {
    pub version: String,
    /// Unix seconds of the last success.
    pub at: i64,
    pub pid: u32,
}

/// Write the record atomically (temporary file, then rename).
pub fn write(path: &Path, record: &HealthRecord) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(record).map_err(io::Error::other)?)?;
    fs::rename(&tmp, path)
}

pub fn read(path: &Path) -> Option<HealthRecord> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

pub trait HealthProbe {
    /// The newest record the daemon has written, if any.
    fn current(&self) -> Option<HealthRecord>;
}

pub struct FileHealth(pub PathBuf);

impl HealthProbe for FileHealth {
    fn current(&self) -> Option<HealthRecord> {
        read(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_garbage_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        assert_eq!(FileHealth(path.clone()).current(), None);
        let rec = HealthRecord {
            version: "1.2.3".into(),
            at: 100,
            pid: 7,
        };
        write(&path, &rec).unwrap();
        assert_eq!(FileHealth(path.clone()).current(), Some(rec));
        fs::write(&path, "not json").unwrap();
        assert_eq!(read(&path), None);
    }
}
