//! Telemetry log in Mission Planner/QGC format: each frame prefixed by its receive time,
//! big-endian u64 microseconds since the Unix epoch.

use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::frame::{frames, parse_one};

pub struct Tlog {
    file: File,
    path: PathBuf,
}

impl Tlog {
    pub fn create(dir: &Path) -> io::Result<Tlog> {
        std::fs::create_dir_all(dir)?;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = dir.join(format!("flight-{secs}.tlog"));
        Ok(Tlog {
            file: File::create_new(&path)?, // a cut within the same second must not truncate the last file
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One write per datagram, so a crash loses at most the datagram in flight.
    pub fn write(&mut self, datagram: &[u8], at: SystemTime) -> io::Result<()> {
        let us = (at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64)
            .to_be_bytes();
        let mut buf = Vec::with_capacity(datagram.len() + 16);
        for f in frames(datagram) {
            buf.extend_from_slice(&us);
            buf.extend_from_slice(f.bytes);
        }
        self.file.write_all(&buf)
    }
}

pub fn read(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 < bytes.len() {
        let us = u64::from_be_bytes(bytes[i..i + 8].try_into().unwrap());
        let Some(f) = parse_one(&bytes[i + 8..]) else {
            break;
        };
        out.push((us, f.bytes));
        i += 8 + f.bytes.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::testutil::heartbeat;
    use std::time::Duration;

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Tlog::create(dir.path()).unwrap();
        let at = UNIX_EPOCH + Duration::from_micros(1_700_000_000_000_123);
        let mut two = heartbeat(1, 1, 1, 2, 3);
        two.extend(heartbeat(2, 1, 1, 2, 3));
        log.write(&two, at).unwrap();
        let bytes = std::fs::read(log.path()).unwrap();
        let entries = read(&bytes);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, 1_700_000_000_000_123);
        assert_eq!(entries[1].1, &heartbeat(2, 1, 1, 2, 3)[..]);
    }

    #[test]
    fn read_stops_at_truncated_tail() {
        let mut bytes = 5u64.to_be_bytes().to_vec();
        bytes.extend(heartbeat(1, 1, 1, 2, 3));
        bytes.extend(6u64.to_be_bytes());
        bytes.extend(&heartbeat(2, 1, 1, 2, 3)[..4]);
        assert_eq!(read(&bytes).len(), 1);
    }
}
