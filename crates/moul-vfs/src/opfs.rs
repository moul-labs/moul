//! Storage in the browser's origin private file system (OPFS).
//!
//! SQLite needs synchronous file calls, and the only synchronous OPFS API is
//! the sync access handle, available in dedicated workers. Getting a handle is
//! asynchronous and takes an exclusive lock on the file, so
//! [`OpfsStorage::open`] gets every handle the volume will need up front, and
//! the tab that holds the leader lock is the only one that calls it.

use std::cell::RefCell;
use std::collections::BTreeMap;

use js_sys::{Function, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DomException, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemReadWriteOptions, FileSystemRemoveOptions,
    FileSystemSyncAccessHandle, WorkerGlobalScope,
};

use crate::storage::{Storage, StorageFile, check_name};
use crate::volume::LOG_NAME;
use crate::{Error, Result};

/// How long to keep retrying a file another worker still holds. A tab that
/// just lost the leader lock may take a moment to close its handles.
const HANDLE_RETRY_MS: u32 = 5_000;

fn js_err(what: &str, err: JsValue) -> Error {
    let detail = if let Some(e) = err.dyn_ref::<DomException>() {
        format!("{}: {}", e.name(), e.message())
    } else if let Some(e) = err.dyn_ref::<js_sys::Error>() {
        format!("{}: {}", String::from(e.name()), String::from(e.message()))
    } else {
        format!("{err:?}")
    };
    Error::io(format!("{what}: {detail}"))
}

fn is_dom_error(err: &JsValue, names: &[&str]) -> bool {
    err.dyn_ref::<DomException>()
        .is_some_and(|e| names.contains(&e.name().as_str()))
}

/// Resolves after `ms` milliseconds, using the worker's `setTimeout`.
pub(crate) async fn sleep_ms(ms: u32) {
    let promise = Promise::new(&mut |resolve, _| {
        let set_timeout: Option<Function> = Reflect::get(&js_sys::global(), &"setTimeout".into())
            .ok()
            .and_then(|f| f.dyn_into().ok());
        match set_timeout {
            Some(f) => {
                let _ = f.call2(&JsValue::NULL, &resolve, &JsValue::from(ms));
            }
            None => {
                let _ = resolve.call0(&JsValue::NULL);
            }
        }
    });
    let _ = JsFuture::from(promise).await;
}

/// The OPFS root directory. Only available in a worker.
async fn root() -> Result<FileSystemDirectoryHandle> {
    let scope: WorkerGlobalScope = js_sys::global()
        .dyn_into()
        .map_err(|_| Error::io("OPFS storage needs a dedicated worker"))?;
    let dir = JsFuture::from(scope.navigator().storage().get_directory())
        .await
        .map_err(|e| js_err("cannot open the origin private file system", e))?;
    Ok(dir.unchecked_into())
}

/// Opens (and creates) the folder at `path`, a `/`-separated path under the root.
async fn folder(path: &str) -> Result<FileSystemDirectoryHandle> {
    let mut dir = root().await?;
    let create = FileSystemGetDirectoryOptions::new();
    create.set_create(true);
    for part in path.split('/').filter(|p| !p.is_empty()) {
        check_name(part)?;
        let next = JsFuture::from(dir.get_directory_handle_with_options(part, &create))
            .await
            .map_err(|e| js_err(&format!("cannot open folder {part:?}"), e))?;
        dir = next.unchecked_into();
    }
    Ok(dir)
}

async fn sync_handle(
    dir: &FileSystemDirectoryHandle,
    name: &str,
) -> Result<FileSystemSyncAccessHandle> {
    let create = FileSystemGetFileOptions::new();
    create.set_create(true);
    let file: FileSystemFileHandle =
        JsFuture::from(dir.get_file_handle_with_options(name, &create))
            .await
            .map_err(|e| js_err(&format!("cannot open {name}"), e))?
            .unchecked_into();
    let mut waited = 0;
    let mut delay = 10;
    loop {
        match JsFuture::from(file.create_sync_access_handle()).await {
            Ok(h) => return Ok(h.unchecked_into()),
            Err(e)
                if waited < HANDLE_RETRY_MS
                    && is_dom_error(&e, &["NoModificationAllowedError", "InvalidStateError"]) =>
            {
                sleep_ms(delay).await;
                waited += delay;
                delay = (delay * 2).min(500);
            }
            Err(e) => return Err(js_err(&format!("cannot lock {name}"), e)),
        }
    }
}

/// A folder in the origin private file system, with a sync access handle
/// open on each file the volume will use.
pub struct OpfsStorage {
    path: String,
    handles: RefCell<BTreeMap<String, FileSystemSyncAccessHandle>>,
}

impl std::fmt::Debug for OpfsStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpfsStorage")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl OpfsStorage {
    /// Opens the folder at `path` (for example `moul/notes`), creating it, and
    /// takes a sync access handle on each of `files` and on the commit log.
    ///
    /// Fails if another worker keeps one of them open for more than a few
    /// seconds; tabs elect a leader first so that this does not happen.
    pub async fn open(path: &str, files: &[&str]) -> Result<Self> {
        let dir = folder(path).await?;
        let mut handles = BTreeMap::new();
        for name in files.iter().copied().chain([LOG_NAME]) {
            check_name(name)?;
            if handles.contains_key(name) {
                continue;
            }
            match sync_handle(&dir, name).await {
                Ok(h) => {
                    handles.insert(name.to_owned(), h);
                }
                Err(e) => {
                    for h in handles.values() {
                        h.close();
                    }
                    return Err(e);
                }
            }
        }
        Ok(Self {
            path: path.to_owned(),
            handles: RefCell::new(handles),
        })
    }

    /// Deletes the folder at `path` and everything in it. Its files must not
    /// be open, in this worker or another.
    pub async fn remove(path: &str) -> Result<()> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let Some((last, parents)) = parts.split_last() else {
            return Err(Error::invalid("cannot remove the OPFS root"));
        };
        let parent = folder(&parents.join("/")).await?;
        let opts = FileSystemRemoveOptions::new();
        opts.set_recursive(true);
        match JsFuture::from(parent.remove_entry_with_options(last, &opts)).await {
            Ok(_) => Ok(()),
            Err(e) if is_dom_error(&e, &["NotFoundError"]) => Ok(()),
            Err(e) => Err(js_err(&format!("cannot remove {path}"), e)),
        }
    }
}

impl Drop for OpfsStorage {
    fn drop(&mut self) {
        for h in self.handles.borrow().values() {
            h.close();
        }
    }
}

impl Storage for OpfsStorage {
    fn open(&self, name: &str) -> Result<Box<dyn StorageFile>> {
        let handle = self.handles.borrow_mut().remove(name).ok_or_else(|| {
            Error::invalid(format!(
                "{name} was not opened in advance in OPFS folder {}",
                self.path
            ))
        })?;
        Ok(Box::new(OpfsFile {
            handle,
            name: name.to_owned(),
        }))
    }

    fn describe(&self) -> String {
        format!("opfs:{}", self.path)
    }
}

struct OpfsFile {
    handle: FileSystemSyncAccessHandle,
    name: String,
}

impl OpfsFile {
    fn err(&self, what: &str, e: JsValue) -> Error {
        js_err(&format!("cannot {what} {}", self.name), e)
    }
}

impl Drop for OpfsFile {
    fn drop(&mut self) {
        self.handle.close();
    }
}

impl StorageFile for OpfsFile {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let at = FileSystemReadWriteOptions::new();
        let mut done = 0;
        while done < buf.len() {
            at.set_at((offset + done as u64) as f64);
            let n = self
                .handle
                .read_with_u8_array_and_options(&mut buf[done..], &at)
                .map_err(|e| self.err("read", e))? as usize;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        let at = FileSystemReadWriteOptions::new();
        let mut done = 0;
        while done < buf.len() {
            at.set_at((offset + done as u64) as f64);
            let n = self
                .handle
                .write_with_u8_array_and_options(&buf[done..], &at)
                .map_err(|e| self.err("write", e))? as usize;
            if n == 0 {
                return Err(Error::io(format!(
                    "cannot write {}: storage accepted no bytes (quota?)",
                    self.name
                )));
            }
            done += n;
        }
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.handle
            .truncate_with_f64(size as f64)
            .map_err(|e| self.err("truncate", e))
    }

    fn size(&mut self) -> Result<u64> {
        self.handle
            .get_size()
            .map(|s| s as u64)
            .map_err(|e| self.err("stat", e))
    }

    fn flush(&mut self) -> Result<()> {
        self.handle.flush().map_err(|e| self.err("flush", e))
    }
}
