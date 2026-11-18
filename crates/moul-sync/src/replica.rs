//! The replica interface: a flat store of named objects with conditional
//! writes.
//!
//! Every transport (an S3-compatible bucket, a folder on disk, memory) offers
//! the same five calls. Keys are relative to the replica's root and use `/`,
//! such as `ltx/0/0000000000000001-0000000000000001.ltx`. The one thing a
//! transport must get right is the condition on [`Replica::put`]: it must be
//! checked and applied atomically, because it is what keeps two devices from
//! writing the same TXID.

use std::future::Future;
use std::pin::Pin;

use crate::Result;

/// A boxed future. Sync runs on one thread, in a browser worker or under a
/// small executor in the CLI, so futures need not be `Send`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// When a [`Replica::put`] may write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// Always.
    None,
    /// Only if no object has the key (`If-None-Match: *`).
    IfNoneMatch,
    /// Only if the object exists and its entity tag is this one (`If-Match`).
    IfMatch(String),
}

/// What a [`Replica::put`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Put {
    /// The object was written; this is its new entity tag.
    Written {
        /// The new entity tag.
        etag: String,
    },
    /// The condition did not hold (status 412), and nothing was written.
    Refused,
}

/// An object read from a replica.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// Its content.
    pub bytes: Vec<u8>,
    /// Its entity tag, for a later [`Condition::IfMatch`].
    pub etag: String,
}

/// An object as a listing describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Its key, relative to the replica's root.
    pub key: String,
    /// Its size in bytes.
    pub size: u64,
    /// When it was last written, in milliseconds since the Unix epoch.
    pub modified: i64,
}

/// Where a replica lives.
pub trait Replica {
    /// A short description for messages, such as `s3://bucket/prefix`. Never
    /// includes credentials.
    fn describe(&self) -> String;

    /// Reads the object at `key`, or `None` if there is none.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Object>>>;

    /// Writes `bytes` at `key` if `condition` holds, atomically.
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, Result<Put>>;

    /// Lists every object whose key starts with `prefix`, in key order.
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ObjectInfo>>>;

    /// Deletes the object at `key`. Deleting a missing object succeeds.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>>;
}

impl<R: Replica + ?Sized> Replica for Box<R> {
    fn describe(&self) -> String {
        (**self).describe()
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Object>>> {
        (**self).get(key)
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, Result<Put>> {
        (**self).put(key, bytes, condition)
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ObjectInfo>>> {
        (**self).list(prefix)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        (**self).delete(key)
    }
}

/// Checks that `key` is a relative key a transport can store.
pub(crate) fn check_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key.contains('\0')
        && key
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..");
    if ok {
        Ok(())
    } else {
        Err(crate::Error::invalid(format!(
            "{key:?} is not a valid replica key"
        )))
    }
}

/// Runs `future` to completion on this thread. The native transports do
/// blocking I/O inside their futures, so this is all the CLI and the tests
/// need; it is not a general-purpose executor.
#[cfg(not(target_arch = "wasm32"))]
pub fn block_on<F: Future>(future: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread::{self, Thread};

    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => thread::park(),
        }
    }
}
