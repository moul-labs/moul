#![doc = include_str!("../README.md")]
// Handlers return an HTTP response as their error, as axum does.
#![allow(clippy::result_large_err)]

#[cfg(not(target_arch = "wasm32"))]
pub mod api;
#[cfg(not(target_arch = "wasm32"))]
pub mod directory;
#[cfg(not(target_arch = "wasm32"))]
pub mod store;

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;

    use moul_server::ServerConfig;

    use crate::api::{Hub, Signup};
    use crate::store::Store;

    /// Settings of a hub.
    #[derive(Clone, Debug)]
    pub struct HubConfig {
        /// The hub's folder: the directory database in `directory/`, and
        /// replicas in `replicas/` unless `store` says otherwise.
        pub dir: PathBuf,
        /// Where to listen.
        pub listen: SocketAddr,
        /// The address devices use.
        pub public_url: String,
        /// Where replicas live; `<dir>/replicas` when `None`.
        pub store: Option<Store>,
        /// Who may sign up.
        pub signup: Signup,
        /// Databases per account.
        pub max_databases: i64,
        /// Bytes per account.
        pub max_bytes: i64,
        /// The admin key, for `/api/hub/admin` and the directory's settings.
        pub admin_key: Option<String>,
        /// Proxies believed about the client's address.
        pub trusted_proxies: Vec<moul_server::config::Trusted>,
    }

    impl Default for HubConfig {
        fn default() -> Self {
            HubConfig {
                dir: PathBuf::from("hub-data"),
                listen: SocketAddr::from(([127, 0, 0, 1], 8091)),
                public_url: String::new(),
                store: None,
                signup: Signup::Closed,
                max_databases: 10,
                max_bytes: 1 << 30,
                admin_key: None,
                trusted_proxies: vec![moul_server::config::Trusted::Loopback],
            }
        }
    }

    /// A running hub.
    pub struct RunningHub {
        /// Where it listens.
        pub addr: SocketAddr,
        /// The directory's server state.
        pub state: moul_server::http::AppState,
        /// What opening the directory told.
        pub opened: moul_server::server::Opened,
        /// Where replicas live, as people read it.
        pub store: String,
        /// The hub's own state.
        pub hub: Arc<Hub>,
        shutdown: tokio::sync::oneshot::Sender<()>,
        serving: tokio::task::JoinHandle<()>,
        thread: moul_server::db::DbThread,
    }

    impl RunningHub {
        /// Stops taking requests and closes the directory.
        pub async fn stop(self) {
            // Event streams last until told to end; end them first.
            self.hub.end_events();
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

    /// Starts a hub.
    pub async fn start(config: HubConfig) -> Result<RunningHub, String> {
        let listener = tokio::net::TcpListener::bind(config.listen)
            .await
            .map_err(|e| format!("cannot listen on {}: {e}", config.listen))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let store = config
            .store
            .clone()
            .unwrap_or_else(|| Store::Dir(config.dir.join("replicas")));
        let store_spec = store.describe();
        let server = ServerConfig {
            dir: config.dir.join("directory"),
            listen: addr,
            public_url: config.public_url.clone(),
            admin_key: config.admin_key.clone(),
            trusted_proxies: config.trusted_proxies.clone(),
            // Snapshots of large databases are single objects.
            max_body_bytes: (256 << 20) + (1 << 20),
            ..ServerConfig::default()
        };
        let (db, thread, opened) = moul_server::server::open_db(&server)?;
        db.run(crate::directory::prepare)
            .await
            .map_err(|e| e.to_string())??;
        let state = moul_server::server::state(server, db, opened.admin_key.clone()).await;
        state.log.spawn_writer(state.clone());
        moul_server::effects::spawn(state.clone());
        let hub = Arc::new(Hub::new(
            store,
            config.signup,
            config.max_databases,
            config.max_bytes,
        ));
        crate::api::spawn_maintenance(state.clone(), hub.clone());
        let app =
            moul_server::http::router_with(state.clone(), crate::api::routes(hub.clone()), false);
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
        Ok(RunningHub {
            addr,
            state,
            opened,
            store: store_spec,
            hub,
            shutdown: tx,
            serving,
            thread,
        })
    }
}
