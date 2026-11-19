//! `moul hub`, `moul login` and `moul logout`.

use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use moul_hub::HubConfig;
use moul_hub::api::Signup;
use serde_json::{Value, json};

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

/// `moul hub [<folder>]`
pub(crate) fn hub(args: &[String]) -> Result<()> {
    let a = Args::parse(
        args,
        &[
            "--listen",
            "-l",
            "--public-url",
            "--store",
            "--signup",
            "--max-databases",
            "--max-bytes",
            "--admin-key",
        ],
        &[],
    )?;
    let dir = PathBuf::from(
        a.positional
            .first()
            .map(String::as_str)
            .unwrap_or("hub-data"),
    );
    a.arity(1)?;
    let mut c = HubConfig {
        dir,
        ..HubConfig::default()
    };
    if let Some(l) = a.value(&["--listen", "-l"]) {
        c.listen = if let Ok(port) = l.parse::<u16>() {
            SocketAddr::from(([127, 0, 0, 1], port))
        } else {
            l.parse()
                .map_err(|_| format!("--listen {l:?}: give a port, or an address and port"))?
        };
    }
    c.public_url = a
        .value(&["--public-url"])
        .map(str::to_owned)
        .or_else(|| env("MOUL_PUBLIC_URL"))
        .unwrap_or_default();
    c.store = a
        .value(&["--store"])
        .map(moul_hub::store::Store::from_spec)
        .transpose()?;
    c.admin_key = a
        .value(&["--admin-key"])
        .map(str::to_owned)
        .or_else(|| env("MOUL_ADMIN_KEY"));
    if let Ok(list) = std::env::var("MOUL_TRUSTED_PROXIES") {
        c.trusted_proxies = moul_server::config::parse_trusted(&list)?;
    }
    c.signup = match a.value(&["--signup"]).unwrap_or("closed") {
        "open" => Signup::Open,
        "invite" => Signup::Invite,
        "closed" => Signup::Closed,
        other => return Err(format!("--signup {other:?}: open, invite or closed")),
    };
    if let Some(n) = a.value(&["--max-databases"]) {
        c.max_databases = n
            .parse()
            .map_err(|_| format!("--max-databases {n:?} is not a number"))?;
    }
    if let Some(n) = a.value(&["--max-bytes"]) {
        c.max_bytes = n
            .parse()
            .map_err(|_| format!("--max-bytes {n:?} is not a number"))?;
    }
    runtime()?.block_on(async move {
        let h = moul_hub::start(c).await?;
        println!("moul hub {} at http://{}", env!("CARGO_PKG_VERSION"), h.addr);
        println!("  replicas  {}", h.store);
        println!("  sign-up   {}", a.value(&["--signup"]).unwrap_or("closed"));
        if h.opened.admin_key_made {
            println!(
                "\nadmin key: {}\n(made now and kept in the hub's directory; set MOUL_ADMIN_KEY or --admin-key to choose your own)",
                h.opened.admin_key
            );
        }
        moul_server::server::signal().await;
        h.stop().await;
        Ok(())
    })
}

fn prompt(question: &str) -> Result<String> {
    print!("{question}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read the answer: {e}"))?;
    Ok(line.trim().to_owned())
}

fn base_of(hub: &str) -> Result<String> {
    let hub = hub.trim().trim_end_matches('/');
    let hub = hub.strip_prefix("moul+").unwrap_or(hub);
    if !(hub.starts_with("https://") || hub.starts_with("http://")) {
        return Err(format!(
            "{hub:?}: give the hub's address, such as https://sync.moul.dev"
        ));
    }
    Ok(hub.to_owned())
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    body: Value,
) -> Result<Value> {
    let mut r = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    let resp = r
        .send()
        .await
        .map_err(|e| format!("cannot reach the hub: {e}"))?;
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(v["message"]
            .as_str()
            .map_or_else(|| format!("the hub answered {status}"), str::to_owned));
    }
    Ok(v)
}

fn save(base: &str, entry: Option<Value>) -> Result<PathBuf> {
    let path =
        moul_server::replica::hubs_file().ok_or("cannot tell where to keep the token: set HOME")?;
    let mut all: serde_json::Map<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    match entry {
        Some(e) => {
            all.insert(base.to_owned(), e);
        }
        None => {
            all.remove(base);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(all)).unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// `moul login <hub>`
pub(crate) fn login(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &["--email", "--label"], &[])?;
    let base = base_of(a.positional(0, "the hub's address")?)?;
    a.arity(1)?;
    let email = match a.value(&["--email"]) {
        Some(e) => e.to_owned(),
        None => prompt("Email: ")?,
    };
    let label = a.value(&["--label"]).map(str::to_owned).unwrap_or_else(|| {
        let host = env("HOSTNAME")
            .or_else(|| env("COMPUTERNAME"))
            .unwrap_or_else(|| "this computer".into());
        format!("moul CLI on {host}")
    });
    runtime()?.block_on(async move {
        let client = reqwest::Client::new();
        post(
            &client,
            &format!("{base}/api/hub/signin"),
            None,
            json!({"email": email}),
        )
        .await?;
        println!("If {email} may sign in to {base}, a code is on its way.");
        let code = prompt("Code: ")?;
        let v = post(
            &client,
            &format!("{base}/api/hub/verify"),
            None,
            json!({"email": email, "code": code, "label": label}),
        )
        .await?;
        let token = v["token"].as_str().ok_or("the hub sent no token")?;
        let path = save(
            &base,
            Some(json!({"token": token, "email": email, "device": v["device"]["id"]})),
        )?;
        println!(
            "Signed in to {base} as {email}; the token is in {}",
            path.display()
        );
        println!("Pull a database with: moul pull moul+{base}/<database> ./<folder>");
        Ok(())
    })
}

/// `moul logout <hub>`
pub(crate) fn logout(args: &[String]) -> Result<()> {
    let a = Args::parse(args, &[], &[])?;
    let base = base_of(a.positional(0, "the hub's address")?)?;
    a.arity(1)?;
    let saved: Option<Value> = moul_server::replica::hubs_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| v[&base].clone())
        .filter(|v| !v.is_null());
    let Some(saved) = saved else {
        return Err(format!("not signed in to {base}"));
    };
    runtime()?.block_on(async {
        if let (Some(token), Some(device)) = (saved["token"].as_str(), saved["device"].as_str()) {
            let r = reqwest::Client::new()
                .delete(format!("{base}/api/hub/devices/{device}"))
                .bearer_auth(token)
                .send()
                .await;
            if let Err(e) = r {
                eprintln!(
                    "moul: could not reach {base} to revoke the token ({e}); forgetting it anyway"
                );
            }
        }
    });
    save(&base, None)?;
    println!("Signed out of {base}");
    Ok(())
}
