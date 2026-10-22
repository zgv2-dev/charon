//! UDP fan-out. Datagrams are forwarded byte-for-byte; the router never originates or edits a packet.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{net::UdpSocket, task::JoinHandle};

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    platform,
    plugin::{Plugin, Side},
    stats::{Health, Stats, VehicleStatus},
    tlog::Tlog,
};

/// Vehicles always send here.
pub const LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 14551);

/// The vehicle address is forgotten after this much silence.
pub const SOURCE_TTL: Duration = Duration::from_secs(5);

/// The bind failed because another process (normally the GCS) holds the port.
/// Windows reports an exclusive-bind conflict as WSAEACCES, hence PermissionDenied.
pub fn port_taken(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

#[derive(Debug, Default, Clone)]
pub struct OutputCounters {
    pub packets_out: u64,
    pub bytes_out: u64,
    pub packets_in: u64,
    pub bytes_in: u64,
    pub last_tx: Option<Instant>,
    pub last_rx: Option<Instant>,
}

struct Output {
    target: SocketAddr,
    /// Bound to an ephemeral port; the GCS replies here.
    sock: UdpSocket,
    counters: Mutex<OutputCounters>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Output {
    async fn stop(&self) {
        let task = self.task.lock().unwrap().take();
        if let Some(t) = task {
            t.abort();
            let _ = t.await;
        }
    }
}

struct Shared {
    /// The vehicle side. None only while `set_listen` swaps it, or after a swap that couldn't bind anything.
    inbound: Mutex<Option<Arc<UdpSocket>>>,
    listen: Mutex<SocketAddr>,
    outputs: Mutex<Vec<Arc<Output>>>,
    /// Targets whose datagrams are dropped.
    read_only: Mutex<Vec<SocketAddr>>,
    /// One vehicle at a time: the latest source seen on the listen port.
    vehicle: Mutex<Option<(SocketAddr, Instant)>>,
    stats: Mutex<Stats>,
    tlog: Mutex<Option<Tlog>>,
    plugins: Mutex<Vec<Arc<dyn Plugin>>>,
}

pub struct Router {
    shared: Arc<Shared>,
    inbound_task: Mutex<Option<JoinHandle<()>>>,
}

impl Router {
    pub async fn start(listen: SocketAddr, outputs: &[SocketAddr]) -> io::Result<Router> {
        let inbound = bind(listen)?;
        let shared = Arc::new(Shared {
            inbound: Mutex::default(),
            listen: Mutex::new(listen),
            outputs: Mutex::default(),
            read_only: Mutex::default(),
            vehicle: Mutex::default(),
            stats: Mutex::default(),
            tlog: Mutex::default(),
            plugins: Mutex::default(),
        });
        let router = Router {
            shared,
            inbound_task: Mutex::default(),
        };
        router.run_inbound(inbound);
        router.set_outputs(outputs).await?;
        tracing::info!("router listening on {}", router.local_addr());
        Ok(router)
    }

    pub fn local_addr(&self) -> SocketAddr {
        *self.shared.listen.lock().unwrap()
    }

    /// From now on `plugin` sees every datagram.
    pub fn add_plugin(&self, plugin: Arc<dyn Plugin>) {
        self.shared.plugins.lock().unwrap().push(plugin);
    }

    /// Moves the vehicle side to `listen`; outputs, stats and the tlog carry on. On failure it stays where it was.
    pub async fn set_listen(&self, listen: SocketAddr) -> io::Result<()> {
        let old = self.local_addr();
        // The old socket closes first: an exclusive bind to the same port fails while it is open.
        self.stop_inbound().await;
        match bind(listen) {
            Ok(sock) => {
                self.run_inbound(sock);
                tracing::info!("router listening on {}", self.local_addr());
                Ok(())
            }
            Err(e) => {
                // shortcut: if another app takes the old port in this gap, nothing is received until the next change
                match bind(old) {
                    Ok(sock) => self.run_inbound(sock),
                    Err(e2) => tracing::error!("can't listen on {old} again: {e2}"),
                }
                Err(e)
            }
        }
    }

    fn run_inbound(&self, sock: Arc<UdpSocket>) {
        *self.shared.listen.lock().unwrap() = sock.local_addr().expect("bound socket");
        *self.shared.inbound.lock().unwrap() = Some(sock.clone());
        let task = tokio::spawn(inbound_loop(self.shared.clone(), sock));
        *self.inbound_task.lock().unwrap() = Some(task);
    }

    async fn stop_inbound(&self) {
        let task = self.inbound_task.lock().unwrap().take();
        if let Some(t) = task {
            t.abort();
            let _ = t.await;
        }
        self.shared.inbound.lock().unwrap().take();
    }

    /// Outputs whose target is unchanged keep their socket, so the GCS link doesn't blip.
    pub async fn set_outputs(&self, targets: &[SocketAddr]) -> io::Result<()> {
        let old = self.shared.outputs.lock().unwrap().clone();
        let mut next = Vec::with_capacity(targets.len());
        for &target in targets {
            match old.iter().find(|o| o.target == target) {
                Some(o) => next.push(o.clone()),
                None => {
                    next.push(self.spawn_output(target).await?);
                    tracing::info!("output added: {target}");
                }
            }
        }
        *self.shared.outputs.lock().unwrap() = next;
        for o in old.iter().filter(|o| !targets.contains(&o.target)) {
            o.stop().await;
            tracing::info!("output removed: {}", o.target);
        }
        Ok(())
    }

    /// Whatever these outputs send from now on is counted and dropped: not routed, recorded or shown to plugins.
    pub fn set_read_only(&self, targets: &[SocketAddr]) {
        *self.shared.read_only.lock().unwrap() = targets.to_vec();
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
            o.stop().await;
        }
        self.stop_inbound().await;
        tracing::info!("router stopped");
    }

    async fn spawn_output(&self, target: SocketAddr) -> io::Result<Arc<Output>> {
        // shortcut: non-loopback IPv6 targets unsupported, add when someone needs one
        let ip = if target.ip().is_loopback() {
            target.ip()
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        };
        let sock = UdpSocket::bind(SocketAddr::new(ip, 0)).await?;
        let out = Arc::new(Output {
            target,
            sock,
            counters: Mutex::default(),
            task: Mutex::default(),
        });
        let task = tokio::spawn(output_loop(self.shared.clone(), out.clone()));
        *out.task.lock().unwrap() = Some(task);
        Ok(out)
    }
}

impl Shared {
    fn to_plugins(&self, data: &[u8], from: Side) {
        let plugins = self.plugins.lock().unwrap().clone();
        for p in &plugins {
            p.datagram(data, from);
        }
    }

    // shortcut: blocking file write on the async path; fine at telemetry rates, move to a writer task if it shows up in latency
    fn record(&self, data: &[u8]) {
        let mut tlog = self.tlog.lock().unwrap();
        if let Some(log) = tlog.as_mut() {
            if let Err(e) = log.write(data, SystemTime::now()) {
                tracing::warn!("tlog write failed, recording stopped: {e}");
                *tlog = None; // routing must not stop because the disk is full
            }
        }
    }

    async fn to_outputs(&self, data: &[u8], except: Option<SocketAddr>) {
        let outputs = self.outputs.lock().unwrap().clone();
        for o in outputs.iter().filter(|o| Some(o.target) != except) {
            if o.sock.send_to(data, o.target).await.is_ok() {
                let mut c = o.counters.lock().unwrap();
                c.packets_out += 1;
                c.bytes_out += data.len() as u64;
                c.last_tx = Some(Instant::now());
            }
        }
    }

    async fn to_vehicle(&self, data: &[u8]) {
        let vehicle = self
            .vehicle
            .lock()
            .unwrap()
            .filter(|(_, seen)| seen.elapsed() < SOURCE_TTL);
        let inbound = self.inbound.lock().unwrap().clone();
        if let (Some((addr, _)), Some(sock)) = (vehicle, inbound) {
            let _ = sock.send_to(data, addr).await;
        }
    }
}

/// recv_from that never gives up: Windows reports an ICMP "port unreachable" from an earlier send
/// as WSAECONNRESET on the next receive.
async fn recv(sock: &UdpSocket, buf: &mut [u8]) -> (usize, SocketAddr) {
    loop {
        match sock.recv_from(buf).await {
            Ok(r) => return r,
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                tracing::warn!("udp recv: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn bind(addr: SocketAddr) -> io::Result<Arc<UdpSocket>> {
    Ok(Arc::new(UdpSocket::from_std(
        platform::bind_udp_exclusive(addr)?,
    )?))
}

async fn inbound_loop(shared: Arc<Shared>, sock: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, from) = recv(&sock, &mut buf).await;
        let data = &buf[..n];
        *shared.vehicle.lock().unwrap() = Some((from, Instant::now()));
        shared.stats.lock().unwrap().ingest(data, Instant::now());
        shared.record(data);
        shared.to_plugins(data, Side::Vehicle);
        shared.to_outputs(data, None).await;
    }
}

async fn output_loop(shared: Arc<Shared>, out: Arc<Output>) {
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, _) = recv(&out.sock, &mut buf).await;
        let data = &buf[..n];
        {
            let mut c = out.counters.lock().unwrap();
            c.packets_in += 1;
            c.bytes_in += n as u64;
            c.last_rx = Some(Instant::now());
        }
        if shared.read_only.lock().unwrap().contains(&out.target) {
            continue;
        }
        shared.stats.lock().unwrap().ingest(data, Instant::now());
        shared.record(data);
        shared.to_plugins(data, Side::Output(out.target));
        shared.to_vehicle(data).await;
        shared.to_outputs(data, Some(out.target)).await;
    }
}

#[derive(Debug, Clone)]
pub struct OutputStatus {
    pub target: SocketAddr,
    pub counters: OutputCounters,
    /// Something replied on this output's socket within SOURCE_TTL.
    pub gcs_connected: bool,
}

#[derive(Debug, Clone)]
pub struct RouterSnapshot {
    pub listen: SocketAddr,
    pub health: Health,
    pub vehicle: Option<VehicleStatus>,
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
                let counters = o.counters.lock().unwrap().clone();
                let gcs_connected = counters
                    .last_rx
                    .is_some_and(|t| now.saturating_duration_since(t) < SOURCE_TTL);
                OutputStatus {
                    target: o.target,
                    counters,
                    gcs_connected,
                }
            })
            .collect();
        let stats = self.shared.stats.lock().unwrap();
        RouterSnapshot {
            listen: self.local_addr(),
            health: stats.health(now),
            vehicle: stats.vehicle(now),
            outputs,
            crc_errors: stats.crc_errors,
        }
    }
}
