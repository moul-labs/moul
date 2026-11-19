//! Commit capture: every commit that changes one database file becomes a
//! level-0 LTX file, the unit a replica is built from.
//!
//! [`Capture`] is a volume [`Observer`]. For each commit that touches its file
//! (`main.db`, the shared store, or `writer.db`, the writer-only store, each
//! with a capture of its own under a [`Fanout`](crate::Fanout)) it appends
//! the next TXID's file to
//! `ltx.pack` in its own storage, flushes it, then records the TXID and commit
//! number in `capture.state` and flushes that. A crash between the two leaves
//! a record the state does not count yet; the commit is still in the volume's
//! log, so the next open hands it over again and the record is written afresh
//! over the stale one.
//!
//! The files live in one pack, not one storage file each, because the browser
//! back end can only serve files it opened in advance. Sync reads them with
//! [`Capture::read`] and drops them with [`Capture::release`] once the replica
//! holds them.
//!
//! TXID 1 holds every page, as LTX requires of a file that starts a replica.
//! So does any file that follows a commit capture never saw (the volume was
//! opened without it): SQLite bumps the change counter in page 1 on every
//! write transaction, so a counter that did not move by exactly one means the
//! file changed behind capture's back, and a whole copy puts the replica
//! right.
//!
//! # Following a replica
//!
//! A device that follows another writer applies the writer's files with
//! [`Capture::apply`]: their pages go into the volume as one commit, exactly
//! as written, and capture records the writer's TXID instead of writing a file
//! of its own. The expected TXID is recorded durably first, so a crash in
//! between cannot turn the writer's commit into a local file with the wrong
//! number. [`Capture::fence`] and [`Capture::rebase`] serve hand-off and a
//! new replica.
//!
//! ```text
//! capture.state: two 64-byte slots, the newer valid one wins
//!   magic "MOULCAP1" | version u32 | page size u32 | generation u64 | seq u64
//!   | txid u64 | change counter u32 | expected txid u64 | expect flags u32
//!   | crc64 of bytes 0..56
//! ltx.pack: records, one per TXID, in order
//!   magic "MLTX" | txid u64 | length u32 | LTX file | crc64 of everything before
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use moul_ltx::{Crc64, Encoder, Header, Ltx, Txid, is_contiguous, lock_pgno};

use crate::log::{Commit, CommitSeq, FileCommit};
use crate::storage::{MaybeSend, Storage, StorageFile, check_name, read_exact_at};
use crate::volume::{Before, Observer, Volume};
use crate::{Error, ErrorKind, Result};

/// The file in a capture folder that records how far capture has got.
pub const STATE_NAME: &str = "capture.state";
/// The file in a capture folder that holds the level-0 files.
pub const PACK_NAME: &str = "ltx.pack";
const SLOT_LEN: usize = 64;
const MAGIC: &[u8; 8] = b"MOULCAP1";
const VERSION: u32 = 1;
const RECORD_MAGIC: &[u8; 4] = b"MLTX";
const RECORD_HEAD: usize = 16;
const RECORD_TRAILER: usize = 8;
const EXPECT_SET: u32 = 1;
const EXPECT_REPLACE: u32 = 2;

/// How far capture has got: the last TXID written and the commit it came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// The last commit captured, or skipped because it did not touch the file.
    pub seq: CommitSeq,
    /// The last TXID written or applied, or `Txid(0)` before the first.
    pub txid: Txid,
    /// Page size of the last file written.
    pub page_size: u32,
    /// The change counter in page 1 after the last file written.
    pub change_counter: u32,
}

/// How [`Capture::apply`] treats the database it applies a file to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyMode {
    /// The database is at the TXID before the file: add the file's pages. A
    /// snapshot replaces the database whatever the mode.
    Follow,
    /// Replace the database with the file, which must be a snapshot or a
    /// whole restore, and drop every level-0 file not yet released. Used to
    /// join a replica and to resync after losing the writer role.
    Replace,
}

/// A TXID that the next commit to the file is known to be, because
/// [`Capture::apply`] is writing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Expect {
    txid: Txid,
    replace: bool,
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

/// Where each TXID's file sits in the pack: offset of the LTX bytes, length.
type PackIndex = BTreeMap<Txid, (u64, u32)>;

struct Inner {
    storage_name: String,
    state_file: Box<dyn StorageFile>,
    pack: Box<dyn StorageFile>,
    index: PackIndex,
    /// The end of the last record the state counts.
    end: u64,
    file: String,
    position: Position,
    expect: Option<Expect>,
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
///
/// Capture and the volume it observes must be used from one thread at a
/// time: [`apply`](Self::apply) relies on no other commit landing while it
/// writes.
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

fn encode_slot(p: Position, expect: Option<Expect>, generation: u64) -> [u8; SLOT_LEN] {
    let mut b = [0u8; SLOT_LEN];
    b[0..8].copy_from_slice(MAGIC);
    b[8..12].copy_from_slice(&VERSION.to_be_bytes());
    b[12..16].copy_from_slice(&p.page_size.to_be_bytes());
    b[16..24].copy_from_slice(&generation.to_be_bytes());
    b[24..32].copy_from_slice(&p.seq.0.to_be_bytes());
    b[32..40].copy_from_slice(&p.txid.0.to_be_bytes());
    b[40..44].copy_from_slice(&p.change_counter.to_be_bytes());
    if let Some(e) = expect {
        b[44..52].copy_from_slice(&e.txid.0.to_be_bytes());
        let flags = EXPECT_SET | if e.replace { EXPECT_REPLACE } else { 0 };
        b[52..56].copy_from_slice(&flags.to_be_bytes());
    }
    let crc = Crc64::checksum(&b[..56]);
    b[56..64].copy_from_slice(&crc.to_be_bytes());
    b
}

fn decode_slot(b: &[u8]) -> Option<(Position, Option<Expect>, u64)> {
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
    let flags = u32_at(52);
    let expect = (flags & EXPECT_SET != 0).then(|| Expect {
        txid: Txid(u64_at(44)),
        replace: flags & EXPECT_REPLACE != 0,
    });
    Some((p, expect, u64_at(16)))
}

fn change_counter(page1: &[u8]) -> u32 {
    u32::from_be_bytes(page1[24..28].try_into().expect("4 bytes"))
}

fn encode_record(txid: Txid, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_HEAD + bytes.len() + RECORD_TRAILER);
    out.extend_from_slice(RECORD_MAGIC);
    out.extend_from_slice(&txid.0.to_be_bytes());
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    let crc = Crc64::checksum(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// Reads the pack's records in order and returns the index of those the
/// state counts (TXID at or before `upto`, without a gap) and where they end.
fn scan_pack(pack: &mut dyn StorageFile, upto: Txid) -> Result<(PackIndex, u64)> {
    let len = pack.size()?;
    let mut index = BTreeMap::new();
    let mut offset = 0u64;
    let mut end = 0u64;
    let mut previous: Option<Txid> = None;
    while offset + (RECORD_HEAD + RECORD_TRAILER) as u64 <= len {
        let mut head = [0u8; RECORD_HEAD];
        read_exact_at(pack, &mut head, offset)?;
        if &head[0..4] != RECORD_MAGIC {
            break;
        }
        let txid = Txid(u64::from_be_bytes(head[4..12].try_into().expect("8 bytes")));
        let n = u32::from_be_bytes(head[12..16].try_into().expect("4 bytes"));
        let total = (RECORD_HEAD + RECORD_TRAILER) as u64 + n as u64;
        if offset + total > len {
            break; // torn
        }
        let mut record = vec![0u8; total as usize];
        read_exact_at(pack, &mut record, offset)?;
        let body = record.len() - RECORD_TRAILER;
        let crc = u64::from_be_bytes(record[body..].try_into().expect("8 bytes"));
        if Crc64::checksum(&record[..body]) != crc {
            break; // torn
        }
        if txid > upto || previous.is_some_and(|p| txid.0 != p.0 + 1) {
            break; // written after the state, or stale
        }
        previous = Some(txid);
        index.insert(txid, (offset + RECORD_HEAD as u64, n));
        offset += total;
        end = offset;
    }
    Ok((index, end))
}

impl Capture {
    /// Opens capture for database file `file` (usually `main.db`), keeping its
    /// state and level-0 files in `storage`, which must serve
    /// [`STATE_NAME`] and [`PACK_NAME`].
    pub fn open(storage: impl Storage + 'static, file: &str) -> Result<Self> {
        check_name(file)?;
        let mut state_file = storage.open(STATE_NAME)?;
        let mut buf = [0u8; 2 * SLOT_LEN];
        let n = state_file.read_at(&mut buf, 0)?;
        let mut best: Option<(Position, Option<Expect>, u64, usize)> = None;
        for slot in 0..2 {
            if n < (slot + 1) * SLOT_LEN {
                break;
            }
            if let Some((p, e, g)) = decode_slot(&buf[slot * SLOT_LEN..(slot + 1) * SLOT_LEN])
                && best.is_none_or(|(_, _, bg, _)| g > bg)
            {
                best = Some((p, e, g, slot));
            }
        }
        // The first record goes to slot 0. If it is torn and slot 1 was
        // never written, nothing was ever recorded, so capture starts afresh.
        let never_written = buf[SLOT_LEN..n.max(SLOT_LEN)].iter().all(|&b| b == 0);
        let (position, expect, generation, slot) = match best {
            Some(found) => found,
            None if never_written => (Position::default(), None, 0, 1),
            None => {
                return Err(Error::corrupt(format!(
                    "{STATE_NAME} in {} has no valid record",
                    storage.describe()
                )));
            }
        };
        let mut pack = storage.open(PACK_NAME)?;
        let (index, end) = scan_pack(pack.as_mut(), position.txid)?;
        // In the browser storage handles are not `Send`, and there is one thread.
        #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
        let inner = Arc::new(Mutex::new(Inner {
            storage_name: storage.describe(),
            state_file,
            pack,
            index,
            end,
            file: file.to_owned(),
            position,
            expect,
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

    /// The database file captured, such as `main.db`.
    pub fn file(&self) -> String {
        self.lock().file.clone()
    }

    /// The first TXID whose file is still held, if any.
    pub fn first_held(&self) -> Option<Txid> {
        self.lock().index.keys().next().copied()
    }

    /// The bytes of the level-0 file for `txid`, which must be at or before
    /// [`position`](Self::position) and not yet released.
    pub fn read(&self, txid: Txid) -> Result<Vec<u8>> {
        let mut inner = self.lock();
        if txid.0 == 0 || txid > inner.position.txid {
            return Err(Error::invalid(format!(
                "TXID {txid} has not been captured; capture is at {}",
                inner.position.txid
            )));
        }
        let Some(&(offset, len)) = inner.index.get(&txid) else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!(
                    "the file for TXID {txid} is not held: it was released or applied from the replica"
                ),
            ));
        };
        let mut out = vec![0u8; len as usize];
        read_exact_at(inner.pack.as_mut(), &mut out, offset)?;
        Ok(out)
    }

    /// Drops the files up to and including `upto`, which the replica now
    /// holds. The pack is emptied once every file in it is released.
    pub fn release(&self, upto: Txid) -> Result<()> {
        let mut inner = self.lock();
        if inner.index.keys().next_back().is_some_and(|&t| t > upto) {
            return Ok(()); // a file still to push; drop them all together later
        }
        if inner.end == 0 && inner.index.is_empty() {
            return Ok(());
        }
        inner.clear_pack()
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
        inner.write_whole(seq, &bytes).map(Some)
    }

    /// Starts the numbering again: drops every file held and captures the
    /// whole file as it is now as TXID 1, the start of a new replica. Returns
    /// `None`, and leaves the next commit to become TXID 1, when the file is
    /// empty.
    pub fn rebase(&self, volume: &Volume) -> Result<Option<Txid>> {
        let name = self.lock().file.clone();
        let (seq, bytes) = volume.snapshot(&name)?;
        let mut inner = self.lock();
        inner.clear_pack()?;
        let ps = inner.position.page_size;
        inner.position = Position {
            seq,
            txid: Txid(0),
            page_size: ps,
            change_counter: 0,
        };
        if bytes.len() < 100 {
            inner.write_state()?;
            return Ok(None);
        }
        inner.write_whole(seq, &bytes).map(Some)
    }

    /// Forgets the numbering without writing anything: drops every file held
    /// and sets the position back to no TXID, so the next file written or
    /// applied is TXID 1. For a device that joins a replica with no files.
    pub fn forget(&self) -> Result<()> {
        let mut inner = self.lock();
        inner.clear_pack()?;
        inner.position.txid = Txid(0);
        inner.position.change_counter = 0;
        inner.write_state()
    }

    /// Writes the next TXID as a file that holds page 1 unchanged: a fence.
    /// A device that has just taken over writing uploads it at once, so a
    /// late upload from the previous writer finds the TXID taken. Nothing
    /// changes in the database.
    pub fn fence(&self, volume: &Volume) -> Result<Txid> {
        let name = self.lock().file.clone();
        let size = volume.committed_size(&name)?;
        let mut header = [0u8; 100];
        if volume.read_committed(&name, &mut header, 0)? < 100 {
            return Err(Error::invalid(format!(
                "{name} is empty; there is nothing to fence"
            )));
        }
        let ps = page_size_of(&header)?;
        let mut page1 = vec![0u8; ps as usize];
        volume.read_committed(&name, &mut page1, 0)?;
        let mut inner = self.lock();
        let p = inner.position;
        let counter = change_counter(&page1);
        if p.txid.0 == 0 || p.page_size != ps || p.change_counter != counter {
            return Err(Error::invalid(format!(
                "{name} has changes capture has not recorded; cannot fence at TXID {}",
                p.txid
            )));
        }
        let pages = (size / ps as u64) as u32;
        inner.write_pages(p.seq, ps, pages, counter, |enc| {
            enc.write_page(1, &page1).map_err(ltx_error)
        })
    }

    /// Applies `ltx`, a file from the replica, to `volume` as one commit, and
    /// records its last TXID as the position instead of writing a file.
    /// Returns the new position.
    ///
    /// With [`ApplyMode::Follow`] the file must follow on from the position
    /// (it may repeat TXIDs already applied, as a compacted file does). The
    /// connection to the file must not be in a transaction. With
    /// [`ApplyMode::Replace`], close the connection first: the file may hold
    /// the same change counter as the database it replaces, and SQLite would
    /// keep its cache.
    pub fn apply(&self, volume: &Volume, ltx: &[u8], mode: ApplyMode) -> Result<Position> {
        let file = Ltx::decode(ltx).map_err(ltx_error)?;
        let h = *file.header();
        let replace = mode == ApplyMode::Replace || h.is_snapshot();
        let name = {
            let mut inner = self.lock();
            let p = inner.position;
            if mode == ApplyMode::Replace && !h.is_snapshot() {
                return Err(Error::invalid(format!(
                    "replacing the database needs a snapshot, not TXIDs {}-{}",
                    h.min_txid, h.max_txid
                )));
            }
            if !replace {
                if p.txid.0 == 0 || !is_contiguous(p.txid, h.min_txid, h.max_txid) {
                    return Err(Error::invalid(format!(
                        "file {}-{} does not follow on from TXID {}",
                        h.min_txid, h.max_txid, p.txid
                    )));
                }
                if p.page_size != h.page_size {
                    return Err(Error::invalid(format!(
                        "file has {}-byte pages but the database has {}",
                        h.page_size, p.page_size
                    )));
                }
            }
            inner.expect = Some(Expect {
                txid: h.max_txid,
                replace,
            });
            if let Err(e) = inner.write_state() {
                inner.expect = None;
                return Err(e);
            }
            inner.file.clone()
        };

        let mut pages = BTreeMap::new();
        for page in file.iter_pages() {
            let (pgno, data) = page.map_err(ltx_error)?;
            pages.insert(pgno, data.into_boxed_slice());
        }
        let commit = FileCommit {
            name,
            page_size: h.page_size,
            truncated_to: if replace { 0 } else { h.commit },
            size: h.commit,
            pages,
        };
        let applied = volume.apply_pages(commit);
        let mut inner = self.lock();
        if inner.expect.take().is_some() {
            // The commit never reached capture. Recorded as expected only in
            // memory: the next state written clears it, and so does recovery.
            applied?;
            return Err(Error::invalid("the applied commit did not reach capture"));
        }
        applied?;
        Ok(inner.position)
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
    fn write_state(&mut self) -> Result<()> {
        let slot = 1 - self.slot;
        let generation = self.generation + 1;
        self.state_file.write_at(
            &encode_slot(self.position, self.expect, generation),
            (slot * SLOT_LEN) as u64,
        )?;
        self.state_file.flush()?;
        self.slot = slot;
        self.generation = generation;
        Ok(())
    }

    fn clear_pack(&mut self) -> Result<()> {
        self.pack.truncate(0)?;
        self.pack.flush()?;
        self.index.clear();
        self.end = 0;
        Ok(())
    }

    /// Writes the next TXID as a whole copy of `bytes`, the file's content.
    fn write_whole(&mut self, seq: CommitSeq, bytes: &[u8]) -> Result<Txid> {
        let ps = page_size_of(bytes)?;
        let size = (bytes.len() / ps as usize) as u32;
        let page = |pgno: u32| {
            let at = (pgno as usize - 1) * ps as usize;
            &bytes[at..at + ps as usize]
        };
        let counter = change_counter(page(1));
        self.write_file(seq, ps, size, counter, |pgno, buf| {
            buf.copy_from_slice(page(pgno));
            Ok(())
        })
    }

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

        // Files are numbered on from the last one recorded, so a record past
        // `end` is from a crash before its state was written: overwrite it.
        if self
            .index
            .keys()
            .next_back()
            .is_some_and(|&t| t.0 + 1 != txid.0)
        {
            self.clear_pack()?;
        }
        let record = encode_record(txid, &bytes);
        let at = self.end;
        self.pack.write_at(&record, at)?;
        self.pack.truncate(at + record.len() as u64)?;
        self.pack.flush()?;

        self.position = Position {
            seq,
            txid,
            page_size,
            change_counter: counter,
        };
        self.expect = None;
        self.write_state()?;
        self.index
            .insert(txid, (at + RECORD_HEAD as u64, bytes.len() as u32));
        self.end = at + record.len() as u64;
        Ok(txid)
    }

    /// Records the commit [`Capture::apply`] wrote as replica TXID `e.txid`.
    fn adopt(&mut self, seq: CommitSeq, fc: &FileCommit, e: Expect) -> Result<()> {
        let counter = fc
            .pages
            .get(&1)
            .map_or(self.position.change_counter, |p| change_counter(p));
        if e.replace || self.index.keys().next_back().is_some_and(|&t| t > e.txid) {
            self.clear_pack()?;
        }
        self.position = Position {
            seq,
            txid: e.txid,
            page_size: fc.page_size,
            change_counter: counter,
        };
        self.write_state()
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
            Some(fc) => match inner.expect.take() {
                Some(e) => inner.adopt(commit.seq, fc, e),
                None => inner.capture(commit.seq, fc, before),
            },
            None => {
                // Nothing to write. Not recorded durably: after a reopen the
                // commit is either gone from the log or handed over again.
                inner.position.seq = commit.seq;
                Ok(())
            }
        }
    }

    fn recovered(&mut self) -> Result<()> {
        // An expected commit that recovery did not hand over never happened.
        let mut inner = self.lock();
        if inner.expect.take().is_some() {
            inner.write_state()?;
        }
        Ok(())
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("storage", &self.storage_name)
            .finish_non_exhaustive()
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
        for expect in [
            None,
            Some(Expect {
                txid: Txid(12),
                replace: true,
            }),
        ] {
            let b = encode_slot(p, expect, 3);
            assert_eq!(decode_slot(&b), Some((p, expect, 3)));
            for i in 0..SLOT_LEN {
                let mut bad = b;
                bad[i] ^= 1;
                assert_eq!(decode_slot(&bad), None, "byte {i}");
            }
        }
    }

    #[test]
    fn pack_scan_stops_at_damage_and_at_the_state() {
        let storage = crate::MemoryStorage::new();
        let mut pack = storage.open(PACK_NAME).unwrap();
        let mut at = 0;
        for t in 1..=3u64 {
            let r = encode_record(Txid(t), &vec![t as u8; 10 * t as usize]);
            pack.write_at(&r, at).unwrap();
            at += r.len() as u64;
        }
        let (index, end) = scan_pack(pack.as_mut(), Txid(3)).unwrap();
        assert_eq!(index.len(), 3);
        assert_eq!(end, at);
        let (index, _) = scan_pack(pack.as_mut(), Txid(2)).unwrap();
        assert_eq!(
            index.keys().copied().collect::<Vec<_>>(),
            [Txid(1), Txid(2)]
        );
        pack.truncate(at - 1).unwrap();
        let (index, _) = scan_pack(pack.as_mut(), Txid(9)).unwrap();
        assert_eq!(index.len(), 2, "a torn last record is not counted");
    }
}
