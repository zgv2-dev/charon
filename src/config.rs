//! Router settings, stored as TOML in %APPDATA%.

use std::{io, net::SocketAddr, path::Path};

use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Where the vehicle sends.
    pub listen: SocketAddr,
    pub outputs: Vec<SocketAddr>,
    /// Outputs that only listen: whatever they send is counted and dropped, so it never reaches the vehicle.
    pub read_only: Vec<SocketAddr>,
    pub autostart: bool,
    pub record_tlog: bool,
    /// Link transition sounds; toggled from the window or the extension popup.
    pub sound: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            listen: crate::router::LISTEN,
            // The GCS on this machine. An app with more local listeners passes its own defaults to `load_or`.
            outputs: vec!["127.0.0.1:14550".parse().unwrap()],
            read_only: Vec::new(),
            autostart: true,
            record_tlog: false,
            sound: true,
        }
    }
}

impl Config {
    /// A missing file gives defaults. A file that doesn't parse is renamed to `.bad` so it isn't overwritten.
    pub fn load(path: &Path) -> io::Result<Config> {
        Config::load_or(path, Config::default())
    }

    /// As `load`, with `fallback` for a missing file and for any setting the file leaves out.
    pub fn load_or(path: &Path, fallback: Config) -> io::Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(fallback),
            Err(e) => return Err(e),
        };
        let mut merged = toml::Table::try_from(&fallback).expect("config serializes");
        match toml::from_str::<toml::Table>(&text).and_then(|file| {
            merged.extend(file);
            toml::Value::Table(merged).try_into()
        }) {
            Ok(c) => Ok(c),
            Err(e) => {
                tracing::warn!("config unreadable, kept as .bad: {e}");
                std::fs::rename(path, path.with_extension("toml.bad"))?;
                Ok(fallback)
            }
        }
    }

    /// Write-then-rename, so a crash mid-save never leaves a half-written config.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(
            &tmp,
            toml::to_string_pretty(self).expect("config serializes"),
        )?;
        std::fs::rename(tmp, path)
    }

    /// Outputs that would send straight back into the router's listen port.
    pub fn looping_outputs(&self, listen: SocketAddr) -> Vec<SocketAddr> {
        self.outputs
            .iter()
            .copied()
            .filter(|o| {
                o.port() == listen.port()
                    && (o.ip().is_loopback()
                        || listen.ip().is_unspecified()
                        || o.ip() == listen.ip())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        let mut c = Config::default();
        c.outputs.push("192.168.1.20:14550".parse().unwrap());
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("version = 1")
        );
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("nope.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn partial_file_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "autostart = false\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert!(!c.autostart);
        assert_eq!(c.outputs, Config::default().outputs);
        assert_eq!(c.version, CONFIG_VERSION);
    }

    #[test]
    fn apps_fallback_fills_a_missing_file_and_left_out_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fallback = Config {
            outputs: vec![
                "127.0.0.1:14550".parse().unwrap(),
                "127.0.0.1:14570".parse().unwrap(),
            ],
            ..Config::default()
        };
        assert_eq!(Config::load_or(&path, fallback.clone()).unwrap(), fallback);
        std::fs::write(&path, "autostart = false\n").unwrap();
        let c = Config::load_or(&path, fallback.clone()).unwrap();
        assert!(!c.autostart);
        assert_eq!(c.outputs, fallback.outputs);
    }

    #[test]
    fn unreadable_file_is_kept_as_bad() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "listen = [not toml").unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(
            dir.path().join("config.toml.bad").exists(),
            "user's edits must survive"
        );
    }

    #[test]
    fn detects_outputs_that_loop() {
        let mut c = Config::default();
        c.outputs = vec![
            "127.0.0.1:14551".parse().unwrap(),
            "192.168.1.5:14551".parse().unwrap(),
            "127.0.0.1:14550".parse().unwrap(),
        ];
        assert_eq!(
            c.looping_outputs(crate::router::LISTEN),
            vec![c.outputs[0], c.outputs[1]]
        );
        assert_eq!(
            c.looping_outputs("192.168.1.9:14551".parse().unwrap()),
            vec![c.outputs[0]]
        );
    }
}
