//! Plugins see every datagram the router passes, in both directions. Add them with `Supervisor::add_plugin`.

/// An output, for as long as it stays open; `RouterSnapshot::outputs` says which one it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u64);

/// Where a datagram came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// From the source: the vehicle.
    Vehicle,
    /// From an output, normally a GCS.
    Output(OutputId),
}

pub trait Plugin: Send + Sync {
    /// Called on the routing path for every datagram, before it is forwarded: keep it quick and never block.
    fn datagram(&self, data: &[u8], from: Side);
}
