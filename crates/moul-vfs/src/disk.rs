//! Storage in a folder on disk, for the native binary, tests and the hub.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::storage::{Storage, StorageFile, check_name};
use crate::{Error, Result};

/// A folder on the local file system. Each name is a file directly inside it.
#[derive(Clone, Debug)]
pub struct DiskStorage {
    dir: PathBuf,
}

impl DiskStorage {
    /// Uses `dir`, creating it and its parents if needed.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)
            .map_err(|e| Error::io(format!("cannot create {}: {e}", dir.display())))?;
        Ok(Self { dir })
    }

    /// The folder this storage writes to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Storage for DiskStorage {
    fn open(&self, name: &str) -> Result<Box<dyn StorageFile>> {
        check_name(name)?;
        let path = self.dir.join(name);
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::io(format!("cannot open {}: {e}", path.display())))?;
        if !existed {
            sync_dir(&self.dir)?;
        }
        Ok(Box::new(DiskFile { file, path }))
    }

    fn describe(&self) -> String {
        self.dir.display().to_string()
    }
}

/// Makes a new directory entry durable. Windows has no directory handle to
/// flush; creating a file there is durable once the file itself is flushed.
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| Error::io(format!("cannot flush folder {}: {e}", dir.display())))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

struct DiskFile {
    file: File,
    path: PathBuf,
}

impl DiskFile {
    fn err(&self, what: &str, e: std::io::Error) -> Error {
        Error::io(format!("{what} {}: {e}", self.path.display()))
    }
}

impl StorageFile for DiskFile {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            match positioned::read(&self.file, &mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.err("cannot read", e)),
            }
        }
        Ok(done)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        let mut done = 0;
        while done < buf.len() {
            match positioned::write(&self.file, &buf[done..], offset + done as u64) {
                Ok(0) => {
                    return Err(Error::io(format!(
                        "cannot write {}: storage accepted no bytes",
                        self.path.display()
                    )));
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.err("cannot write", e)),
            }
        }
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.file
            .set_len(size)
            .map_err(|e| self.err("cannot truncate", e))
    }

    fn size(&mut self) -> Result<u64> {
        self.file
            .metadata()
            .map(|m| m.len())
            .map_err(|e| self.err("cannot stat", e))
    }

    fn flush(&mut self) -> Result<()> {
        // sync_all and not sync_data: a truncation changes the length, which
        // is metadata, and must be durable too.
        self.file
            .sync_all()
            .map_err(|e| self.err("cannot flush", e))
    }
}

#[cfg(unix)]
mod positioned {
    use std::fs::File;
    use std::os::unix::fs::FileExt;

    pub fn read(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        file.read_at(buf, offset)
    }

    pub fn write(file: &File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
        file.write_at(buf, offset)
    }
}

#[cfg(windows)]
mod positioned {
    use std::fs::File;
    use std::os::windows::fs::FileExt;

    pub fn read(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        file.seek_read(buf, offset)
    }

    pub fn write(file: &File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
        file.seek_write(buf, offset)
    }
}
