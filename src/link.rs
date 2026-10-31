//! One open endpoint: a task that reads it, hands what arrives to the router, and writes what the router sends.
//! Writes go through a queue, so a slow TCP client or serial port never holds up the routing path.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
};

use crate::{endpoint::Endpoint, platform, stream::Deframer};

/// A UDP peer is forgotten after this much silence.
pub const PEER_TTL: Duration = Duration::from_secs(5);
/// Datagrams waiting to be written; past this they are dropped and counted.
const QUEUE: usize = 256;
/// Between attempts to reach a TCP server or reopen a serial port.
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Called with every datagram (or batch of whole frames, from a stream) the link receives.
pub type OnReceive = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Debug, Default, Clone)]
pub struct Counters {
    pub packets_out: u64,
    pub bytes_out: u64,
    pub packets_in: u64,
    pub bytes_in: u64,
    pub last_tx: Option<Instant>,
    pub last_rx: Option<Instant>,
    /// Sent while the link was down or its queue full.
    pub dropped: u64,
    /// Stream bytes that weren't MAVLink, or frames with a bad CRC.
    pub skipped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// UDP: bound, and the peer it last heard from, if any recently.
    Udp { peer: Option<SocketAddr> },
    /// A TCP server with this many clients.
    Serving { clients: usize },
    /// A TCP client or serial port, open.
    Connected,
    /// Trying again after this error.
    Retrying(String),
}

pub struct Link {
    pub endpoint: Endpoint,
    tx: mpsc::Sender<Vec<u8>>,
    counters: Arc<Mutex<Counters>>,
    state: Arc<Mutex<State>>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// Where a UDP link listens, for a port 0 bind.
    local: Option<SocketAddr>,
}

impl Link {
    /// Binds now, so a taken port fails here; a TCP client and a serial port connect in the background and keep
    /// trying.
    pub async fn open(endpoint: Endpoint, on_rx: OnReceive) -> io::Result<Arc<Link>> {
        let (tx, rx) = mpsc::channel(QUEUE);
        let counters = Arc::<Mutex<Counters>>::default();
        let ctx = Ctx {
            counters: counters.clone(),
            state: Arc::new(Mutex::new(State::Retrying("starting".into()))),
            on_rx,
        };
        let state = ctx.state.clone();
        let (task, local) = match &endpoint {
            Endpoint::UdpIn(addr) => {
                let sock = UdpSocket::from_std(platform::bind_udp_exclusive(*addr)?)?;
                let local = sock.local_addr()?;
                *state.lock().unwrap() = State::Udp { peer: None };
                (tokio::spawn(udp_in(sock, rx, ctx)), Some(local))
            }
            Endpoint::UdpOut(target) => {
                let sock = UdpSocket::bind(SocketAddr::new(ephemeral_ip(target), 0)).await?;
                let local = sock.local_addr()?;
                *state.lock().unwrap() = State::Udp { peer: None };
                (tokio::spawn(udp_out(sock, *target, rx, ctx)), Some(local))
            }
            Endpoint::TcpServer(addr) => {
                let listener = TcpListener::bind(addr).await?;
                let local = listener.local_addr()?;
                *state.lock().unwrap() = State::Serving { clients: 0 };
                (tokio::spawn(tcp_server(listener, rx, ctx)), Some(local))
            }
            Endpoint::TcpClient(addr) => (tokio::spawn(tcp_client(addr.clone(), rx, ctx)), None),
            Endpoint::Serial { port, baud } => {
                (tokio::spawn(serial(port.clone(), *baud, rx, ctx)), None)
            }
        };
        Ok(Arc::new(Link {
            endpoint,
            tx,
            counters,
            state,
            task: Mutex::new(Some(task)),
            local,
        }))
    }

    /// Queues `data` to be written; never waits.
    pub fn send(&self, data: &[u8]) {
        if self.tx.try_send(data.to_vec()).is_err() {
            self.counters.lock().unwrap().dropped += 1;
        }
    }

    pub fn counters(&self) -> Counters {
        self.counters.lock().unwrap().clone()
    }

    pub fn state(&self) -> State {
        let mut s = self.state.lock().unwrap().clone();
        // the peer goes stale without an event to clear it
        if let State::Udp { peer } = &mut s
            && self
                .counters
                .lock()
                .unwrap()
                .last_rx
                .is_none_or(|t| t.elapsed() >= PEER_TTL)
        {
            *peer = None;
        }
        s
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local
    }

    pub async fn close(&self) {
        let task = self.task.lock().unwrap().take();
        if let Some(t) = task {
            t.abort();
            let _ = t.await;
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(t) = self.task.lock().unwrap().take() {
            t.abort();
        }
    }
}

#[derive(Clone)]
struct Ctx {
    counters: Arc<Mutex<Counters>>,
    state: Arc<Mutex<State>>,
    on_rx: OnReceive,
}

impl Ctx {
    fn received(&self, data: &[u8]) {
        {
            let mut c = self.counters.lock().unwrap();
            c.packets_in += 1;
            c.bytes_in += data.len() as u64;
            c.last_rx = Some(Instant::now());
        }
        (self.on_rx)(data);
    }

    fn sent(&self, n: usize) {
        let mut c = self.counters.lock().unwrap();
        c.packets_out += 1;
        c.bytes_out += n as u64;
        c.last_tx = Some(Instant::now());
    }

    fn dropped(&self) {
        self.counters.lock().unwrap().dropped += 1;
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap() = s;
    }
}

/// An unspecified address of the target's family, or loopback for a loopback target.
fn ephemeral_ip(target: &SocketAddr) -> IpAddr {
    match target.ip() {
        ip if ip.is_loopback() => ip,
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
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

async fn udp_in(sock: UdpSocket, mut rx: mpsc::Receiver<Vec<u8>>, ctx: Ctx) {
    let mut buf = vec![0u8; 65_536];
    let mut peer: Option<(SocketAddr, Instant)> = None;
    loop {
        tokio::select! {
            (n, from) = recv(&sock, &mut buf) => {
                if peer.is_none_or(|(p, _)| p != from) {
                    ctx.set(State::Udp { peer: Some(from) });
                }
                peer = Some((from, Instant::now()));
                ctx.received(&buf[..n]);
            }
            Some(data) = rx.recv() => match peer.filter(|(_, seen)| seen.elapsed() < PEER_TTL) {
                Some((to, _)) if sock.send_to(&data, to).await.is_ok() => ctx.sent(data.len()),
                _ => ctx.dropped(),
            },
        }
    }
}

async fn udp_out(sock: UdpSocket, target: SocketAddr, mut rx: mpsc::Receiver<Vec<u8>>, ctx: Ctx) {
    let mut buf = vec![0u8; 65_536];
    loop {
        tokio::select! {
            (n, from) = recv(&sock, &mut buf) => {
                ctx.set(State::Udp { peer: Some(from) });
                ctx.received(&buf[..n]);
            }
            Some(data) = rx.recv() => {
                if sock.send_to(&data, target).await.is_ok() {
                    ctx.sent(data.len());
                }
            }
        }
    }
}

/// Reads frames from `stream` and writes what is queued, until either side fails.
async fn duplex<S: AsyncRead + AsyncWrite>(
    stream: S,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    ctx: &Ctx,
) -> io::Error {
    let (mut read, mut write) = tokio::io::split(stream);
    let mut deframer = Deframer::default();
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            r = read.read(&mut buf) => match r {
                Ok(0) => return io::Error::new(io::ErrorKind::UnexpectedEof, "closed by the other end"),
                Ok(n) => {
                    let before = deframer.skipped;
                    for d in deframer.push(&buf[..n]) {
                        ctx.received(&d);
                    }
                    ctx.counters.lock().unwrap().skipped += deframer.skipped - before;
                }
                Err(e) => return e,
            },
            Some(data) = rx.recv() => match write.write_all(&data).await {
                Ok(()) => ctx.sent(data.len()),
                Err(e) => return e,
            },
        }
    }
}

/// Waits before the next attempt, dropping what is sent meanwhile: stale telemetry is worse than none.
async fn wait_retry(rx: &mut mpsc::Receiver<Vec<u8>>, ctx: &Ctx, delay: &mut Duration) {
    let until = tokio::time::Instant::now() + *delay;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => break,
            Some(_) = rx.recv() => ctx.dropped(),
        }
    }
    *delay = (*delay * 2).min(RETRY_MAX);
}

async fn tcp_client(addr: String, mut rx: mpsc::Receiver<Vec<u8>>, ctx: Ctx) {
    let mut delay = RETRY_MIN;
    loop {
        let err = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                tracing::info!("connected to tcp:{addr}");
                ctx.set(State::Connected);
                delay = RETRY_MIN;
                duplex(stream, &mut rx, &ctx).await
            }
            Ok(Err(e)) => e,
            Err(_) => io::Error::new(io::ErrorKind::TimedOut, "no answer"),
        };
        tracing::info!("tcp:{addr}: {err}");
        ctx.set(State::Retrying(err.to_string()));
        wait_retry(&mut rx, &ctx, &mut delay).await;
    }
}

async fn tcp_server(listener: TcpListener, mut rx: mpsc::Receiver<Vec<u8>>, ctx: Ctx) {
    let mut clients: Vec<mpsc::Sender<Vec<u8>>> = Vec::new();
    // counted by the clients' own tasks too, so a client leaving shows without waiting for the next datagram
    let connected = Arc::new(AtomicUsize::new(0));
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, from)) => {
                    let _ = stream.set_nodelay(true);
                    tracing::info!("tcp client {from} connected");
                    let (tx, mut client_rx) = mpsc::channel(QUEUE);
                    clients.push(tx);
                    let n = connected.fetch_add(1, Ordering::Relaxed) + 1;
                    ctx.set(State::Serving { clients: n });
                    let (ctx, connected) = (ctx.clone(), connected.clone());
                    tokio::spawn(async move {
                        let e = duplex(stream, &mut client_rx, &ctx).await;
                        tracing::info!("tcp client {from}: {e}");
                        let n = connected.fetch_sub(1, Ordering::Relaxed) - 1;
                        ctx.set(State::Serving { clients: n });
                    });
                }
                Err(e) => {
                    tracing::warn!("tcp accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(data) = rx.recv() => {
                // a client whose task ended has a closed queue; one that is only slow loses this datagram
                clients.retain(|c| !c.is_closed());
                if clients.is_empty() {
                    ctx.dropped();
                }
                for c in &clients {
                    if c.try_send(data.clone()).is_err() {
                        ctx.dropped();
                    }
                }
            }
        }
    }
}

async fn serial(port: String, baud: u32, mut rx: mpsc::Receiver<Vec<u8>>, ctx: Ctx) {
    use tokio_serial::SerialPortBuilderExt;
    let mut delay = RETRY_MIN;
    loop {
        let err = match tokio_serial::new(&port, baud).open_native_async() {
            Ok(stream) => {
                tracing::info!("opened {port} at {baud}");
                ctx.set(State::Connected);
                delay = RETRY_MIN;
                duplex(stream, &mut rx, &ctx).await
            }
            Err(e) => io::Error::other(e),
        };
        tracing::info!("serial {port}: {err}");
        ctx.set(State::Retrying(err.to_string()));
        wait_retry(&mut rx, &ctx, &mut delay).await;
    }
}
