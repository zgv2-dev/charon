# Charon

Ferries a vehicle's MAVLink stream across: a router that reads the vehicle from one source (UDP, TCP or a serial port) and passes it to the GCS and any other outputs, and their replies back to the vehicle. UDP datagrams go on byte for byte; a TCP or serial stream goes on as whole frames. It tracks every vehicle's heartbeat and packet loss, can record a tlog, and keeps its settings in a TOML file. It has no UI. Windows, macOS and Linux.

Everything else belongs to the app, as plugins: a plugin implements `charon::plugin::Plugin` and sees every datagram, in both directions.

## Use

```toml
[dependencies]
charon = { git = "https://github.com/zgv2-dev/charon", rev = "<commit>" }
```

```rust
use std::sync::Arc;

use charon::{config::Config, platform, plugin::{Plugin, Side}, supervisor::Supervisor};

struct Count(std::sync::atomic::AtomicU64);

impl Plugin for Count {
    // Runs on the routing path: keep it quick and never block.
    fn datagram(&self, _data: &[u8], _from: Side) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    platform::set_app(platform::App { name: "My App", id: "Me.MyApp" }); // data folders, autostart, toasts
    charon::logging::init("my-app");
    let cfg = Config::load(&platform::config_path())?;
    let sup = Supervisor::start(cfg).await?; // reads cfg.source, forwards to cfg.outputs
    sup.add_plugin(Arc::new(Count(Default::default())));
    // Change settings live with sup.apply(cfg); read sup.snapshot() and sup.warnings().
    std::future::pending().await
}
```

| Module | What it does |
| --- | --- |
| `endpoint` | Where MAVLink comes from or goes: `udpin:`, `udpout:`, `tcp:`, `tcpin:`, `serial:`. |
| `link` | One open endpoint: reads it, and writes through a queue so a slow client never holds up routing. TCP clients and serial ports reconnect on their own. |
| `router` | The fan-out. The source can change while running; outputs whose endpoint is unchanged keep their link. |
| `supervisor` | Owns the router and applies config changes in order, with warnings for ones that can't apply. |
| `config` | Source, named outputs (read-only, off, or only some sysids), the followed vehicle, tlog recording, and autostart with the feature. Reads version 1 files. `load_or` takes the app's defaults. |
| `stats` | Heartbeat age, loss and components of every vehicle, and the health of the followed one. |
| `stream` | Cuts frames out of a TCP or serial byte stream. |
| `frame` | MAVLink v1/v2 frame boundaries, headers and CRC. Payloads aren't decoded. |
| `plugin` | The `Plugin` trait. |
| `tlog` | Records and reads Mission Planner tlogs. |
| `platform` | Single instance, window focus, exclusive UDP bind, data folders, toasts, and `set_autostart` with the feature. Windows-only calls have stubs elsewhere. |
| `logging` | Daily log files in the app's data folder. |

## Settings

```toml
version = 2
source = "udpin:0.0.0.0:14551"    # or "tcp:192.168.1.5:5760", "tcpin:0.0.0.0:5760", "serial:COM5:921600"
follow = 1                        # the vehicle whose health counts; leave out for the last one heard
record_tlog = false
sound = true

[[outputs]]
name = "Mission Planner"
endpoint = "udpout:127.0.0.1:14550"

[[outputs]]
name = "Logger"
endpoint = "tcpin:0.0.0.0:5761"
read_only = true                  # hears everything, says nothing
sysids = [1]                      # only vehicle 1's frames
enabled = false                   # kept, not opened
```

A version 1 file (`listen`, `outputs` as addresses, `read_only`) is read as this: a `udpin:` source and `udpout:` outputs
named after their addresses. It is written as version 2 on the next save.

Starting at login is opt-in: an app that wants it turns on the `autostart` feature
(`charon = { ..., features = ["autostart"] }`), which adds `Config::autostart` and `platform::set_autostart`. Without
it, none of that code is built.
