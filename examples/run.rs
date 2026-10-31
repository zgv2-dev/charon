#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    // cargo run --example run -- serial:/dev/ttyUSB0:57600
    let source = std::env::args()
        .nth(1)
        .unwrap_or("udpin:0.0.0.0:14551".into())
        .parse()
        .expect("a source like udpin:0.0.0.0:14551");
    let gcs = charon::config::Output::new("GCS", "udpout:127.0.0.1:14550".parse().unwrap());
    let (r, _) = charon::router::Router::start(source, &[gcs])
        .await
        .expect("open the source");
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        println!("{:#?}", r.snapshot());
    }
}
