//! Owns the running router: applies config changes in order and reports config warnings.

use std::{
    io,
    sync::{Arc, Mutex},
};

use tokio::sync::watch;

use crate::{
    config::{Config, Output},
    endpoint::Endpoint,
    router::{Router, RouterSnapshot},
};

pub struct Supervisor {
    router: Arc<Router>,
    cfg: watch::Sender<Config>,
    warnings: Arc<Mutex<Vec<String>>>,
}

impl Supervisor {
    /// Fails when the source can't be opened, normally a taken port (check with `router::port_taken`). There is no
    /// retry: the user closes whatever holds it and starts the app again. A TCP client or serial source never fails
    /// here; it keeps trying in the background.
    pub async fn start(cfg: Config) -> io::Result<Supervisor> {
        let source = cfg.source.clone();
        let (router, _) = Router::start(source.clone(), &[]).await?;
        let router = Arc::new(router);
        let (tx, rx) = watch::channel(cfg);
        let warnings = Arc::<Mutex<Vec<String>>>::default();
        tokio::spawn(run(rx, router.clone(), warnings.clone(), source));
        Ok(Supervisor {
            router,
            cfg: tx,
            warnings,
        })
    }

    pub fn apply(&self, cfg: Config) {
        self.cfg.send_replace(cfg);
    }

    pub fn snapshot(&self) -> RouterSnapshot {
        self.router.snapshot()
    }

    /// From now on `plugin` sees every datagram.
    pub fn add_plugin(&self, plugin: Arc<dyn crate::plugin::Plugin>) {
        self.router.add_plugin(plugin);
    }

    pub fn cut_tlog(&self) -> std::io::Result<Option<std::path::PathBuf>> {
        self.router.cut_tlog()
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().unwrap().clone()
    }
}

async fn run(
    mut rx: watch::Receiver<Config>,
    router: Arc<Router>,
    warnings: Arc<Mutex<Vec<String>>>,
    // As configured, which with port 0 isn't what the link reports. Passed in: a change applied before this task
    // first runs is already in the channel.
    mut source: Endpoint,
) {
    let mut recording = false;
    loop {
        let cfg = rx.borrow_and_update().clone();
        let mut warns = Vec::new();
        if cfg.source != source {
            match router.set_source(cfg.source.clone()).await {
                Ok(()) => source = cfg.source.clone(),
                Err(e) => warns.push(format!(
                    "Can't open {}: {e}. Still on {}",
                    cfg.source,
                    actual(&source, &router)
                )),
            }
        }
        let actual = actual(&source, &router);
        let (looping, outputs): (Vec<&Output>, Vec<&Output>) = cfg
            .outputs
            .iter()
            .partition(|o| o.endpoint.reaches(&actual));
        warns.extend(looping.iter().map(|o| {
            format!(
                "{} ({}) can't be an output: it sends to the source and would loop",
                o.name, o.endpoint
            )
        }));
        let outputs: Vec<Output> = outputs.into_iter().cloned().collect();
        for (name, e) in router.set_outputs(&outputs).await {
            warns.push(format!("Can't open output {name}: {e}"));
        }
        router.set_follow(cfg.follow);
        if cfg.record_tlog != recording {
            match router.set_tlog(cfg.record_tlog.then(crate::platform::tlog_dir).as_deref()) {
                Ok(()) => recording = cfg.record_tlog,
                Err(e) => warns.push(format!("Can't record tlog: {e}")),
            }
        }
        {
            let mut w = warnings.lock().unwrap();
            if *w != warns {
                warns.iter().for_each(|x| tracing::warn!("{x}"));
                *w = warns;
            }
        }
        if rx.changed().await.is_err() {
            return; // config sender dropped: app is closing
        }
    }
}

/// The source as it is: a port 0 bind got a real port.
fn actual(source: &Endpoint, router: &Router) -> Endpoint {
    match (source, router.local_addr()) {
        (Endpoint::UdpIn(_), Some(a)) => Endpoint::UdpIn(a),
        (Endpoint::TcpServer(_), Some(a)) => Endpoint::TcpServer(a),
        (s, _) => s.clone(),
    }
}
