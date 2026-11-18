//! Commit capture: every commit that changes one database file becomes a
//! level-0 LTX file, the unit a replica is built from.
//!
//! [`Capture`] is a volume [`Observer`]. For each commit that touches its file
//! (`main.db`, the shared store) it writes the next TXID as
//! `<txid>-<txid>.ltx` into its own storage, the `ltx/` folder, flushes it,
//! then records the TXID and commit number in `capture.state` and flushes
//! that. A crash between the two leaves a file the state does not count yet;
//! the commit is still in the volume's log, so the next open hands it over
//! again and the file is written afresh under the same name.
//!
//! TXID 1 holds every page, as LTX requires of a file that starts a replica.
//! So does any file that follows a commit capture never saw (the volume was
//! opened without it): SQLite bumps the change counter in page 1 on every
//! write transaction, so a counter that did not move by exactly one means the
//! file changed behind capture's back, and a whole copy puts the replica
//! right.
//!
//! ```text
//! capture.state: two 64-byte slots, the newer valid one wins
//!   magic "MOULCAP1" | version u32 | page size u32 | generation u64 | seq u64
//!   | txid u64 | change counter u32 | reserved (12 bytes) | crc64 of bytes 0..56
//! ```

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use moul_ltx::{Crc64, Encoder, Header, Txid, file_name, lock_pgno};

use crate::log::{Commit, CommitSeq, FileCommit};
use crate::storage::{MaybeSend, Storage, StorageFile, check_name, read_exact_at};
use crate::volume::{Before, Observer, Volume};
use crate::{Error, Result};

/// The file in a capture folder that records how far capture has got.
pub const STATE_NAME: &str = "capture.state";
const SLOT_LEN: usize = 64;
const MAGIC: &[u8; 8] = b"MOULCAP1";
const VERSION: u32 = 1;

/// How far capture has got: the last TXID written and the commit it came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// The last commit captured, or skipped because it did not touch the file.
    pub seq: CommitSeq,
    /// The last TXID written, or `Txid(0)` before the first.
    pub txid: Txid,
    /// Page size of the last file written.
    pub page_size: u32,
    /// The change counter in page 1 after the last file written.
    pub change_counter: u32,
}

/// Milliseconds since the Unix epoch, for LTX timestamps.
pub type Clock = Box<dyn FnMut() -> i64 + 'static>;

/// The time now, from the system clock or, in the browser, `Date.now()`.
pub fn system_clock() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    }
}

struct Inner {
    storage: Box<dyn Storage>,
    state_file: Box<dyn StorageFile>,
    file: String,
    position: Position,
    generation: u64,
    slot: usize,
    clock: Clock,
}

// SAFETY: on native targets every field is `Send` but the clock, which
// `Capture::with_clock` only accepts when it is `MaybeSend` (`Send` there).
// `Inner` is only reached through its mutex.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for Inner {}

/// Captures the commits to one database file of a volume as level-0 LTX
/// files. Clones share the same state.
///
/// ```
/// # use rusqlite as _; // links SQLite, which the VFS glue needs
/// use moul_vfs::{Capture, MemoryStorage, Volume};
///
/// let ltx = MemoryStorage::new();
/// let capture = Capture::open(ltx.clone(), "main.db").unwrap();
/// let volume = Volume::open_observed(MemoryStorage::new(), capture.clone()).unwrap();
/// // ... open SQLite on `volume.vfs_name()` and write ...
/// assert_eq!(capture.position().txid.0, 0);
/// ```
#[derive(Clone)]
pub struct Capture {
    inner: Arc<Mutex<Inner>>,
}

impl fmt::Debug for Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        f.debug_struct("Capture")
            .field("file", &inner.file)
            .field("position", &inner.position)
            .finish_non_exhaustive()
    }
}

fn encode_slot(p: Position, generation: u64) -> [u8; SLOT_LEN] {
    let mut b = [0u8; SLOT_LEN];
    b[0..8].copy_from_slice(MAGIC);
    b[8..12].copy_from_slice(&VERSION.to_be_bytes());
    b[12..16].copy_from_slice(&p.page_size.to_be_bytes());
    b[16..24].copy_from_slice(&generation.to_be_bytes());
    b[24..32].copy_from_slice(&p.seq.0.to_be_bytes());
    b[32..40].copy_from_slice(&p.txid.0.to_be_bytes());
    b[40..44].copy_from_slice(&p.change_counter.to_be_bytes());
    let crc = Crc64::checksum(&b[..56]);
    b[56..64].copy_from_slice(&crc.to_be_bytes());
    b
}

fn decode_slot(b: &[u8]) -> Option<(Position, u64)> {
    if b.len() != SLOT_LEN || &b[0..8] != MAGIC {
        return None;
    }
    let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().expect("4 bytes"));
    let u64_at = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().expect("8 bytes"));
    if Crc64::checksum(&b[..56]) != u64_at(56) || u32_at(8) != VERSION {
        return None;
    }
    let p = Position {
        page_size: u32_at(12),
        seq: CommitSeq(u64_at(24)),
        txid: Txid(u64_at(32)),
        change_counter: u32_at(40),
    };
    Some((p, u64_at(16)))
}

fn change_counter(page1: &[u8]) -> u32 {
    u32::from_be_bytes(page1[24..28].try_into().expect("4 bytes"))
}

impl Capture {
    /// Opens capture for database file `file` (usually `main.db`), keeping its
    /// LTX files and state in `storage`.
    pub fn open(storage: impl Storage + 'static, file: &str) -> Result<Self> {
        check_name(file)?;
        let storage: Box<dyn Storage> = Box::new(storage);
        let mut state_file = storage.open(STATE_NAME)?;
        let mut buf = [0u8; 2 * SLOT_LEN];
        let n = state_file.read_at(&mut buf, 0)?;
        let mut best: Option<(Position, u64, usize)> = None;
        for slot in 0..2 {
            if n < (slot + 1) * SLOT_LEN {
                break;
            }
            if let Some((p, g)) = decode_slot(&buf[slot * SLOT_LEN..(slot + 1) * SLOT_LEN])
                && best.is_none_or(|(_, bg, _)| g > bg)
            {
                best = Some((p, g, slot));
            }
        }
        // The first record goes to slot 0. If it is torn and slot 1 was
        // never written, nothing was ever recorded, so capture starts afresh.
        let never_written = buf[SLOT_LEN..n.max(SLOT_LEN)].iter().all(|&b| b == 0);
        let (position, generation, slot) = match best {
            Some(found) => found,
            None if never_written => (Position::default(), 0, 1),
            None => {
                return Err(Error::corrupt(format!(
                    "{STATE_NAME} in {} has no valid record",
                    storage.describe()
                )));
            }
        };
        // In the browser storage handles are not `Send`, and there is one thread.
        #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
        let inner = Arc::new(Mutex::new(Inner {
            storage,
            state_file,
            file: file.to_owned(),
            position,
            generation,
            slot,
            clock: Box::new(system_clock),
        }));
        Ok(Self { inner })
    }

    /// Uses `clock` for file timestamps instead of the system clock, so tests
    /// can write the same bytes every run.
    pub fn with_clock(self, clock: impl FnMut() -> i64 + MaybeSend + 'static) -> Self {
        self.lock().clock = Box::new(clock);
        self
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How far capture has got.
    pub fn position(&self) -> Position {
        self.lock().position
    }

    /// The bytes of the level-0 file for `txid`, which must be at or before
    /// [`position`](Self::position).
    pub fn read(&self, txid: Txid) -> Result<Vec<u8>> {
        let inner = self.lock();
        if txid.0 == 0 || txid > inner.position.txid {
            return Err(Error::invalid(format!(
                "TXID {txid} has not been captured; capture is at {}",
                inner.position.txid
            )));
        }
        let mut f = inner.storage.open(&file_name(txid, txid))?;
        let mut out = vec![0u8; f.size()? as usize];
        read_exact_at(f.as_mut(), &mut out, 0)?;
        Ok(out)
    }

    /// Captures the whole file as it is now, if nothing has been captured yet
    /// and the volume already holds it: a database that existed before
    /// capture was turned on. Returns the TXID written, if any.
    pub fn snapshot_existing(&self, volume: &Volume) -> Result<Option<Txid>> {
        let name = self.lock().file.clone();
        // Read before locking capture: the volume calls capture with its own
        // lock held, so taking them the other way round could deadlock.
        let (seq, bytes) = volume.snapshot(&name)?;
        let mut inner = self.lock();
        if inner.position.txid.0 != 0 || bytes.len() < 100 || seq <= inner.position.seq {
            return Ok(None);
        }
        let ps = page_size_of(&bytes)?;
        let size = (bytes.len() / ps as usize) as u32;
        let page = |pgno: u32| {
            let at = (pgno as usize - 1) * ps as usize;
            &bytes[at..at + ps as usize]
        };
        let counter = change_counter(page(1));
        let txid = inner.write_file(seq, ps, size, counter, |pgno, buf| {
            buf.copy_from_slice(page(pgno));
            Ok(())
        })?;
        Ok(Some(txid))
    }
}

fn page_size_of(page1: &[u8]) -> Result<u32> {
    let raw = u16::from_be_bytes([page1[16], page1[17]]);
    let ps = if raw == 1 { 65_536 } else { raw as u32 };
    if moul_ltx::is_valid_page_size(ps) {
        Ok(ps)
    } else {
        Err(Error::corrupt(format!("page 1 gives a page size of {ps}")))
    }
}

impl Inner {
    /// Writes the next TXID holding pages `1..=size` (but the lock page), each
    /// filled by `page`, then records it. Used for whole copies.
    fn write_file(
        &mut self,
        seq: CommitSeq,
        page_size: u32,
        size: u32,
        counter: u32,
        mut page: impl FnMut(u32, &mut [u8]) -> Result<()>,
    ) -> Result<Txid> {
        let lock = lock_pgno(page_size);
        let mut buf = vec![0u8; page_size as usize];
        self.write_pages(seq, page_size, size, counter, |enc| {
            for pgno in (1..=size).filter(|&p| p != lock) {
                page(pgno, &mut buf)?;
                enc.write_page(pgno, &buf).map_err(ltx_error)?;
            }
            Ok(())
        })
    }

    fn write_pages(
        &mut self,
        seq: CommitSeq,
        page_size: u32,
        size: u32,
        counter: u32,
        fill: impl FnOnce(&mut Encoder<Vec<u8>>) -> Result<()>,
    ) -> Result<Txid> {
        if size == 0 {
            return Err(Error::invalid(format!(
                "commit {seq} leaves {} empty, which LTX cannot express",
                self.file
            )));
        }
        let txid = Txid(self.position.txid.0 + 1);
        let header = Header::new(page_size, size, txid, txid, (self.clock)());
        let mut enc = Encoder::new(Vec::new(), header).map_err(ltx_error)?;
        fill(&mut enc)?;
        let (bytes, _) = enc.finish().map_err(ltx_error)?;

        let mut f = self.storage.open(&file_name(txid, txid))?;
        f.write_at(&bytes, 0)?;
        f.truncate(bytes.len() as u64)?;
        f.flush()?;

        let position = Position {
            seq,
            txid,
            page_size,
            change_counter: counter,
        };
        let slot = 1 - self.slot;
        let generation = self.generation + 1;
        self.state_file
            .write_at(&encode_slot(position, generation), (slot * SLOT_LEN) as u64)?;
        self.state_file.flush()?;
        self.slot = slot;
        self.generation = generation;
        self.position = position;
        Ok(txid)
    }

    fn capture(&mut self, seq: CommitSeq, fc: &FileCommit, before: &mut Before<'_>) -> Result<()> {
        let ps = fc.page_size;
        let lock = lock_pgno(ps);
        let size = fc.size;
        let counter = fc.pages.get(&1).map(|p| change_counter(p));
        let whole =
            fc.pages.range(1..=size).count() as u64 == size as u64 - u64::from(lock <= size);
        let p = self.position;
        let follows = p.txid.0 > 0
            && p.page_size == ps
            && counter.is_some_and(|c| c == p.change_counter.wrapping_add(1));
        let counter = counter.unwrap_or(p.change_counter);

        if follows || whole {
            // The commit's own pages, and zeros for any page the file was cut
            // below and grew back over without writing.
            let zeros = vec![0u8; ps as usize];
            return self
                .write_pages(seq, ps, size, counter, |enc| {
                    // Pages past `truncated_to` that the commit did not write.
                    let zeros_until = |enc: &mut Encoder<Vec<u8>>, from: &mut u32, end: u32| {
                        while *from < end {
                            if *from != lock {
                                enc.write_page(*from, &zeros).map_err(ltx_error)?;
                            }
                            *from += 1;
                        }
                        Ok::<_, Error>(())
                    };
                    let mut next_zero = fc.truncated_to.saturating_add(1);
                    for (&pgno, page) in fc.pages.range(1..=size) {
                        zeros_until(enc, &mut next_zero, pgno)?;
                        enc.write_page(pgno, page).map_err(ltx_error)?;
                        next_zero = next_zero.max(pgno + 1);
                    }
                    zeros_until(enc, &mut next_zero, size + 1)
                })
                .map(|_| ());
        }

        // The first file, or one after commits capture never saw: copy the
        // whole file, filling the pages the commit did not write from before.
        let name = fc.name.clone();
        self.write_file(seq, ps, size, counter, |pgno, buf| {
            if let Some(page) = fc.pages.get(&pgno) {
                buf.copy_from_slice(page);
            } else if pgno <= fc.truncated_to {
                let n = before.read(&name, buf, (pgno as u64 - 1) * ps as u64)?;
                buf[n..].fill(0);
            } else {
                buf.fill(0);
            }
            Ok(())
        })
        .map(|_| ())
    }
}

fn ltx_error(e: moul_ltx::Error) -> Error {
    Error::invalid(format!("LTX: {e}"))
}

impl Observer for Capture {
    fn commit(&mut self, commit: &Commit, before: &mut Before<'_>) -> Result<()> {
        let mut inner = self.lock();
        if commit.seq <= inner.position.seq {
            return Ok(()); // already captured, and handed over again on reopen
        }
        match commit.files.iter().find(|f| f.name == inner.file) {
            Some(fc) => inner.capture(commit.seq, fc, before),
            None => {
                // Nothing to write. Not recorded durably: after a reopen the
                // commit is either gone from the log or handed over again.
                inner.position.seq = commit.seq;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_slots_round_trip_and_reject_damage() {
        let p = Position {
            seq: CommitSeq(9),
            txid: Txid(4),
            page_size: 4096,
            change_counter: 77,
        };
        let b = encode_slot(p, 3);
        assert_eq!(decode_slot(&b), Some((p, 3)));
        for i in 0..SLOT_LEN {
            let mut bad = b;
            bad[i] ^= 1;
            assert_eq!(decode_slot(&bad), None, "byte {i}");
        }
    }
}
