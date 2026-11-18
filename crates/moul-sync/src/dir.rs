//! A replica in a folder on disk, laid out as Litestream's file replica is.
//!
//! Objects are files under the folder, so `litestream restore` can read a
//! replica written here directly. A write goes to a temporary file first and
//! is then linked or renamed into place, so a reader never sees half a file.
//! `If-None-Match` is atomic (a hard link fails if the name exists);
//! `If-Match` compares and then renames, which is atomic only against other
//! writers in this process. Use a bucket for devices that write at once.

use std::fs;
use std::io::{ErrorKind as IoKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::replica::{BoxFuture, Condition, Object, ObjectInfo, Put, Replica, check_key};
use crate::{Error, Result};

static TMP: AtomicU64 = AtomicU64::new(0);

/// A replica in a folder on disk.
#[derive(Debug)]
pub struct DirReplica {
    root: PathBuf,
    cas: Mutex<()>,
}

fn io(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::network(format!("{what} {}: {e}", path.display()))
}

fn etag(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    let hex: String = d[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

impl DirReplica {
    /// A replica in the folder `root`, created on first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cas: Mutex::new(()),
        }
    }

    /// The folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        Ok(key
            .split('/')
            .fold(self.root.clone(), |p, part| p.join(part)))
    }

    fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        match fs::read(path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == IoKind::NotFound => Ok(None),
            Err(e) => Err(io("cannot read", path, e)),
        }
    }

    /// Writes `bytes` to a temporary file beside `path` and flushes it.
    fn stage(&self, path: &Path, bytes: &[u8]) -> Result<PathBuf> {
        let dir = path.parent().expect("keys have a parent");
        fs::create_dir_all(dir).map_err(|e| io("cannot create", dir, e))?;
        let tmp = dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = fs::File::create(&tmp).map_err(|e| io("cannot create", &tmp, e))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| io("cannot write", &tmp, e))?;
        Ok(tmp)
    }

    fn put_now(&self, key: &str, bytes: &[u8], condition: Condition) -> Result<Put> {
        let path = self.path(key)?;
        let tmp = self.stage(&path, bytes)?;
        let result = (|| match &condition {
            Condition::IfNoneMatch => match fs::hard_link(&tmp, &path) {
                Ok(()) => Ok(Put::Written { etag: etag(bytes) }),
                Err(e) if e.kind() == IoKind::AlreadyExists => Ok(Put::Refused),
                Err(e) => Err(io("cannot write", &path, e)),
            },
            Condition::IfMatch(want) => {
                let _guard = self.cas.lock().unwrap_or_else(|e| e.into_inner());
                match self.read(&path)? {
                    Some(current) if &etag(&current) == want => {
                        fs::rename(&tmp, &path).map_err(|e| io("cannot write", &path, e))?;
                        Ok(Put::Written { etag: etag(bytes) })
                    }
                    _ => Ok(Put::Refused),
                }
            }
            Condition::None => {
                fs::rename(&tmp, &path).map_err(|e| io("cannot write", &path, e))?;
                Ok(Put::Written { etag: etag(bytes) })
            }
        })();
        let _ = fs::remove_file(&tmp);
        result
    }

    fn walk(&self, dir: &Path, key: &str, out: &mut Vec<ObjectInfo>) -> Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == IoKind::NotFound => return Ok(()),
            Err(e) => return Err(io("cannot list", dir, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| io("cannot list", dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-") {
                continue;
            }
            let child = if key.is_empty() {
                name.clone()
            } else {
                format!("{key}/{name}")
            };
            let meta = entry.metadata().map_err(|e| io("cannot list", dir, e))?;
            if meta.is_dir() {
                self.walk(&entry.path(), &child, out)?;
            } else {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64);
                out.push(ObjectInfo {
                    key: child,
                    size: meta.len(),
                    modified,
                });
            }
        }
        Ok(())
    }
}

impl Replica for DirReplica {
    fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Object>>> {
        Box::pin(async move {
            let path = self.path(key)?;
            Ok(self.read(&path)?.map(|bytes| Object {
                etag: etag(&bytes),
                bytes,
            }))
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, Result<Put>> {
        Box::pin(async move { self.put_now(key, bytes, condition) })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ObjectInfo>>> {
        Box::pin(async move {
            let mut out = Vec::new();
            self.walk(&self.root, "", &mut out)?;
            out.retain(|o| o.key.starts_with(prefix));
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let path = self.path(key)?;
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == IoKind::NotFound => Ok(()),
                Err(e) => Err(io("cannot delete", &path, e)),
            }
        })
    }
}
