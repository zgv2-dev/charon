use std::{net::SocketAddr, time::Duration};

use charon::{
    config::Config, frame::testutil::heartbeat, router::port_taken, supervisor::Supervisor,
};
use tokio::net::UdpSocket;

async fn wait_for(mut cond: impl FnMut() -> bool) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met within 2 s");
}

fn cfg(outputs: Vec<SocketAddr>) -> Config {
    Config {
        listen: any_port(),
        outputs,
        ..Config::default()
    }
}

fn any_port() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

#[tokio::test]
async fn start_fails_when_port_taken() {
    let gcs = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let err = Supervisor::start(Config {
        listen: gcs.local_addr().unwrap(),
        ..cfg(vec![])
    })
    .await
    .err()
    .expect("must fail");
    assert!(port_taken(&err), "{err}");
}

#[tokio::test]
async fn looping_output_is_skipped_with_warning() {
    let sup = Supervisor::start(cfg(vec![])).await.unwrap();
    let listen = sup.snapshot().listen;
    sup.apply(cfg(vec![listen]));
    wait_for(|| !sup.warnings().is_empty()).await;
    assert!(sup.snapshot().outputs.is_empty());
    assert_eq!(
        sup.warnings(),
        vec![format!(
            "{listen} can't be an output: it targets the listen port and would loop"
        )]
    );
}

#[tokio::test]
async fn apply_adds_output_without_blip() {
    let gcs = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let extra = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut c = cfg(vec![gcs.local_addr().unwrap()]);
    let sup = Supervisor::start(c.clone()).await.unwrap();
    wait_for(|| sup.snapshot().outputs.len() == 1).await;
    let listen = sup.snapshot().listen;

    let vehicle = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0u8; 512];
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), listen)
        .await
        .unwrap();
    let (_, before) = gcs.recv_from(&mut buf).await.unwrap();

    c.outputs.push(extra.local_addr().unwrap());
    sup.apply(c);
    wait_for(|| sup.snapshot().outputs.len() == 2).await;
    vehicle
        .send_to(&heartbeat(1, 1, 1, 2, 3), listen)
        .await
        .unwrap();
    let (_, after) = gcs.recv_from(&mut buf).await.unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn listen_moves_and_outputs_carry_on() {
    let gcs = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut c = cfg(vec![gcs.local_addr().unwrap()]);
    let sup = Supervisor::start(c.clone()).await.unwrap();
    wait_for(|| sup.snapshot().outputs.len() == 1).await;
    let free = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap(); // socket dropped: port free
    c.listen = free;
    sup.apply(c);
    wait_for(|| sup.snapshot().listen == free).await;

    let vehicle = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), free)
        .await
        .unwrap();
    let mut buf = [0u8; 512];
    tokio::time::timeout(Duration::from_secs(2), gcs.recv_from(&mut buf))
        .await
        .expect("forwarded from the new listen address")
        .unwrap();
    assert!(sup.warnings().is_empty(), "{:?}", sup.warnings());
}

#[tokio::test]
async fn taken_listen_port_keeps_the_old_one_with_warning() {
    let mut c = cfg(vec![]);
    let sup = Supervisor::start(c.clone()).await.unwrap();
    let old = sup.snapshot().listen;
    let gcs = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    c.listen = gcs.local_addr().unwrap();
    sup.apply(c.clone());
    wait_for(|| !sup.warnings().is_empty()).await;
    let w = &sup.warnings()[0];
    assert!(
        w.starts_with(&format!("Can't listen on {}: ", c.listen)),
        "{w}"
    );
    assert!(w.ends_with(&format!("Still on {old}")), "{w}");
    assert_eq!(sup.snapshot().listen, old);

    let vehicle = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    vehicle
        .send_to(&heartbeat(0, 1, 1, 2, 3), old)
        .await
        .unwrap();
    wait_for(|| sup.snapshot().vehicle.is_some()).await;
}
