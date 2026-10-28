//! Charon: routes a vehicle's MAVLink stream, from UDP, TCP or a serial port, to the GCS and other outputs, and tracks
//! every vehicle's heartbeat and loss. No UI.
//!
//! Anything more is the app's: it implements [`plugin::Plugin`] to see the traffic, and adds it to the running
//! router with [`supervisor::Supervisor::add_plugin`].

pub mod config;
pub mod endpoint;
pub mod frame;
pub mod link;
pub mod logging;
pub mod platform;
pub mod plugin;
pub mod router;
pub mod stats;
pub mod stream;
pub mod supervisor;
pub mod tlog;
