//! The interface a storage back end implements.
//!
//! A back end is a flat folder of named files with positioned reads and writes.
//! It knows nothing about SQLite or pages: the [`Volume`](crate::Volume) above
//! it decides what to write and when to flush. Calls are synchronous, because
//! SQLite calls the file layer synchronously; the browser back end uses OPFS
//! sync access handles, which exist for exactly this.

use crate::Result;

/// `Send` on native targets and nothing in the browser, where JavaScript
/// objects cannot cross threads and there is only one thread anyway.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// `Send` on native targets and nothing in the browser, where JavaScript
/// objects cannot cross threads and there is only one thread anyway.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// A folder of named files.
pub trait Storage: MaybeSend {
    /// Opens `name` for reading and writing, creating it empty if it does not
    /// exist. Names are plain file names: never empty and without `/` or `\`.
    ///
    /// The volume opens each file once and keeps it open, so a back end may
    /// hand out a handle it opened in advance.
    fn open(&self, name: &str) -> Result<Box<dyn StorageFile>>;

    /// A short description for error messages and logs, such as the folder path.
    fn describe(&self) -> String;
}

/// One open file.
///
/// Writes become durable only after [`flush`](Self::flush) returns. Until then a
/// crash may lose any of them, in whole or in part; the volume is written to
/// survive exactly that.
pub trait StorageFile: MaybeSend {
    /// Reads up to `buf.len()` bytes at `offset` and returns how many were read.
    /// Fewer than asked means the file ended.
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize>;

    /// Writes all of `buf` at `offset`, growing the file if needed. A gap between
    /// the old end and `offset` reads back as zeros.
    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()>;

    /// Cuts or extends the file to `size` bytes. Extension reads back as zeros.
    fn truncate(&mut self, size: u64) -> Result<()>;

    /// The file's current size in bytes.
    fn size(&mut self) -> Result<u64>;

    /// Makes every earlier write and truncation durable.
    fn flush(&mut self) -> Result<()>;
}

/// Checks that `name` is a plain file name a back end can store.
pub(crate) fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 {
        return Err(crate::Error::invalid(format!(
            "file name must be 1 to 255 bytes, got {:?}",
            name
        )));
    }
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err(crate::Error::invalid(format!(
            "file name {name:?} must not contain a path separator"
        )));
    }
    Ok(())
}

/// Reads exactly `buf.len()` bytes or fails with a short-read error.
pub(crate) fn read_exact_at(file: &mut dyn StorageFile, buf: &mut [u8], offset: u64) -> Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(crate::Error::corrupt(format!(
                "short read: wanted {} bytes at offset {offset}, file ended after {done}",
                buf.len()
            )));
        }
        done += n;
    }
    Ok(())
}
