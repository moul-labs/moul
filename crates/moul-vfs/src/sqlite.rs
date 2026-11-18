//! Registers a [`Volume`](crate::Volume) with SQLite as a VFS.
//!
//! Database files (`SQLITE_OPEN_MAIN_DB`) go to the volume. Journals and
//! temporary files live in memory: the volume's own log makes commits atomic,
//! so a rollback journal on storage would only cost writes. WAL is refused.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsqlite_vfs::{
    AccessMode, FileKind, LockLevel, OpenOptions, OpenRequest, OpenedFile, OsCallback,
    SQLiteIoMethods, SQLiteVfs, SyncOptions, VfsError, VfsErrorCode, VfsFile, VfsRegistration,
    VfsResult, VfsStore, register_vfs,
};

use crate::volume::{Lock, Shared};
use crate::{Error, ErrorKind};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct AppData {
    shared: Arc<Shared>,
    os: Os,
    last_error: Mutex<Option<VfsError>>,
}

/// Owns the SQLite registration of one volume.
pub(crate) struct Registration {
    name: String,
    inner: Option<VfsRegistration<AppData>>,
}

// SAFETY: the registration only holds pointers to heap data that SQLite reaches
// through its own (thread-safe, on native builds) VFS list. The app data is a
// mutex-protected `Shared` plus `Os`, both safe to use from any thread on native
// targets.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for Registration {}
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Sync for Registration {}

impl Registration {
    pub(crate) fn register(shared: Arc<Shared>) -> crate::Result<Self> {
        let name = format!("moul-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let data = AppData {
            shared,
            os: Os::new(),
            last_error: Mutex::new(None),
        };
        // SAFETY: `MoulVfs` uses the default constructor with `Handle` as the
        // file type throughout, and the app data lives until `release`.
        let inner = unsafe { register_vfs::<MoulIo, MoulVfs>(&name, data, false) }
            .map_err(|e| Error::io(format!("cannot register VFS {name}: {e:?}")))?;
        Ok(Self {
            name,
            inner: Some(inner),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Unregisters the VFS if nothing uses it any more. Otherwise it is left
    /// registered, and leaked, because SQLite still holds pointers into it.
    pub(crate) fn release(&mut self, unused: bool) {
        if let Some(inner) = self.inner.take() {
            if unused {
                // SAFETY: no file of this VFS is open, so SQLite holds no
                // pointers into it, and the volume being dropped is the only
                // other owner.
                if let Err((inner, _)) = unsafe { inner.unregister() } {
                    let _ = inner.into_raw();
                }
            } else {
                let _ = inner.into_raw();
            }
        }
    }
}

fn vfs_err(err: Error, code: VfsErrorCode) -> VfsError {
    let code = match err.kind() {
        ErrorKind::Busy => VfsErrorCode::Busy,
        ErrorKind::Corrupt => VfsErrorCode::Corrupt,
        _ => code,
    };
    VfsError::new(code, err.to_string().into())
}

/// A file SQLite opened through the volume.
pub(crate) enum Handle {
    /// A database file, served by the volume.
    Page { shared: Arc<Shared>, name: String },
    /// A journal or temporary file, kept in memory.
    Mem {
        shared: Arc<Shared>,
        data: Arc<Mutex<Vec<u8>>>,
    },
}

impl Handle {
    fn page<T>(
        &self,
        code: VfsErrorCode,
        f: impl FnOnce(&mut crate::volume::State, &str) -> crate::Result<T>,
    ) -> VfsResult<T> {
        match self {
            Handle::Page { shared, name } => {
                f(&mut shared.state(), name).map_err(|e| vfs_err(e, code))
            }
            Handle::Mem { .. } => unreachable!("only called on database files"),
        }
    }

    fn mem(&self) -> Option<std::sync::MutexGuard<'_, Vec<u8>>> {
        match self {
            Handle::Mem { data, .. } => Some(data.lock().unwrap_or_else(|e| e.into_inner())),
            Handle::Page { .. } => None,
        }
    }
}

fn to_lock(level: LockLevel) -> Lock {
    match level {
        LockLevel::None => Lock::None,
        LockLevel::Shared => Lock::Shared,
        LockLevel::Reserved => Lock::Reserved,
        LockLevel::Pending => Lock::Pending,
        LockLevel::Exclusive => Lock::Exclusive,
    }
}

impl VfsFile for Handle {
    fn read(&mut self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if let Some(data) = self.mem() {
            if offset >= data.len() as u64 {
                return Ok(0);
            }
            let start = offset as usize;
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            return Ok(n);
        }
        self.page(VfsErrorCode::IoRead, |s, name| s.read(name, buf, offset))
    }

    fn write(&mut self, buf: &[u8], offset: u64) -> VfsResult<()> {
        if let Some(mut data) = self.mem() {
            let start = offset as usize;
            let end = start + buf.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[start..end].copy_from_slice(buf);
            return Ok(());
        }
        self.page(VfsErrorCode::IoWrite, |s, name| s.write(name, buf, offset))
    }

    fn truncate(&mut self, size: u64) -> VfsResult<()> {
        if let Some(mut data) = self.mem() {
            data.resize(size as usize, 0);
            return Ok(());
        }
        self.page(VfsErrorCode::IoTruncate, |s, name| s.truncate(name, size))
    }

    fn sync(&mut self, _options: SyncOptions) -> VfsResult<()> {
        // Durability comes from the commit log, written when SQLite releases
        // its write lock, not from SQLite's own sync calls.
        Ok(())
    }

    fn size(&self) -> VfsResult<u64> {
        if let Some(data) = self.mem() {
            return Ok(data.len() as u64);
        }
        self.page(VfsErrorCode::IoStat, |s, name| s.size(name))
    }

    fn lock(&mut self, level: LockLevel) -> VfsResult<()> {
        if self.mem().is_some() {
            return Ok(());
        }
        self.page(VfsErrorCode::IoLock, |s, name| s.lock(name, to_lock(level)))
    }

    fn unlock(&mut self, level: LockLevel) -> VfsResult<()> {
        if self.mem().is_some() {
            return Ok(());
        }
        self.page(VfsErrorCode::IoUnlock, |s, name| {
            s.unlock(name, to_lock(level))
        })
    }

    fn check_reserved_lock(&self) -> VfsResult<bool> {
        // One connection per volume: no other connection can hold a lock.
        Ok(false)
    }
}

pub(crate) struct MoulStore;

impl VfsStore for MoulStore {
    type File = Handle;
    type AppData = AppData;

    fn record_error(data: &AppData, error: VfsError) {
        *data.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
    }

    fn last_error(data: &AppData) -> Option<VfsError> {
        data.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn open_file(data: &AppData, request: OpenRequest<'_>) -> VfsResult<OpenedFile<Handle>> {
        let options = request.options;
        let access = options.access();
        let shared = data.shared.clone();
        let Some(filename) = request.filename else {
            data.shared.state().open_handles += 1;
            return Ok(OpenedFile {
                file: Handle::Mem {
                    shared,
                    data: Arc::default(),
                },
                access,
            });
        };
        let name = filename.path();
        let mut state = data.shared.state();
        let file = match options.kind() {
            Some(FileKind::MainDb) => {
                state
                    .open_page(name)
                    .map_err(|e| vfs_err(e, VfsErrorCode::CantOpen))?;
                Handle::Page {
                    shared,
                    name: name.to_owned(),
                }
            }
            Some(FileKind::Wal) => {
                return Err(VfsError::new(
                    VfsErrorCode::CantOpen,
                    "WAL is not supported by the Moul VFS".into(),
                ));
            }
            _ => {
                let data = match state.temps.get(name) {
                    Some(_) if options.exclusive() => {
                        return Err(VfsError::new(
                            VfsErrorCode::CantOpen,
                            format!("{name} already exists").into(),
                        ));
                    }
                    Some(data) => data.clone(),
                    None if options.create() => {
                        let data = Arc::<Mutex<Vec<u8>>>::default();
                        state.temps.insert(name.to_owned(), data.clone());
                        data
                    }
                    None => {
                        return Err(VfsError::new(
                            VfsErrorCode::CantOpen,
                            format!("{name} not found").into(),
                        ));
                    }
                };
                Handle::Mem { shared, data }
            }
        };
        state.open_handles += 1;
        Ok(OpenedFile { file, access })
    }

    fn close_file(
        _data: &AppData,
        name: Option<&str>,
        file: Handle,
        options: OpenOptions,
    ) -> VfsResult<()> {
        match file {
            Handle::Page { shared, name } => {
                let mut state = shared.state();
                state.open_handles -= 1;
                state
                    .close_page(&name)
                    .map_err(|e| vfs_err(e, VfsErrorCode::IoClose))
            }
            Handle::Mem { shared, data } => {
                let mut state = shared.state();
                state.open_handles -= 1;
                if let (Some(name), true) = (name, options.delete_on_close())
                    && state.temps.get(name).is_some_and(|d| Arc::ptr_eq(d, &data))
                {
                    state.temps.remove(name);
                }
                Ok(())
            }
        }
    }

    fn access(data: &AppData, name: &str, _mode: AccessMode) -> VfsResult<bool> {
        Ok(data.shared.state().exists(name))
    }

    fn full_pathname(_data: &AppData, name: &str) -> VfsResult<String> {
        Ok(name.to_owned())
    }

    fn delete_file(data: &AppData, name: &str, _sync_dir: bool) -> VfsResult<()> {
        // Only journals and temporary files are deleted; they live in memory.
        data.shared.state().temps.remove(name);
        Ok(())
    }
}

pub(crate) struct MoulIo;

impl SQLiteIoMethods for MoulIo {
    type Store = MoulStore;
}

pub(crate) struct MoulVfs;

impl SQLiteVfs<MoulIo> for MoulVfs {
    type Os = Os;

    fn os(data: &AppData) -> &Os {
        &data.os
    }
}

/// Clock, randomness and sleep for SQLite.
pub(crate) struct Os {
    seed: Mutex<u64>,
}

impl Os {
    fn new() -> Self {
        let now = now_ms().unwrap_or(0) as u64;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            seed: Mutex::new(now ^ id.rotate_left(32) ^ 0x2545_F491_4F6C_DD1D),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> Option<i64> {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    i64::try_from(d.as_millis()).ok()
}

#[cfg(target_arch = "wasm32")]
fn now_ms() -> Option<i64> {
    Some(js_sys::Date::now() as i64)
}

impl OsCallback for Os {
    fn sleep(&self, dur: Duration) {
        // SQLite sleeps only in busy handlers. With one connection per volume
        // nothing is ever busy, and a worker cannot block without atomics.
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::sleep(dur);
        #[cfg(target_arch = "wasm32")]
        let _ = dur;
    }

    fn random(&self, buf: &mut [u8]) -> usize {
        // SQLite uses this to seed its own generator, for temporary names and
        // rowids. It is not used for anything secret.
        let mut seed = self.seed.lock().unwrap_or_else(|e| e.into_inner());
        for chunk in buf.chunks_mut(8) {
            *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
        buf.len()
    }

    fn epoch_timestamp_in_ms(&self) -> VfsResult<i64> {
        now_ms().ok_or_else(|| VfsError::new(VfsErrorCode::Error, "clock unavailable".into()))
    }
}
