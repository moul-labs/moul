//! Where a hub keeps replicas: a folder, or a bucket, with one prefix for
//! each database of each account (`<account>/<database>/`).
//!
//! Replica calls block (the transports do their I/O inside their futures),
//! so they run on the blocking pool.

use std::path::PathBuf;

use moul_sync::{DirReplica, Replica, S3Config, S3Replica, block_on};

/// The root of every replica a hub keeps.
#[derive(Clone)]
pub enum Store {
    /// Folders under this one.
    Dir(PathBuf),
    /// Prefixes in a bucket, under the config's own prefix.
    S3(S3Config),
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Store::Dir(p) => write!(f, "Store::Dir({})", p.display()),
            Store::S3(c) => write!(f, "Store::S3(s3://{}/{})", c.bucket, c.prefix),
        }
    }
}

impl Store {
    /// The store a spec names: a folder, or `s3://bucket/prefix?endpoint=…`
    /// with credentials from the environment.
    pub fn from_spec(spec: &str) -> Result<Store, String> {
        if spec.starts_with("s3://") {
            let env = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
            let key = env("AWS_ACCESS_KEY_ID")
                .ok_or("AWS_ACCESS_KEY_ID is not set; an s3:// store needs credentials")?;
            let secret = env("AWS_SECRET_ACCESS_KEY")
                .ok_or("AWS_SECRET_ACCESS_KEY is not set; an s3:// store needs credentials")?;
            let mut url = spec.to_owned();
            for (k, v) in [
                (
                    "endpoint",
                    env("AWS_ENDPOINT_URL_S3").or_else(|| env("AWS_ENDPOINT_URL")),
                ),
                (
                    "region",
                    env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")),
                ),
            ] {
                if let Some(v) = v
                    && !url.contains(&format!("{k}="))
                {
                    url.push(if url.contains('?') { '&' } else { '?' });
                    url.push_str(&format!("{k}={v}"));
                }
            }
            let mut c = S3Config::from_url(&url, &key, &secret).map_err(|e| e.to_string())?;
            c.session_token = env("AWS_SESSION_TOKEN");
            return Ok(Store::S3(c));
        }
        let path = spec.strip_prefix("file://").unwrap_or(spec);
        if path.contains("://") {
            return Err(format!("{spec:?} is not a folder or an s3:// URL"));
        }
        Ok(Store::Dir(PathBuf::from(path)))
    }

    /// Where the store is, for people: never credentials.
    pub fn describe(&self) -> String {
        match self {
            Store::Dir(p) => p.display().to_string(),
            Store::S3(c) => format!("s3://{}/{}", c.bucket, c.prefix.trim_matches('/')),
        }
    }

    /// The replica of `database` of `account`.
    pub fn open(&self, account: &str, database: &str) -> Result<Box<dyn Replica>, String> {
        match self {
            Store::Dir(root) => Ok(Box::new(DirReplica::new(root.join(account).join(database)))),
            Store::S3(c) => {
                let mut c = c.clone();
                let base = c.prefix.trim_matches('/').to_owned();
                c.prefix = if base.is_empty() {
                    format!("{account}/{database}")
                } else {
                    format!("{base}/{account}/{database}")
                };
                Ok(Box::new(
                    S3Replica::with_default_client(c).map_err(|e| e.to_string())?,
                ))
            }
        }
    }

    /// Runs `f` on the replica of `database`, on the blocking pool.
    pub async fn with<T: Send + 'static>(
        &self,
        account: &str,
        database: &str,
        f: impl FnOnce(&dyn Replica) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let store = self.clone();
        let (a, d) = (account.to_owned(), database.to_owned());
        tokio::task::spawn_blocking(move || {
            let r = store.open(&a, &d)?;
            f(r.as_ref())
        })
        .await
        .map_err(|e| format!("the store task failed: {e}"))?
    }

    /// Deletes everything `database` holds.
    pub async fn delete_all(&self, account: &str, database: &str) -> Result<(), String> {
        let dir = match self {
            Store::Dir(root) => Some(root.join(account).join(database)),
            Store::S3(_) => None,
        };
        self.with(account, database, |r| {
            let all = block_on(r.list("")).map_err(|e| e.to_string())?;
            for o in all {
                block_on(r.delete(&o.key)).map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .await?;
        if let Some(d) = dir {
            let _ = std::fs::remove_dir_all(d);
        }
        Ok(())
    }
}

/// Whether `key` is one a replica may hold: an LTX file, Moul's records, the
/// same under `moul/writer-only/`, or an uploaded file.
pub fn valid_key(key: &str) -> bool {
    let key = key.strip_prefix("moul/writer-only/").unwrap_or(key);
    if key == "moul/head.json" || key == "moul/writer.json" {
        return true;
    }
    if moul_sync::layout::parse_ltx_key(key).is_some() {
        return true;
    }
    key.strip_prefix("files/")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked() {
        assert!(valid_key("ltx/0/0000000000000001-0000000000000001.ltx"));
        assert!(valid_key("moul/head.json"));
        assert!(valid_key("moul/writer-only/moul/writer.json"));
        assert!(valid_key(
            "moul/writer-only/ltx/9/0000000000000001-00000000000000a4.ltx"
        ));
        assert!(valid_key(&format!("files/{}", "a".repeat(64))));
        assert!(!valid_key("ltx/0/../../etc/passwd"));
        assert!(!valid_key("moul/other.json"));
        assert!(!valid_key("files/abc"));
        assert!(!valid_key(""));
    }
}
