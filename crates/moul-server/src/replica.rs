//! Naming a replica: a folder, an S3-compatible bucket, or a database on a
//! Moul hub.
//!
//! ```text
//! ./backup  or  file:///var/backups/notes          a folder
//! s3://<bucket>/<prefix>?endpoint=<url>&region=<r> a bucket
//! moul+https://sync.moul.dev/<database>            a hub database
//! ```
//!
//! A bucket's credentials come from `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN` (the endpoint and region
//! also from `AWS_ENDPOINT_URL` and `AWS_REGION`); a hub's token from
//! `MOUL_HUB_TOKEN` or what `moul login` saved.

use std::collections::BTreeMap;

use moul_sync::{DirReplica, HubConfig, HubReplica, Replica, S3Config, S3Replica};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Credentials for a bucket, when they are not in the environment.
#[derive(Clone, Debug, Default)]
pub struct Keys {
    /// The access key id.
    pub access_key_id: String,
    /// The secret.
    pub secret_access_key: String,
}

/// Where `moul login` keeps hub tokens: `$XDG_CONFIG_HOME/moul/hubs.json`,
/// `~/.config/moul/hubs.json`, or `%APPDATA%\moul\hubs.json`.
pub fn hubs_file() -> Option<std::path::PathBuf> {
    let base = env("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env("APPDATA")
                .filter(|_| cfg!(windows))
                .map(std::path::PathBuf::from)
        })
        .or_else(|| env("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("moul").join("hubs.json"))
}

/// The hub's address in a `moul+https://host[/base]/<database>` spec:
/// `https://host[/base]`.
pub fn hub_base(spec: &str) -> Option<String> {
    let rest = spec.strip_prefix("moul+")?;
    let (base, _db) = rest.trim_end_matches('/').rsplit_once('/')?;
    base.contains("://").then(|| base.to_owned())
}

/// The token `moul login` saved for the hub at `base`.
pub fn saved_token(base: &str) -> Option<String> {
    let text = std::fs::read_to_string(hubs_file()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v[base.trim_end_matches('/')]["token"]
        .as_str()
        .map(str::to_owned)
}

/// Opens the replica `spec` names. `keys` overrides the environment's
/// bucket credentials; `hub_token` gives a hub's token.
pub fn open(
    spec: &str,
    keys: Option<&Keys>,
    hub_token: Option<&str>,
) -> Result<Box<dyn Replica>, String> {
    if spec.starts_with("moul+http://") || spec.starts_with("moul+https://") {
        let token = hub_token
            .map(str::to_owned)
            .or_else(|| env("MOUL_HUB_TOKEN"))
            .or_else(|| hub_base(spec).and_then(|b| saved_token(&b)))
            .ok_or("a hub replica needs a token: run `moul login <hub>` or set MOUL_HUB_TOKEN")?;
        let config = HubConfig::from_url(spec, &token).map_err(|e| e.to_string())?;
        return Ok(Box::new(
            HubReplica::with_default_client(config).map_err(|e| e.to_string())?,
        ));
    }
    if spec.starts_with("s3://") {
        let (key, secret) = match keys {
            Some(k) => (k.access_key_id.clone(), k.secret_access_key.clone()),
            None => (
                env("AWS_ACCESS_KEY_ID")
                    .ok_or("AWS_ACCESS_KEY_ID is not set; an s3:// replica needs credentials")?,
                env("AWS_SECRET_ACCESS_KEY").ok_or(
                    "AWS_SECRET_ACCESS_KEY is not set; an s3:// replica needs credentials",
                )?,
            ),
        };
        let mut url = spec.to_owned();
        let mut add = |k: &str, v: Option<String>| {
            if let Some(v) = v
                && !url.contains(&format!("{k}="))
            {
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&format!("{k}={v}"));
            }
        };
        add(
            "endpoint",
            env("AWS_ENDPOINT_URL_S3").or_else(|| env("AWS_ENDPOINT_URL")),
        );
        add(
            "region",
            env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")),
        );
        let mut config = S3Config::from_url(&url, &key, &secret).map_err(|e| e.to_string())?;
        if keys.is_none() {
            config.session_token = env("AWS_SESSION_TOKEN");
        }
        return Ok(Box::new(
            S3Replica::with_default_client(config).map_err(|e| e.to_string())?,
        ));
    }
    let path = spec.strip_prefix("file://").unwrap_or(spec);
    if path.contains("://") {
        return Err(format!(
            "{spec:?} is not a folder, an s3:// URL or a moul+https:// hub address"
        ));
    }
    Ok(Box::new(DirReplica::new(path)))
}

/// The replica the Go engine's `litestream_*` settings name, if they turn
/// replication on: the spec and its keys.
pub fn from_settings(s: &BTreeMap<String, String>) -> Option<(String, Keys)> {
    let get = |k: &str, var: &str| {
        s.get(k)
            .cloned()
            .filter(|v| !v.is_empty())
            .or_else(|| env(var))
            .unwrap_or_default()
    };
    let enabled = s.get("litestream_enabled").is_some_and(|v| v == "true");
    let bucket = get("litestream_s3_bucket", "LITESTREAM_S3_BUCKET");
    if !enabled || bucket.is_empty() {
        return None;
    }
    let path = get("litestream_replica_path", "LITESTREAM_REPLICA_PATH");
    let path = if path.is_empty() {
        "main.db".to_owned()
    } else {
        path
    };
    let mut spec = format!("s3://{bucket}/{}", path.trim_matches('/'));
    let mut q = Vec::new();
    let endpoint = get("litestream_s3_endpoint", "LITESTREAM_S3_ENDPOINT");
    if !endpoint.is_empty() {
        q.push(format!("endpoint={endpoint}"));
    }
    let region = get("litestream_s3_region", "LITESTREAM_REGION");
    if !region.is_empty() {
        q.push(format!("region={region}"));
    }
    if !q.is_empty() {
        spec.push('?');
        spec.push_str(&q.join("&"));
    }
    Some((
        spec,
        Keys {
            access_key_id: get("litestream_access_key_id", "LITESTREAM_ACCESS_KEY_ID"),
            secret_access_key: get(
                "litestream_secret_access_key",
                "LITESTREAM_SECRET_ACCESS_KEY",
            ),
        },
    ))
}
