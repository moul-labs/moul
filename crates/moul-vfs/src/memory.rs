//! Storage in memory, with a crash simulator for tests.
//!
//! [`MemoryStorage`] keeps two copies of each file: what a reader sees now and
//! what would survive a crash, which is the state at the last flush. A test can
//! make any step fail, then call [`MemoryStorage::crash`] to throw away writes
//! that were never flushed, in whole or in part, and reopen the volume on what
//! is left. It also backs a volume that has nowhere durable to live, such as a
//! private window.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::storage::{Storage, StorageFile, check_name};
use crate::{Error, Result};

/// What a simulated crash does with writes that were not flushed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashMode {
    /// Every unflushed write and truncation is lost.
    DropUnflushed,
    /// Every unflushed write and truncation survives, as if the crash came
    /// just after the storage caught up.
    KeepUnflushed,
    /// For each file, an arbitrary prefix of its unflushed operations survives
    /// and the next one is torn: half of a write lands. Files are cut
    /// independently, so a later write to one file can survive while an earlier
    /// write to another is lost. The seed makes the choice repeatable.
    Torn(u64),
}

#[derive(Clone, Debug)]
enum Op {
    Write { offset: u64, data: Vec<u8> },
    Truncate { size: u64 },
}

#[derive(Debug, Default)]
struct MemFile {
    current: Vec<u8>,
    durable: Vec<u8>,
    unflushed: Vec<Op>,
}

#[derive(Debug, Default)]
struct Disk {
    files: BTreeMap<String, MemFile>,
    /// Bumped by every crash; handles from an older generation stop working.
    generation: u64,
    /// Mutating operations so far: writes, truncations and flushes.
    steps: u64,
    /// The step that fails. It and every step after it return an error, like
    /// a device that has gone away.
    fail_at: Option<u64>,
}

impl Disk {
    fn step(&mut self, what: &str, name: &str) -> Result<()> {
        let step = self.steps;
        self.steps += 1;
        match self.fail_at {
            Some(at) if step >= at => Err(Error::io(format!(
                "injected failure at step {step} ({what} {name})"
            ))),
            _ => Ok(()),
        }
    }
}

/// A folder of files held in memory. Clones share the same files.
#[derive(Clone, Debug, Default)]
pub struct MemoryStorage {
    disk: Arc<Mutex<Disk>>,
}

impl MemoryStorage {
    /// An empty folder.
    pub fn new() -> Self {
        Self::default()
    }

    fn disk(&self) -> MutexGuard<'_, Disk> {
        self.disk.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many mutating steps (writes, truncations, flushes) have run.
    pub fn steps(&self) -> u64 {
        self.disk().steps
    }

    /// Makes the step numbered `step` fail, and every step after it, counting
    /// from the start of this storage's life. `None` turns failures off.
    pub fn fail_at(&self, step: Option<u64>) {
        self.disk().fail_at = step;
    }

    /// Simulates power loss: every open handle stops working, and each file
    /// keeps its flushed content plus whatever `mode` lets survive. Failure
    /// injection is turned off, so the next volume can open.
    pub fn crash(&self, mode: CrashMode) {
        let mut disk = self.disk();
        disk.generation += 1;
        disk.fail_at = None;
        let mut rng = match mode {
            CrashMode::Torn(seed) => seed ^ 0x9E37_79B9_7F4A_7C15,
            _ => 0,
        };
        for file in disk.files.values_mut() {
            let ops = std::mem::take(&mut file.unflushed);
            let (keep, tear) = match mode {
                CrashMode::DropUnflushed => (0, false),
                CrashMode::KeepUnflushed => (ops.len(), false),
                CrashMode::Torn(_) => {
                    let keep = (next_random(&mut rng) % (ops.len() as u64 + 1)) as usize;
                    (keep, keep < ops.len())
                }
            };
            let mut content = std::mem::take(&mut file.durable);
            for op in &ops[..keep] {
                apply(&mut content, op);
            }
            if tear && let Op::Write { offset, data } = &ops[keep] {
                let half = data.len() / 2;
                apply(
                    &mut content,
                    &Op::Write {
                        offset: *offset,
                        data: data[..half].to_vec(),
                    },
                );
            }
            file.current = content.clone();
            file.durable = content;
        }
    }

    /// An independent copy holding each file's flushed content, as a fresh
    /// device would after a clean shutdown. Steps and failures start over.
    pub fn fork(&self) -> MemoryStorage {
        let disk = self.disk();
        let files = disk
            .files
            .iter()
            .map(|(name, f)| {
                let file = MemFile {
                    current: f.durable.clone(),
                    durable: f.durable.clone(),
                    unflushed: Vec::new(),
                };
                (name.clone(), file)
            })
            .collect();
        MemoryStorage {
            disk: Arc::new(Mutex::new(Disk {
                files,
                ..Disk::default()
            })),
        }
    }

    /// The file's current content, or `None` if it does not exist.
    pub fn file(&self, name: &str) -> Option<Vec<u8>> {
        self.disk().files.get(name).map(|f| f.current.clone())
    }

    /// Replaces a file's content, flushed. For setting up tests.
    pub fn set_file(&self, name: &str, content: &[u8]) {
        let mut disk = self.disk();
        let file = disk.files.entry(name.to_owned()).or_default();
        file.current = content.to_vec();
        file.durable = content.to_vec();
        file.unflushed.clear();
    }

    /// The names of all files, in order.
    pub fn names(&self) -> Vec<String> {
        self.disk().files.keys().cloned().collect()
    }
}

fn next_random(state: &mut u64) -> u64 {
    // SplitMix64: small, fast and good enough to pick crash points.
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn apply(content: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write { offset, data } => {
            let start = *offset as usize;
            let end = start + data.len();
            if content.len() < end {
                content.resize(end, 0);
            }
            content[start..end].copy_from_slice(data);
        }
        Op::Truncate { size } => content.resize(*size as usize, 0),
    }
}

impl Storage for MemoryStorage {
    fn open(&self, name: &str) -> Result<Box<dyn StorageFile>> {
        check_name(name)?;
        let mut disk = self.disk();
        disk.files.entry(name.to_owned()).or_default();
        Ok(Box::new(MemoryFile {
            disk: self.disk.clone(),
            name: name.to_owned(),
            generation: disk.generation,
        }))
    }

    fn describe(&self) -> String {
        "memory".to_owned()
    }
}

struct MemoryFile {
    disk: Arc<Mutex<Disk>>,
    name: String,
    generation: u64,
}

impl MemoryFile {
    fn with<T>(&self, what: &str, mutating: bool, f: impl FnOnce(&mut MemFile) -> T) -> Result<T> {
        let mut disk = self.disk.lock().unwrap_or_else(|e| e.into_inner());
        if disk.generation != self.generation {
            return Err(Error::io(format!("{} was lost in a crash", self.name)));
        }
        if mutating {
            disk.step(what, &self.name)?;
        }
        let file = disk
            .files
            .get_mut(&self.name)
            .ok_or_else(|| Error::io(format!("{} was removed", self.name)))?;
        Ok(f(file))
    }
}

impl StorageFile for MemoryFile {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.with("read", false, |file| {
            let len = file.current.len() as u64;
            if offset >= len {
                return 0;
            }
            let start = offset as usize;
            let n = buf.len().min(file.current.len() - start);
            buf[..n].copy_from_slice(&file.current[start..start + n]);
            n
        })
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        self.with("write", true, |file| {
            let op = Op::Write {
                offset,
                data: buf.to_vec(),
            };
            apply(&mut file.current, &op);
            file.unflushed.push(op);
        })
    }

    fn truncate(&mut self, size: u64) -> Result<()> {
        self.with("truncate", true, |file| {
            let op = Op::Truncate { size };
            apply(&mut file.current, &op);
            file.unflushed.push(op);
        })
    }

    fn size(&mut self) -> Result<u64> {
        self.with("size", false, |file| file.current.len() as u64)
    }

    fn flush(&mut self) -> Result<()> {
        self.with("flush", true, |file| {
            file.durable = file.current.clone();
            file.unflushed.clear();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushed_writes_survive_a_crash_and_unflushed_ones_do_not() {
        let storage = MemoryStorage::new();
        let mut f = storage.open("a").unwrap();
        f.write_at(b"hello", 0).unwrap();
        f.flush().unwrap();
        f.write_at(b"HELLO world", 0).unwrap();
        storage.crash(CrashMode::DropUnflushed);
        assert_eq!(storage.file("a").unwrap(), b"hello");
        assert!(f.size().is_err(), "old handles die in a crash");
    }

    #[test]
    fn torn_crash_keeps_a_prefix() {
        for seed in 0..50 {
            let storage = MemoryStorage::new();
            let mut f = storage.open("a").unwrap();
            f.write_at(&[1; 8], 0).unwrap();
            f.write_at(&[2; 8], 8).unwrap();
            f.write_at(&[3; 8], 16).unwrap();
            storage.crash(CrashMode::Torn(seed));
            let got = storage.file("a").unwrap();
            let full = [[1u8; 8], [2; 8], [3; 8]].concat();
            assert!(full.starts_with(&got), "seed {seed}: {got:?}");
        }
    }

    #[test]
    fn injected_failures_are_sticky() {
        let storage = MemoryStorage::new();
        let mut f = storage.open("a").unwrap();
        storage.fail_at(Some(1));
        f.write_at(b"x", 0).unwrap();
        assert!(f.flush().is_err());
        assert!(f.write_at(b"y", 0).is_err());
        assert_eq!(storage.steps(), 3);
    }
}
