//! Plugins see every datagram the router passes, in both directions. Add them with `Supervisor::add_plugin`.

use std::net::SocketAddr;

/// Where a datagram came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Received on the listen port.
    Vehicle,
    /// A reply on this output's socket, normally from a GCS.
    Output(SocketAddr),
}

pub trait Plugin: Send + Sync {
    /// Called on the routing path for every datagram, before it is forwarded: keep it quick and never block.
    fn datagram(&self, data: &[u8], from: Side);
}
