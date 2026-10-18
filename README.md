# Charon

Ferries a vehicle's MAVLink stream across: a UDP router that takes the vehicle's datagrams on one port and passes them byte for byte to the GCS and any other outputs, and their replies back to the vehicle. It tracks the vehicle's heartbeat and packet loss, can record a tlog, and keeps its settings in a TOML file. It has no UI.

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
    let sup = Supervisor::start(cfg).await?; // listens on cfg.listen, forwards to cfg.outputs
    sup.add_plugin(Arc::new(Count(Default::default())));
    // Change settings live with sup.apply(cfg); read sup.snapshot() and sup.warnings().
    std::future::pending().await
}
```

| Module | What it does |
| --- | --- |
| `router` | The UDP fan-out. The listen address can move while running; outputs keep their sockets. |
| `supervisor` | Owns the router and applies config changes in order, with warnings for ones that can't apply. |
| `config` | Listen address, outputs, autostart, tlog recording. `load_or` takes the app's defaults. |
| `stats` | Heartbeat age, loss and health of the one vehicle. |
| `frame` | MAVLink v1/v2 frame boundaries, headers and CRC. Payloads aren't decoded. |
| `plugin` | The `Plugin` trait. |
| `tlog` | Records and reads Mission Planner tlogs. |
| `platform` | Single instance, window focus, exclusive UDP bind, data folders, autostart and toasts. Windows-only calls have stubs elsewhere. |
| `logging` | Daily log files in the app's data folder. |
