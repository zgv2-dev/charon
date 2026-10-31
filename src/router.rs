//! The fan-out. Whatever the source sends goes to every output; whatever an output sends goes to the source and the
//! other outputs. Bytes are forwarded as they arrive (a stream's as whole frames); the router never originates or
//! edits a packet.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime},
};

use crate::{
    config::Output,
    endpoint::Endpoint,
    frame::frames,
    link::{Counters, Link, OnReceive, PEER_TTL, State},
    plugin::{OutputId, Plugin, Side},
    stats::{Health, Stats, VehicleStatus},
    tlog::Tlog,
};

/// Where vehicles send by default.
pub const LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 14551);

/// The bind failed because another process (normally the GCS) holds the port.
/// Windows reports an exclusive-bind conflict as WSAEACCES, hence PermissionDenied.
pub fn port_taken(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

/// An output's settings that change without reopening it.
#[derive(Clone)]
struct Settings {
    name: String,
    read_only: bool,
    sysids: Vec<u8>,
}

struct OutputLink {
    id: OutputId,
    link: Arc<Link>,
    settings: Mutex<Settings>,
}

struct Shared {
    /// None only while `set_source` swaps it, or after a swap that couldn't open anything.
    source: Mutex<Option<Arc<Link>>>,
    outputs: Mutex<Vec<Arc<OutputLink>>>,
    stats: Mutex<Stats>,
    tlog: Mutex<Option<Tlog>>,
    plugins: Mutex<Vec<Arc<dyn Plugin>>>,
}

pub struct Router {
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl Router {
    /// Fails when the source can't be opened, normally because its port is taken (see `port_taken`). An output that
    /// can't be opened is left out, with its error in the result.
    pub async fn start(
        source: Endpoint,
        outputs: &[Output],
    ) -> io::Result<(Router, Vec<(String, io::Error)>)> {
        let shared = Arc::new(Shared {
            source: Mutex::default(),
            outputs: Mutex::default(),
            stats: Mutex::default(),
            tlog: Mutex::default(),
            plugins: Mutex::default(),
        });
        let router = Router {
            shared,
            next_id: AtomicU64::new(1),
        };
        router.open_source(source.clone()).await?;
        let failed = router.set_outputs(outputs).await;
        tracing::info!("router reading {source}");
        Ok((router, failed))
    }

    pub fn source(&self) -> Option<Endpoint> {
        self.source_link().map(|l| l.endpoint.clone())
    }

    /// Where a `udpin:` or `tcpin:` source listens: the port it got, for a port 0 bind.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.source_link()?.local_addr()
    }

    fn source_link(&self) -> Option<Arc<Link>> {
        self.shared.source.lock().unwrap().clone()
    }

    /// From now on `plugin` sees every datagram.
    pub fn add_plugin(&self, plugin: Arc<dyn Plugin>) {
        self.shared.plugins.lock().unwrap().push(plugin);
    }

    /// Whose health the snapshot reports: a sysid, or None for whichever vehicle was heard last.
    pub fn set_follow(&self, sysid: Option<u8>) {
        self.shared.stats.lock().unwrap().set_follow(sysid);
    }

    /// Reads from `source` instead; outputs, stats and the tlog carry on. On failure it stays on the old one.
    pub async fn set_source(&self, source: Endpoint) -> io::Result<()> {
        let old = self.shared.source.lock().unwrap().take();
        // The old one closes first: an exclusive bind to the same port fails while it is open.
        if let Some(old) = &old {
            old.close().await;
        }
        match self.open_source(source.clone()).await {
            Ok(()) => {
                tracing::info!("router reading {source}");
                Ok(())
            }
            Err(e) => {
                // shortcut: if another app takes the old port in this gap, nothing is received until the next change
                if let Some(old) = old {
                    // the port it had, not port 0 again
                    let again = match (&old.endpoint, old.local_addr()) {
                        (Endpoint::UdpIn(_), Some(a)) => Endpoint::UdpIn(a),
                        (Endpoint::TcpServer(_), Some(a)) => Endpoint::TcpServer(a),
                        (e, _) => e.clone(),
                    };
                    if let Err(e2) = self.open_source(again.clone()).await {
                        tracing::error!("can't open {again} again: {e2}");
                    }
                }
                Err(e)
            }
        }
    }

    async fn open_source(&self, source: Endpoint) -> io::Result<()> {
        let shared = Arc::downgrade(&self.shared);
        let on_rx: OnReceive = Arc::new(move |data| {
            if let Some(s) = shared.upgrade() {
                s.on_source(data);
            }
        });
        let link = Link::open(source, on_rx).await?;
        *self.shared.source.lock().unwrap() = Some(link);
        Ok(())
    }

    /// Applies the enabled outputs. One whose endpoint is unchanged keeps its link, so the GCS doesn't blip; its
    /// name, read-only flag and sysids change in place. Returns the ones that couldn't be opened.
    pub async fn set_outputs(&self, outputs: &[Output]) -> Vec<(String, io::Error)> {
        let old = self.shared.outputs.lock().unwrap().clone();
        let mut next = Vec::with_capacity(outputs.len());
        let mut failed = Vec::new();
        for o in outputs.iter().filter(|o| o.enabled) {
            if next
                .iter()
                .any(|n: &Arc<OutputLink>| n.link.endpoint == o.endpoint)
            {
                continue; // listed twice: the first one wins
            }
            let settings = Settings {
                name: o.name.clone(),
                read_only: o.read_only,
                sysids: o.sysids.clone(),
            };
            if let Some(kept) = old.iter().find(|k| k.link.endpoint == o.endpoint) {
                *kept.settings.lock().unwrap() = settings;
                next.push(kept.clone());
                continue;
            }
            let id = OutputId(self.next_id.fetch_add(1, Ordering::Relaxed));
            let shared = Arc::downgrade(&self.shared);
            let on_rx: OnReceive = Arc::new(move |data| {
                if let Some(s) = shared.upgrade() {
                    s.on_output(id, data);
                }
            });
            match Link::open(o.endpoint.clone(), on_rx).await {
                Ok(link) => {
                    tracing::info!("output added: {} ({})", o.name, o.endpoint);
                    next.push(Arc::new(OutputLink {
                        id,
                        link,
                        settings: Mutex::new(settings),
                    }));
                }
                Err(e) => failed.push((o.name.clone(), e)),
            }
        }
        *self.shared.outputs.lock().unwrap() = next.clone();
        for o in old
            .iter()
            .filter(|o| !next.iter().any(|n| Arc::ptr_eq(n, o)))
        {
            o.link.close().await;
            tracing::info!("output removed: {}", o.link.endpoint);
        }
        failed
    }

    /// `Some(dir)` starts a new log file there; `None` stops recording.
    pub fn set_tlog(&self, dir: Option<&Path>) -> io::Result<()> {
        let next = dir.map(Tlog::create).transpose()?;
        if let Some(t) = &next {
            tracing::info!("recording to {}", t.path().display());
        }
        *self.shared.tlog.lock().unwrap() = next;
        Ok(())
    }

    /// Closes the current log so it can be copied off and goes on in a new file in the same folder.
    /// Returns the closed file, or None when not recording.
    pub fn cut_tlog(&self) -> io::Result<Option<PathBuf>> {
        let mut tlog = self.shared.tlog.lock().unwrap();
        let Some(old) = tlog.as_ref() else {
            return Ok(None);
        };
        let closed = old.path().to_path_buf();
        let next = Tlog::create(closed.parent().unwrap_or(Path::new(".")))?;
        tracing::info!("recording to {}", next.path().display());
        *tlog = Some(next);
        Ok(Some(closed))
    }

    pub async fn stop(&self) {
        let outputs = std::mem::take(&mut *self.shared.outputs.lock().unwrap());
        for o in &outputs {
            o.link.close().await;
        }
        let source = self.shared.source.lock().unwrap().take();
        if let Some(s) = source {
            s.close().await;
        }
        tracing::info!("router stopped");
    }
}

impl Shared {
    fn to_plugins(&self, data: &[u8], from: Side) {
        let plugins = self.plugins.lock().unwrap().clone();
        for p in &plugins {
            p.datagram(data, from);
        }
    }

    // shortcut: blocking file write on the routing path; fine at telemetry rates, move to a writer task if it shows up in latency
    fn record(&self, data: &[u8]) {
        let mut tlog = self.tlog.lock().unwrap();
        if let Some(log) = tlog.as_mut()
            && let Err(e) = log.write(data, SystemTime::now())
        {
            tracing::warn!("tlog write failed, recording stopped: {e}");
            *tlog = None; // routing must not stop because the disk is full
        }
    }

    fn to_outputs(&self, data: &[u8], except: Option<OutputId>) {
        let outputs = self.outputs.lock().unwrap().clone();
        for o in outputs.iter().filter(|o| Some(o.id) != except) {
            let sysids = o.settings.lock().unwrap().sysids.clone();
            if sysids.is_empty() {
                o.link.send(data);
                continue;
            }
            // only the frames of those systems; anything not a frame can't be told apart, so it stays out too
            let picked: Vec<u8> = frames(data)
                .iter()
                .filter(|f| sysids.contains(&f.header.sysid))
                .flat_map(|f| f.bytes.iter().copied())
                .collect();
            if !picked.is_empty() {
                o.link.send(&picked);
            }
        }
    }

    fn on_source(&self, data: &[u8]) {
        self.stats.lock().unwrap().ingest(data, Instant::now());
        self.record(data);
        self.to_plugins(data, Side::Vehicle);
        self.to_outputs(data, None);
    }

    fn on_output(&self, id: OutputId, data: &[u8]) {
        let read_only = self
            .outputs
            .lock()
            .unwrap()
            .iter()
            .find(|o| o.id == id)
            .is_none_or(|o| o.settings.lock().unwrap().read_only);
        if read_only {
            return; // counted by its link, and dropped
        }
        self.stats.lock().unwrap().ingest(data, Instant::now());
        self.record(data);
        self.to_plugins(data, Side::Output(id));
        if let Some(source) = self.source.lock().unwrap().clone() {
            source.send(data);
        }
        self.to_outputs(data, Some(id));
    }
}

#[derive(Debug, Clone)]
pub struct LinkStatus {
    pub endpoint: Endpoint,
    /// Where a UDP or `tcpin:` link is bound: the real port when the endpoint asked for port 0.
    pub local_addr: Option<SocketAddr>,
    pub state: State,
    pub counters: Counters,
}

impl LinkStatus {
    fn of(link: &Link) -> LinkStatus {
        LinkStatus {
            endpoint: link.endpoint.clone(),
            local_addr: link.local_addr(),
            state: link.state(),
            counters: link.counters(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutputStatus {
    pub id: OutputId,
    pub name: String,
    pub read_only: bool,
    pub link: LinkStatus,
    /// Something replied on this output within PEER_TTL.
    pub gcs_connected: bool,
}

#[derive(Debug, Clone)]
pub struct RouterSnapshot {
    /// None only between a failed source change and the next one.
    pub source: Option<LinkStatus>,
    pub health: Health,
    /// The followed vehicle.
    pub vehicle: Option<VehicleStatus>,
    pub vehicles: Vec<VehicleStatus>,
    pub outputs: Vec<OutputStatus>,
    pub crc_errors: u64,
}

impl Router {
    pub fn snapshot(&self) -> RouterSnapshot {
        let now = Instant::now();
        let outputs = self
            .shared
            .outputs
            .lock()
            .unwrap()
            .iter()
            .map(|o| {
                let link = LinkStatus::of(&o.link);
                let settings = o.settings.lock().unwrap().clone();
                let gcs_connected = link
                    .counters
                    .last_rx
                    .is_some_and(|t| now.saturating_duration_since(t) < PEER_TTL);
                OutputStatus {
                    id: o.id,
                    name: settings.name,
                    read_only: settings.read_only,
                    link,
                    gcs_connected,
                }
            })
            .collect();
        let stats = self.shared.stats.lock().unwrap();
        RouterSnapshot {
            source: self.source_link().map(|l| LinkStatus::of(&l)),
            health: stats.health(now),
            vehicle: stats.vehicle(now),
            vehicles: stats.vehicles(now),
            outputs,
            crc_errors: stats.crc_errors,
        }
    }
}
