//! Starting and stopping a server.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::chores::{Housekeeping, Release, Replicate, Streams};
use crate::config::ServerConfig;
use crate::db::{self, Chore, Db, DbHandle, DbThread};
use crate::http::{AppState, Inner, router};
use crate::{effects, logs, open, replica};

/// Holds a folder's lock for as long as the engine's thread runs.
struct Hold(#[allow(dead_code)] std::fs::File);

impl Chore for Hold {
    fn name(&self) -> &str {
        "lock"
    }
    fn due_in(&self, _: &Db) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
    fn run(&mut self, _: &mut Db) {}
    fn finish(&mut self, _: &mut Db) {}
}

/// What opening a database told the caller.
#[derive(Clone, Debug)]
pub struct Opened {
    /// The admin key in force.
    pub admin_key: String,
    /// Whether the key was just made (so it should be shown once).
    pub admin_key_made: bool,
    /// The replica pushed to, if any, as people read it.
    pub replica: Option<String>,
}

/// Opens the database on its own thread with the chores a server runs.
pub fn open_db(config: &ServerConfig) -> Result<(DbHandle, DbThread, Opened), String> {
    let cfg = config.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let (handle, thread) = db::spawn("main", move || {
        let (opened, made) = open::open(&cfg)?;
        let open::Opened {
            engine,
            capture,
            writer_capture,
            admin_key,
            lock,
        } = opened;
        let mut chores: Vec<Box<dyn Chore>> =
            vec![Box::new(Hold(lock)), Box::new(Streams::default())];
        if !cfg.follower {
            chores.push(Box::new(Housekeeping::default()));
        }
        let settings = engine.settings();
        let spec = match &cfg.replica {
            Some(s) => Some((s.clone(), None)),
            None => replica::from_settings(&settings).map(|(s, k)| (s, Some(k))),
        };
        let described = match spec {
            Some((spec, keys)) => {
                let r = replica::open(&spec, keys.as_ref(), None)?;
                let d = r.describe();
                let label = format!("moul serve {}", cfg.dir.display());
                chores.push(Box::new(Replicate::new(
                    r,
                    capture,
                    writer_capture,
                    cfg.dir.join("conflicts"),
                    &label,
                    cfg.follower,
                )));
                Some(d)
            }
            None => {
                chores.push(Box::new(Release::new(capture, writer_capture)));
                None
            }
        };
        let _ = tx.send(Opened {
            admin_key,
            admin_key_made: made,
            replica: described,
        });
        Ok((Db::new(engine), chores))
    })?;
    let opened = rx
        .recv()
        .map_err(|_| "the database did not open".to_owned())?;
    Ok((handle, thread, opened))
}

/// A running server.
pub struct Running {
    /// Where it listens.
    pub addr: SocketAddr,
    /// Its shared state.
    pub state: AppState,
    /// What opening told.
    pub opened: Opened,
    shutdown: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
    thread: DbThread,
}

impl Running {
    /// Stops taking requests, finishes the queue and the chores (the last
    /// push to the replica), and closes the database.
    pub async fn stop(self) {
        // Live streams last until told to end; end them first, or the
        // server would wait for them forever.
        let _ = self
            .state
            .db
            .run(|db| db.end_streams("the server is shutting down"))
            .await;
        let _ = self.shutdown.send(());
        let mut serving = self.serving;
        if tokio::time::timeout(std::time::Duration::from_secs(10), &mut serving)
            .await
            .is_err()
        {
            serving.abort();
        }
        self.state.db.close();
        let thread = self.thread;
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
    }
}

/// Builds the state every handler shares.
pub async fn state(config: ServerConfig, db: DbHandle, admin_key: String) -> AppState {
    let state = AppState(Arc::new(Inner {
        db,
        outbound: crate::net::client(config.allow_private_calls),
        config,
        admin_key,
        settings: RwLock::new(Arc::default()),
        tickets: Default::default(),
        limiter: Default::default(),
        log: Default::default(),
        metrics: Default::default(),
        auth: Default::default(),
        started: Instant::now(),
    }));
    state.reload_settings().await;
    state
}

/// Starts a server: opens the database, binds, and serves until stopped.
pub async fn start(config: ServerConfig) -> Result<Running, String> {
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.listen))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let (db, thread, opened) = open_db(&config)?;
    let mut config = config;
    config.listen = addr;
    let effects_on = config.effects && !config.follower;
    let state = state(config, db, opened.admin_key.clone()).await;
    state.log.spawn_writer(state.clone());
    logs::Metrics::spawn_sampler(state.clone());
    if effects_on {
        effects::spawn(state.clone());
    }
    let app = router(state.clone());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = rx.await;
        })
        .await;
    });
    Ok(Running {
        addr,
        state,
        opened,
        shutdown: tx,
        serving,
        thread,
    })
}

/// Waits for Ctrl-C or SIGTERM.
pub async fn signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
}
