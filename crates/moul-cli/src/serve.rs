//! `moul serve` and `moul mcp`: a database folder as an HTTP server, or as
//! MCP tools over stdio.

use std::net::SocketAddr;
use std::path::PathBuf;

use moul_server::ServerConfig;
use moul_server::config::{parse_duration_ms, parse_trusted};

use crate::cli::{Args, Result};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start: {e}"))
}

/// Reads the server's settings from flags and the `MOUL_*` environment.
pub(crate) fn config(a: &Args, dir: PathBuf) -> Result<ServerConfig> {
    let mut c = ServerConfig {
        dir,
        ..ServerConfig::default()
    };
    if let Some(port) = env("MOUL_PORT") {
        // As the Go engine: every interface.
        let port: u16 = port
            .parse()
            .map_err(|_| format!("MOUL_PORT {port:?} is not a port"))?;
        c.listen = SocketAddr::from(([0, 0, 0, 0], port));
    }
    if let Some(l) = a.value(&["--listen", "-l"]) {
        c.listen = parse_listen(l)?;
    }
    c.public_url = a
        .value(&["--public-url"])
        .map(str::to_owned)
        .or_else(|| env("MOUL_PUBLIC_URL"))
        .unwrap_or_default();
    c.admin_key = a
        .value(&["--admin-key"])
        .map(str::to_owned)
        .or_else(|| env("MOUL_ADMIN_KEY"));
    c.signing_key = env("MOUL_JWT_SECRET").map(String::into_bytes);
    if let Some(origins) = a
        .value(&["--cors"])
        .map(str::to_owned)
        .or_else(|| env("MOUL_CORS_ORIGINS"))
    {
        c.cors_origins = origins
            .split(',')
            .map(|o| o.trim().trim_end_matches('/').to_owned())
            .filter(|o| !o.is_empty())
            .collect();
    }
    if let Ok(list) = std::env::var("MOUL_TRUSTED_PROXIES") {
        c.trusted_proxies = parse_trusted(&list)?;
    }
    c.client_ip_header = env("MOUL_CLIENT_IP_HEADER");
    for (var, slot) in [
        ("MOUL_TOKEN_TTL", &mut c.token_ttl_ms),
        ("MOUL_TOKEN_REFRESH_GRACE", &mut c.refresh_grace_ms),
        ("MOUL_FILE_URL_TTL", &mut c.file_url_ttl_ms),
    ] {
        if let Some(v) = env(var) {
            *slot = parse_duration_ms(&v)
                .ok_or_else(|| format!("{var} {v:?} is not a duration such as 24h"))?;
        }
    }
    c.replica = a.value(&["--replica"]).map(str::to_owned);
    c.follower = a.has("--read-only");
    c.allow_private_calls = a.has("--allow-private-calls");
    c.import = a.value(&["--from"]).map(PathBuf::from);
    c.app_name = env("MOUL_APP_NAME").or_else(|| env("MOUL_MCP_NAME"));
    Ok(c)
}

fn parse_listen(s: &str) -> Result<SocketAddr> {
    if let Ok(port) = s.parse::<u16>() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    let s = s
        .strip_prefix(':')
        .map(|p| format!("0.0.0.0:{p}"))
        .unwrap_or_else(|| s.to_owned());
    s.parse().map_err(|_| {
        format!("--listen {s:?}: give a port, or an address and port such as 0.0.0.0:8090")
    })
}

pub(crate) const SERVE_FLAGS: &[&str] = &[
    "--listen",
    "-l",
    "--public-url",
    "--admin-key",
    "--replica",
    "--from",
    "--cors",
];
pub(crate) const SERVE_BARE: &[&str] = &["--read-only", "--allow-private-calls"];

/// `moul serve [<folder>]`
pub(crate) fn serve(args: &[String]) -> Result<()> {
    let a = Args::parse(args, SERVE_FLAGS, SERVE_BARE)?;
    let dir = PathBuf::from(
        a.positional
            .first()
            .map(String::as_str)
            .unwrap_or("moul-data"),
    );
    a.arity(1)?;
    if dir.is_file() {
        return Err(format!(
            "{} is a file; serve a folder, and start it from this file with `moul serve <folder> --from {}`",
            dir.display(),
            dir.display()
        ));
    }
    let config = config(&a, dir)?;
    runtime()?.block_on(async move {
        let running = moul_server::start(config).await?;
        let s = &running.state;
        println!("moul {} serving {}", env!("CARGO_PKG_VERSION"), s.config.dir.display());
        println!("  api      http://{}/api", running.addr);
        println!("  docs     http://{}/docs", running.addr);
        println!("  mcp      http://{}/api/mcp", running.addr);
        if let Some(r) = &running.opened.replica {
            println!("  replica  {r}");
        }
        if s.config.follower {
            println!("  read-only: this folder follows another writer");
        }
        if running.opened.admin_key_made {
            println!(
                "\nadmin key: {}\n(made now and kept in {}/writer.db; set MOUL_ADMIN_KEY or --admin-key to choose your own)",
                running.opened.admin_key,
                s.config.dir.display()
            );
        }
        moul_server::server::signal().await;
        println!("stopping: finishing requests and pushing the last changes");
        running.stop().await;
        Ok(())
    })
}

/// `moul mcp <folder>`
pub(crate) fn mcp(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &["--replica"], &["--read-only"])?;
    let dir = PathBuf::from(a.positional(0, "the database folder")?);
    a.arity(1)?;
    if dir.is_file() {
        return Err(format!(
            "{} is a file; give a folder (`moul serve <folder> --from {}` makes one)",
            dir.display(),
            dir.display()
        ));
    }
    let mut config = ServerConfig {
        dir,
        ..ServerConfig::default()
    };
    config.follower = a.has("--read-only");
    config.replica = a.value(&["--replica"]).map(str::to_owned);
    config.effects = false;
    let name = env("MOUL_APP_NAME").or_else(|| env("MOUL_MCP_NAME"));
    runtime()?.block_on(async move {
        let (db, thread, _) = moul_server::server::open_db(&config)?;
        let name = match name {
            Some(n) => n,
            None => db
                .run(|db| {
                    db.engine
                        .settings()
                        .get("app_name")
                        .cloned()
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default(),
        };
        let served = moul_server::mcp::serve_stdio(db.clone(), name).await;
        db.close();
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        served.map_err(|e| e.to_string())
    })
}
