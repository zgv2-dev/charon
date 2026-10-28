//! TCP sources and outputs, and outputs that only take some systems.

use std::{net::SocketAddr, time::Duration};

use charon::{
    config::Output, endpoint::Endpoint, frame::testutil::heartbeat, link::State, router::Router,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    time::timeout,
};

async fn sock() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").await.unwrap()
}

fn out(s: &UdpSocket) -> Output {
    let a = s.local_addr().unwrap();
    Output::new(a.to_string(), Endpoint::UdpOut(a))
}

fn any_port() -> Endpoint {
    Endpoint::UdpIn("127.0.0.1:0".parse().unwrap())
}

async fn recv(s: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut b = [0u8; 2048];
    let (n, from) = timeout(Duration::from_secs(2), s.recv_from(&mut b))
        .await
        .expect("timed out")
        .unwrap();
    (b[..n].to_vec(), from)
}

/// Reads exactly `n` bytes from a TCP stream.
async fn read(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    timeout(Duration::from_secs(2), s.read_exact(&mut b))
        .await
        .expect("timed out")
        .unwrap();
    b
}

async fn wait_for(mut cond: impl FnMut() -> bool) {
    for _ in 0..150 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met within 3 s");
}

#[tokio::test]
async fn a_tcp_vehicle_reaches_udp_outputs_and_hears_replies() {
    let vehicle = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gcs = sock().await;
    let source = Endpoint::TcpClient(vehicle.local_addr().unwrap().to_string());
    let (_r, _) = Router::start(source, &[out(&gcs)]).await.unwrap();
    let (mut conn, _) = timeout(Duration::from_secs(2), vehicle.accept())
        .await
        .expect("the router connects")
        .unwrap();

    // a frame split across two writes arrives whole
    let hb = heartbeat(0, 1, 1, 2, 3);
    conn.write_all(&hb[..7]).await.unwrap();
    conn.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    conn.write_all(&hb[7..]).await.unwrap();
    let (got, router_port) = recv(&gcs).await;
    assert_eq!(got, hb);

    let reply = heartbeat(0, 255, 190, 6, 8);
    gcs.send_to(&reply, router_port).await.unwrap();
    assert_eq!(read(&mut conn, reply.len()).await, reply);
}

#[tokio::test]
async fn a_tcp_source_connects_again_after_a_drop() {
    let vehicle = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gcs = sock().await;
    let source = Endpoint::TcpClient(vehicle.local_addr().unwrap().to_string());
    let (r, _) = Router::start(source, &[out(&gcs)]).await.unwrap();
    let (first, _) = vehicle.accept().await.unwrap();
    wait_for(|| r.snapshot().source.unwrap().state == State::Connected).await;
    drop(first);
    wait_for(|| matches!(r.snapshot().source.unwrap().state, State::Retrying(_))).await;

    let (mut second, _) = timeout(Duration::from_secs(3), vehicle.accept())
        .await
        .expect("the router connects again")
        .unwrap();
    let hb = heartbeat(1, 1, 1, 2, 3);
    second.write_all(&hb).await.unwrap();
    assert_eq!(recv(&gcs).await.0, hb);
}

#[tokio::test]
async fn every_tcp_client_of_an_output_gets_everything() {
    let vehicle = sock().await;
    let logger = Output::new("Logger", "tcpin:127.0.0.1:0".parse().unwrap());
    let (r, failed) = Router::start(any_port(), &[logger]).await.unwrap();
    assert!(failed.is_empty());
    let port = r.snapshot().outputs[0].link.local_addr.unwrap();
    let mut a = TcpStream::connect(port).await.unwrap();
    let mut b = TcpStream::connect(port).await.unwrap();
    wait_for(|| r.snapshot().outputs[0].link.state == State::Serving { clients: 2 }).await;

    let hb = heartbeat(0, 1, 1, 2, 3);
    vehicle.send_to(&hb, r.local_addr().unwrap()).await.unwrap();
    assert_eq!(read(&mut a, hb.len()).await, hb);
    assert_eq!(read(&mut b, hb.len()).await, hb);

    // a client's frames go to the vehicle
    let reply = heartbeat(0, 255, 190, 6, 8);
    a.write_all(&reply).await.unwrap();
    assert_eq!(recv(&vehicle).await.0, reply);

    drop(b);
    wait_for(|| r.snapshot().outputs[0].link.state == State::Serving { clients: 1 }).await;
}

#[tokio::test]
async fn an_output_with_sysids_gets_only_those_systems() {
    let (only_one, all, vehicle) = (sock().await, sock().await, sock().await);
    let filtered = Output {
        sysids: vec![1],
        ..out(&only_one)
    };
    let (r, _) = Router::start(any_port(), &[filtered, out(&all)])
        .await
        .unwrap();
    let (one, two) = (heartbeat(0, 1, 1, 2, 3), heartbeat(0, 2, 1, 2, 3));
    let both = [one.clone(), two.clone()].concat();
    vehicle
        .send_to(&both, r.local_addr().unwrap())
        .await
        .unwrap();
    assert_eq!(recv(&only_one).await.0, one);
    assert_eq!(recv(&all).await.0, both, "untouched without a filter");

    // nothing at all for a datagram with none of its systems
    vehicle
        .send_to(&two, r.local_addr().unwrap())
        .await
        .unwrap();
    assert_eq!(recv(&all).await.0, two);
    let mut b = [0u8; 64];
    assert!(
        timeout(Duration::from_millis(200), only_one.recv_from(&mut b))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_disabled_output_isnt_opened() {
    let gcs = sock().await;
    let off = Output {
        enabled: false,
        ..out(&gcs)
    };
    let (r, _) = Router::start(any_port(), &[off]).await.unwrap();
    assert!(r.snapshot().outputs.is_empty());
}

#[tokio::test]
async fn renaming_an_output_keeps_its_socket() {
    let (gcs, vehicle) = (sock().await, sock().await);
    let (r, _) = Router::start(any_port(), &[out(&gcs)]).await.unwrap();
    let to = r.local_addr().unwrap();
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), to)
        .await
        .unwrap();
    let (_, before) = recv(&gcs).await;
    let renamed = Output {
        name: "Mission Planner".into(),
        ..out(&gcs)
    };
    assert!(r.set_outputs(&[renamed]).await.is_empty());
    vehicle
        .send_to(&heartbeat(1, 1, 1, 2, 3), to)
        .await
        .unwrap();
    let (_, after) = recv(&gcs).await;
    assert_eq!(before, after);
    assert_eq!(r.snapshot().outputs[0].name, "Mission Planner");
}

#[tokio::test]
async fn a_taken_output_port_leaves_only_that_output_out() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gcs = sock().await;
    let clash = Output::new("Logger", Endpoint::TcpServer(taken.local_addr().unwrap()));
    let (r, failed) = Router::start(any_port(), &[clash, out(&gcs)])
        .await
        .unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, "Logger");
    assert_eq!(r.snapshot().outputs.len(), 1);
}
