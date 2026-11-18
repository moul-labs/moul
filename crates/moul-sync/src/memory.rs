//! A replica in memory, with failures to order, for tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::replica::{BoxFuture, Condition, Object, ObjectInfo, Put, Replica, check_key};
use crate::{Error, Result};

/// A failure [`MemoryReplica::fail_next`] injects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The request fails before it reaches the store: nothing changes.
    Lost,
    /// The request is carried out, but its answer is lost: the caller sees a
    /// network error although the object was written or deleted.
    AnswerLost,
    /// The store is full: writes fail, reads work.
    Full,
}

#[derive(Default)]
struct Inner {
    objects: BTreeMap<String, (Vec<u8>, u64, i64)>,
    next_etag: u64,
    faults: Vec<Fault>,
    full: bool,
    requests: u64,
}

/// A replica held in memory. Clones share the same objects, so two devices
/// in one test can sync through it.
#[derive(Clone, Default)]
pub struct MemoryReplica {
    inner: Arc<Mutex<Inner>>,
    now: Arc<AtomicI64>,
}

impl std::fmt::Debug for MemoryReplica {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryReplica")
            .field("objects", &self.lock().objects.len())
            .finish_non_exhaustive()
    }
}

impl MemoryReplica {
    /// An empty replica.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets the time recorded on objects written from now on.
    pub fn set_now(&self, ms: i64) {
        self.now.store(ms, Ordering::SeqCst);
    }

    /// Makes the next requests fail, one fault each, in order.
    pub fn fail_next(&self, faults: &[Fault]) {
        self.lock().faults.extend_from_slice(faults);
    }

    /// Makes every write fail until called with `false`, as a full disk
    /// would.
    pub fn set_full(&self, full: bool) {
        self.lock().full = full;
    }

    /// How many requests the replica has answered or failed.
    pub fn requests(&self) -> u64 {
        self.lock().requests
    }

    /// Every key, in order.
    pub fn keys(&self) -> Vec<String> {
        self.lock().objects.keys().cloned().collect()
    }

    /// The object at `key`.
    pub fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().objects.get(key).map(|o| o.0.clone())
    }

    /// Writes an object directly, with no condition.
    pub fn insert(&self, key: &str, bytes: &[u8]) {
        let now = self.now.load(Ordering::SeqCst);
        let mut inner = self.lock();
        inner.next_etag += 1;
        let etag = inner.next_etag;
        inner
            .objects
            .insert(key.to_owned(), (bytes.to_vec(), etag, now));
    }

    /// Removes an object directly.
    pub fn remove(&self, key: &str) {
        self.lock().objects.remove(key);
    }

    /// Takes the next fault, counting the request. `Full` applies to writes.
    fn begin(&self, write: bool) -> Result<Option<Fault>> {
        let mut inner = self.lock();
        inner.requests += 1;
        if write && inner.full {
            return Err(Error::network(
                "the replica is full (507 Insufficient Storage)",
            ));
        }
        let fault = (!inner.faults.is_empty()).then(|| inner.faults.remove(0));
        match fault {
            Some(Fault::Lost) => Err(Error::network("connection lost before the request arrived")),
            Some(Fault::Full) if write => Err(Error::network(
                "the replica is full (507 Insufficient Storage)",
            )),
            other => Ok(other),
        }
    }
}

fn answer_lost<T>(fault: Option<Fault>, value: T) -> Result<T> {
    if fault == Some(Fault::AnswerLost) {
        Err(Error::network("connection lost before the answer arrived"))
    } else {
        Ok(value)
    }
}

impl Replica for MemoryReplica {
    fn describe(&self) -> String {
        "memory".into()
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Object>>> {
        Box::pin(async move {
            check_key(key)?;
            let fault = self.begin(false)?;
            let found = self.lock().objects.get(key).map(|(b, e, _)| Object {
                bytes: b.clone(),
                etag: format!("\"{e}\""),
            });
            answer_lost(fault, found)
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, Result<Put>> {
        Box::pin(async move {
            check_key(key)?;
            let fault = self.begin(true)?;
            let now = self.now.load(Ordering::SeqCst);
            let mut inner = self.lock();
            let current = inner.objects.get(key).map(|o| format!("\"{}\"", o.1));
            let allowed = match &condition {
                Condition::None => true,
                Condition::IfNoneMatch => current.is_none(),
                Condition::IfMatch(etag) => current.as_ref() == Some(etag),
            };
            let result = if allowed {
                inner.next_etag += 1;
                let etag = inner.next_etag;
                inner
                    .objects
                    .insert(key.to_owned(), (bytes.to_vec(), etag, now));
                Put::Written {
                    etag: format!("\"{etag}\""),
                }
            } else {
                Put::Refused
            };
            drop(inner);
            answer_lost(fault, result)
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ObjectInfo>>> {
        Box::pin(async move {
            let fault = self.begin(false)?;
            let found = self
                .lock()
                .objects
                .range(prefix.to_owned()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, (b, _, m))| ObjectInfo {
                    key: k.clone(),
                    size: b.len() as u64,
                    modified: *m,
                })
                .collect();
            answer_lost(fault, found)
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            check_key(key)?;
            let fault = self.begin(false)?;
            self.lock().objects.remove(key);
            answer_lost(fault, ())
        })
    }
}
