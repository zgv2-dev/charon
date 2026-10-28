use std::{net::SocketAddr, time::Duration};

use charon::{
    config::Output,
    endpoint::Endpoint,
    frame::testutil::heartbeat,
    router::{Router, port_taken},
};
use tokio::{net::UdpSocket, time::timeout};

async fn sock() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").await.unwrap()
}

async fn recv(s: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut b = [0u8; 2048];
    let (n, from) = timeout(Duration::from_secs(1), s.recv_from(&mut b))
        .await
        .expect("timed out")
        .unwrap();
    (b[..n].to_vec(), from)
}

async fn nothing(s: &UdpSocket) {
    let mut b = [0u8; 2048];
    assert!(
        timeout(Duration::from_millis(200), s.recv_from(&mut b))
            .await
            .is_err(),
        "unexpected packet"
    );
}

fn out(s: &UdpSocket) -> Output {
    out_to(s.local_addr().unwrap())
}

fn out_to(addr: SocketAddr) -> Output {
    Output::new(addr.to_string(), Endpoint::UdpOut(addr))
}

fn any_port() -> Endpoint {
    Endpoint::UdpIn("127.0.0.1:0".parse().unwrap())
}

async fn router(outputs: &[&UdpSocket]) -> Router {
    let outputs: Vec<Output> = outputs.iter().map(|s| out(s)).collect();
    let (r, failed) = Router::start(any_port(), &outputs).await.unwrap();
    assert!(failed.is_empty());
    r
}

#[tokio::test]
async fn vehicle_frames_reach_every_output() {
    let (gcs, tel, vehicle) = (sock().await, sock().await, sock().await);
    let r = router(&[&gcs, &tel]).await;
    let hb = heartbeat(0, 1, 1, 2, 3);
    vehicle.send_to(&hb, r.local_addr().unwrap()).await.unwrap();
    assert_eq!(recv(&gcs).await.0, hb);
    assert_eq!(recv(&tel).await.0, hb);
}

#[tokio::test]
async fn gcs_reply_goes_to_vehicle_and_other_outputs_not_back() {
    let (gcs, tel, vehicle) = (sock().await, sock().await, sock().await);
    let r = router(&[&gcs, &tel]).await;
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    let (_, gcs_port) = recv(&gcs).await;
    recv(&tel).await;

    let reply = heartbeat(0, 255, 190, 6, 8);
    gcs.send_to(&reply, gcs_port).await.unwrap();
    assert_eq!(recv(&vehicle).await.0, reply);
    assert_eq!(recv(&tel).await.0, reply);
    nothing(&gcs).await;
}

#[tokio::test]
async fn a_read_only_output_reaches_neither_the_vehicle_nor_other_outputs() {
    let (gcs, tel, vehicle) = (sock().await, sock().await, sock().await);
    let (r, _) = Router::start(any_port(), &[out(&gcs), out(&tel).read_only()])
        .await
        .unwrap();
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    recv(&gcs).await;
    let (hb, tel_port) = recv(&tel).await;
    assert_eq!(hb, heartbeat(0, 1, 1, 2, 3), "it still hears the vehicle");

    tel.send_to(&heartbeat(0, 255, 190, 6, 8), tel_port)
        .await
        .unwrap();
    nothing(&vehicle).await;
    nothing(&gcs).await;
    let snap = r.snapshot();
    let out = snap
        .outputs
        .iter()
        .find(|o| o.link.endpoint == Endpoint::UdpOut(tel.local_addr().unwrap()))
        .unwrap();
    assert_eq!(out.link.counters.packets_in, 1, "counted");
}

#[tokio::test]
async fn unknown_and_garbage_bytes_forwarded_untouched() {
    let (gcs, vehicle) = (sock().await, sock().await);
    let r = router(&[&gcs]).await;
    let junk = [0x00, 0xFD, 0x01, 0x02, 0x03];
    vehicle
        .send_to(&junk, r.local_addr().unwrap())
        .await
        .unwrap();
    assert_eq!(recv(&gcs).await.0, junk);
}

#[tokio::test]
async fn forwarding_survives_a_vanished_peer() {
    // Windows reports ICMP port-unreachable as WSAECONNRESET on the next recv; loops must keep going.
    let dead = sock().await.local_addr().unwrap(); // socket dropped: port closed
    let gcs = sock().await;
    let (r, _) = Router::start(any_port(), &[out_to(dead), out(&gcs)])
        .await
        .unwrap();

    let old_vehicle = sock().await;
    old_vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    let (_, gcs_port) = recv(&gcs).await;
    drop(old_vehicle);
    gcs.send_to(&heartbeat(0, 255, 190, 6, 8), gcs_port)
        .await
        .unwrap(); // goes to the closed vehicle port
    tokio::time::sleep(Duration::from_millis(100)).await;

    let vehicle = sock().await;
    for seq in 1..5 {
        let hb = heartbeat(seq, 1, 1, 2, 3);
        vehicle.send_to(&hb, r.local_addr().unwrap()).await.unwrap();
        assert_eq!(recv(&gcs).await.0, hb);
    }
}

#[tokio::test]
async fn replies_reach_a_vehicle_that_changed_port() {
    let gcs = sock().await;
    let r = router(&[&gcs]).await;
    let (first, second) = (sock().await, sock().await);
    first
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    let (_, gcs_port) = recv(&gcs).await;
    second
        .send_to(&heartbeat(1, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    recv(&gcs).await;

    let reply = heartbeat(0, 255, 190, 6, 8);
    gcs.send_to(&reply, gcs_port).await.unwrap();
    assert_eq!(recv(&second).await.0, reply);
}

#[tokio::test]
async fn set_outputs_keeps_unchanged_output_socket() {
    let (gcs, tel, vehicle) = (sock().await, sock().await, sock().await);
    let r = router(&[&gcs]).await;
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    let (_, before) = recv(&gcs).await;

    assert!(r.set_outputs(&[out(&gcs), out(&tel)]).await.is_empty());
    vehicle
        .send_to(&heartbeat(1, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    let (_, after) = recv(&gcs).await;
    assert_eq!(before, after, "GCS link must not blip");
    recv(&tel).await;
}

#[tokio::test]
async fn listen_port_cannot_be_shared() {
    let r = router(&[]).await;
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
    s.set_reuse_address(true).unwrap(); // what a GCS might do; SO_EXCLUSIVEADDRUSE must still win on Windows
    let err = s.bind(&r.local_addr().unwrap().into()).unwrap_err();
    assert!(port_taken(&err), "{err}");
}

#[tokio::test]
async fn start_reports_taken_port() {
    let gcs = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let err = Router::start(Endpoint::UdpIn(gcs.local_addr().unwrap()), &[])
        .await
        .err()
        .expect("bind must fail");
    assert!(port_taken(&err), "{err}");
}

#[tokio::test]
async fn snapshot_reports_vehicle_and_gcs() {
    let (gcs, vehicle) = (sock().await, sock().await);
    let r = router(&[&gcs]).await;
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    let (_, gcs_port) = recv(&gcs).await;
    gcs.send_to(&heartbeat(0, 255, 190, 6, 8), gcs_port)
        .await
        .unwrap();
    recv(&vehicle).await;

    let snap = r.snapshot();
    assert_eq!(snap.source.unwrap().local_addr, r.local_addr());
    assert_eq!(snap.vehicle.unwrap().sysid, 1);
    assert_eq!(snap.health, charon::stats::Health::Ok);
    let out = &snap.outputs[0];
    assert!(out.gcs_connected);
    let c = &out.link.counters;
    assert_eq!((c.packets_out, c.packets_in), (1, 1));
    assert_eq!(snap.vehicles.len(), 1);
}

#[tokio::test]
async fn records_both_directions_to_tlog() {
    let dir = tempfile::tempdir().unwrap();
    let (gcs, vehicle) = (sock().await, sock().await);
    let r = router(&[&gcs]).await;
    r.set_tlog(Some(dir.path())).unwrap();
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    let (_, gcs_port) = recv(&gcs).await;
    gcs.send_to(&heartbeat(0, 255, 190, 6, 8), gcs_port)
        .await
        .unwrap();
    recv(&vehicle).await;
    r.set_tlog(None).unwrap();

    let file = std::fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = std::fs::read(file).unwrap();
    let sysids: Vec<u8> = charon::tlog::read(&bytes)
        .iter()
        .map(|(_, f)| f[5])
        .collect();
    assert_eq!(sysids, [1, 255]);
}

#[tokio::test]
async fn cut_closes_the_tlog_and_records_on_in_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let vehicle = sock().await;
    let r = router(&[]).await;
    assert!(r.cut_tlog().unwrap().is_none());
    r.set_tlog(Some(dir.path())).unwrap();
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), r.local_addr().unwrap())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await; // file names are per second
    let closed = r.cut_tlog().unwrap().unwrap();
    assert_eq!(
        charon::tlog::read(&std::fs::read(&closed).unwrap()).len(),
        1
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[derive(Default)]
struct Seen(std::sync::Mutex<Vec<(Vec<u8>, charon::plugin::Side)>>);

impl charon::plugin::Plugin for Seen {
    fn datagram(&self, data: &[u8], from: charon::plugin::Side) {
        self.0.lock().unwrap().push((data.to_vec(), from));
    }
}

#[tokio::test]
async fn plugins_see_both_directions() {
    let (gcs, vehicle) = (sock().await, sock().await);
    let r = router(&[&gcs]).await;
    let seen = std::sync::Arc::new(Seen::default());
    r.add_plugin(seen.clone());
    let hb = heartbeat(0, 1, 1, 2, 3);
    vehicle.send_to(&hb, r.local_addr().unwrap()).await.unwrap();
    let (_, router_port) = recv(&gcs).await;
    let reply = heartbeat(0, 255, 190, 6, 8);
    gcs.send_to(&reply, router_port).await.unwrap();
    recv(&vehicle).await;
    let id = r.snapshot().outputs[0].id;
    assert_eq!(
        *seen.0.lock().unwrap(),
        [
            (hb, charon::plugin::Side::Vehicle),
            (reply, charon::plugin::Side::Output(id)),
        ]
    );
}
