#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let r = charon::router::Router::start(
        "0.0.0.0:14551".parse().unwrap(),
        &["127.0.0.1:14550".parse().unwrap()],
    )
    .await
    .expect("bind 14551");
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        println!("{:#?}", r.snapshot());
    }
}
