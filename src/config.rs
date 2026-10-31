//! Router settings, stored as TOML in the app's config folder.

use std::{io, path::Path};

use serde::{Deserialize, Serialize};

use crate::endpoint::Endpoint;

/// 2: a source of any kind and named outputs. 1 had `listen`, plain UDP `outputs` and a `read_only` list.
pub const CONFIG_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Where the vehicle's MAVLink comes from.
    pub source: Endpoint,
    pub outputs: Vec<Output>,
    /// The vehicle whose heartbeat sets the health; None follows whichever autopilot was heard last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<u8>,
    /// Start at login. Only with the `autostart` feature, which an app turns on for itself.
    #[cfg(feature = "autostart")]
    pub autostart: bool,
    pub record_tlog: bool,
    /// Link transition sounds; toggled from the window or the extension popup.
    pub sound: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
    pub name: String,
    pub endpoint: Endpoint,
    /// Only listens: whatever it sends is counted and dropped, so it never reaches the vehicle.
    #[serde(default)]
    pub read_only: bool,
    /// Off keeps it in the list without opening it.
    #[serde(default = "on")]
    pub enabled: bool,
    /// Only frames from these systems reach it; empty for all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sysids: Vec<u8>,
}

fn on() -> bool {
    true
}

impl Output {
    pub fn new(name: impl Into<String>, endpoint: Endpoint) -> Output {
        Output {
            name: name.into(),
            endpoint,
            read_only: false,
            enabled: true,
            sysids: Vec::new(),
        }
    }

    pub fn read_only(self) -> Output {
        Output {
            read_only: true,
            ..self
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            source: Endpoint::UdpIn(crate::router::LISTEN),
            // The GCS on this machine. An app with more local listeners passes its own defaults to `load_or`.
            outputs: vec![Output::new(
                "GCS",
                Endpoint::UdpOut("127.0.0.1:14550".parse().unwrap()),
            )],
            follow: None,
            #[cfg(feature = "autostart")]
            autostart: false,
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

    /// As `load`, with `fallback` for a missing file and for any setting the file leaves out. A version 1 file is
    /// read as version 2; it is written as 2 on the next save.
    pub fn load_or(path: &Path, fallback: Config) -> io::Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(fallback),
            Err(e) => return Err(e),
        };
        let mut merged = toml::Table::try_from(&fallback).expect("config serializes");
        match toml::from_str::<toml::Table>(&text).and_then(|mut file| {
            migrate(&mut file);
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

    /// Outputs that would send straight back into the source.
    pub fn looping_outputs(&self) -> Vec<&Output> {
        self.outputs
            .iter()
            .filter(|o| o.endpoint.reaches(&self.source))
            .collect()
    }
}

/// Version 1 settings as version 2: `listen` becomes a `udpin:` source, each output a `udpout:` one named after its
/// address, read-only if it was in `read_only`. Left alone when it has no version 1 settings in it.
fn migrate(file: &mut toml::Table) {
    let v1 = file
        .get("version")
        .and_then(toml::Value::as_integer)
        .is_none_or(|v| v < 2);
    if !v1 {
        return;
    }
    file.insert("version".into(), i64::from(CONFIG_VERSION).into());
    if let Some(toml::Value::String(listen)) = file.remove("listen") {
        file.insert("source".into(), format!("udpin:{listen}").into());
    }
    let read_only: Vec<String> = match file.remove("read_only") {
        Some(toml::Value::Array(a)) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if let Some(toml::Value::Array(outputs)) = file.get("outputs")
        && outputs.iter().all(toml::Value::is_str)
    {
        let outputs = outputs
            .iter()
            .filter_map(toml::Value::as_str)
            .map(|addr| {
                let mut o = toml::Table::new();
                o.insert("name".into(), addr.into());
                o.insert("endpoint".into(), format!("udpout:{addr}").into());
                o.insert(
                    "read_only".into(),
                    read_only.iter().any(|r| r == addr).into(),
                );
                toml::Value::Table(o)
            })
            .collect();
        file.insert("outputs".into(), toml::Value::Array(outputs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn udpout(s: &str) -> Endpoint {
        Endpoint::UdpOut(s.parse().unwrap())
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        let mut c = Config::default();
        c.outputs.push(Output {
            sysids: vec![1, 2],
            enabled: false,
            ..Output::new("QGC laptop", udpout("192.168.1.20:14550")).read_only()
        });
        c.outputs
            .push(Output::new("Logger", "tcpin:0.0.0.0:5760".parse().unwrap()));
        c.source = "serial:COM5:921600".parse().unwrap();
        c.follow = Some(2);
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 2"), "{text}");
        assert!(text.contains(r#"source = "serial:COM5:921600""#), "{text}");
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
        std::fs::write(&path, "record_tlog = true\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert!(c.record_tlog);
        assert_eq!(c.outputs, Config::default().outputs);
        assert_eq!(c.version, CONFIG_VERSION);
    }

    #[test]
    fn an_output_needs_only_a_name_and_an_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "version = 2\n[[outputs]]\nname = \"MP\"\nendpoint = \"udpout:127.0.0.1:14550\"\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(&path).unwrap().outputs,
            [Output::new("MP", udpout("127.0.0.1:14550"))]
        );
    }

    #[test]
    fn reads_a_version_1_file_as_version_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"version = 1
listen = "0.0.0.0:14552"
outputs = ["127.0.0.1:14550", "127.0.0.1:14570"]
read_only = ["127.0.0.1:14570"]
record_tlog = true
"#,
        )
        .unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.version, 2);
        assert_eq!(c.source, Endpoint::UdpIn("0.0.0.0:14552".parse().unwrap()));
        assert_eq!(
            c.outputs,
            [
                Output::new("127.0.0.1:14550", udpout("127.0.0.1:14550")),
                Output::new("127.0.0.1:14570", udpout("127.0.0.1:14570")).read_only(),
            ]
        );
        assert!(c.record_tlog);
        assert!(!dir.path().join("config.toml.bad").exists());
    }

    #[test]
    fn apps_fallback_fills_a_missing_file_and_left_out_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fallback = Config {
            outputs: vec![
                Output::new("GCS", udpout("127.0.0.1:14550")),
                Output::new("Widgets", udpout("127.0.0.1:14570")).read_only(),
            ],
            ..Config::default()
        };
        assert_eq!(Config::load_or(&path, fallback.clone()).unwrap(), fallback);
        std::fs::write(&path, "record_tlog = true\n").unwrap();
        let c = Config::load_or(&path, fallback.clone()).unwrap();
        assert!(c.record_tlog);
        assert_eq!(c.outputs, fallback.outputs);
    }

    #[test]
    fn unreadable_file_is_kept_as_bad() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "source = [not toml").unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(
            dir.path().join("config.toml.bad").exists(),
            "user's edits must survive"
        );
    }

    #[test]
    fn an_unknown_endpoint_kind_is_unreadable_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 2\nsource = \"ftp:1.2.3.4:21\"\n").unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(dir.path().join("config.toml.bad").exists());
    }

    #[test]
    fn detects_outputs_that_loop() {
        let c = Config {
            outputs: vec![
                Output::new("a", udpout("127.0.0.1:14551")),
                Output::new("b", udpout("192.168.1.5:14551")),
                Output::new("c", udpout("127.0.0.1:14550")),
            ],
            ..Config::default()
        };
        let names: Vec<_> = c
            .looping_outputs()
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(names, ["a", "b"]);
    }
}
