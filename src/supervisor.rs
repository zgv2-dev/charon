//! Owns the running router: applies config changes in order and reports config warnings.

use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use tokio::sync::watch;

use crate::{
    config::Config,
    router::{Router, RouterSnapshot},
};

pub struct Supervisor {
    router: Arc<Router>,
    cfg: watch::Sender<Config>,
    warnings: Arc<Mutex<Vec<String>>>,
}

impl Supervisor {
    /// Fails when the listen port is taken (check with `router::port_taken`). There is no retry:
    /// the user closes the GCS and starts the app again.
    pub async fn start(cfg: Config) -> io::Result<Supervisor> {
        let listen = cfg.listen;
        let router = Arc::new(Router::start(listen, &[]).await?);
        let (tx, rx) = watch::channel(cfg);
        let warnings = Arc::<Mutex<Vec<String>>>::default();
        tokio::spawn(run(rx, router.clone(), warnings.clone(), listen));
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
    // As configured, which with port 0 isn't what local_addr reports. Passed in: a change applied before this task
    // first runs is already in the channel.
    mut listen: SocketAddr,
) {
    let mut recording = false;
    loop {
        let cfg = rx.borrow_and_update().clone();
        let mut warns = Vec::new();
        if cfg.listen != listen {
            match router.set_listen(cfg.listen).await {
                Ok(()) => listen = cfg.listen,
                Err(e) => warns.push(format!(
                    "Can't listen on {}: {e}. Still on {}",
                    cfg.listen,
                    router.local_addr()
                )),
            }
        }
        let looping = cfg.looping_outputs(router.local_addr());
        let outputs: Vec<SocketAddr> = cfg
            .outputs
            .iter()
            .copied()
            .filter(|o| !looping.contains(o))
            .collect();
        warns.extend(
            looping.iter().map(|o| {
                format!("{o} can't be an output: it targets the listen port and would loop")
            }),
        );
        if let Err(e) = router.set_outputs(&outputs).await {
            warns.push(format!("Output setup failed: {e}"));
        }
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
