//! A volume: the database files of one Moul database, their commit log, and
//! the commit path between SQLite and storage.
//!
//! # Layers
//!
//! SQLite never writes a database file directly. Each file the volume serves
//! is a stack, read from the top down:
//!
//! | Layer     | Holds                                                         |
//! | --------- | ------------------------------------------------------------- |
//! | `tx`      | Pages written by the SQLite transaction in progress            |
//! | `staged`  | Transactions SQLite finished while another file still writes   |
//! | `pending` | Commits durable in the log but not yet copied into the file    |
//! | base      | The file in storage                                           |
//!
//! A write transaction starts when SQLite takes a `RESERVED` lock on a file and
//! ends when it drops below it. SQLite rolls a transaction back by writing the
//! original pages again, so at the end the volume compares the file change
//! counter in page 1 with the one underneath: changed means committed, and the
//! pages move to `staged`; unchanged means rolled back, and they are dropped.
//!
//! When no file holds a write lock any more, everything staged becomes one
//! commit. That is how a transaction that spans `main.db` and `writer.db` (one
//! connection with the other stores attached) commits atomically, although
//! SQLite itself only offers that with a journal file per store.
//!
//! # Commit
//!
//! 1. Append one record with every page and the new sizes to `commit.log`,
//!    and flush. **This is the commit point.**
//! 2. Hand the commit to the observer, which packs it into LTX (see
//!    [`Capture`](crate::Capture)). If the observer fails, the volume stops
//!    here and keeps the record, so the next open hands it over again.
//! 3. Copy the pages into the database files and flush them.
//! 4. Record the commit as applied in the log header, then cut the log back to
//!    its header.
//!
//! On open, every record newer than the applied number is copied in again.
//! Copying is idempotent when done in order, so a crash anywhere is repaired.
//!
//! # Stepping back
//!
//! A device that follows a hub must later remove commits the hub did not
//! confirm. The pieces for that are here but not the feature: commit numbers
//! are local ([`CommitSeq`]) and not replica TXIDs, and a commit sits in its own
//! layer above the base until it is copied in. Phase 9 adds an `unconfirmed`
//! layer between `pending` and base that is copied in only on confirmation, so
//! stepping back is dropping a layer and nothing on disk has to be undone.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::log::{self, Commit, CommitSeq, FileCommit, HEADER_LEN, Header};
use crate::storage::{MaybeSend, Storage, StorageFile, check_name};
use crate::{Error, ErrorKind, Result};

/// The name of the commit log inside a volume.
pub const LOG_NAME: &str = "commit.log";

/// SQLite lock levels, in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Lock {
    #[default]
    None,
    Shared,
    Reserved,
    Pending,
    Exclusive,
}

/// Which layers a read sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// Everything, as SQLite sees the file.
    All,
    /// Everything but the transaction in progress.
    BelowTx,
    /// Only what is committed: `pending` and base. This is what export reads.
    Committed,
}

/// Pages over a file, with the size they leave it at.
#[derive(Debug)]
struct Overlay {
    page_size: u32,
    /// Size in bytes after this layer.
    size: u64,
    /// The smallest size in bytes this layer cut the file to. Pages past it
    /// that the layer does not hold read as zeros, whatever lies beneath.
    min_size: u64,
    pages: BTreeMap<u32, Box<[u8]>>,
}

impl Overlay {
    fn into_commit(self, name: &str) -> FileCommit {
        let ps = self.page_size as u64;
        FileCommit {
            name: name.to_owned(),
            page_size: self.page_size,
            truncated_to: (self.min_size / ps) as u32,
            size: (self.size / ps) as u32,
            pages: self.pages,
        }
    }

    fn from_commit(c: FileCommit) -> Self {
        let ps = c.page_size as u64;
        Self {
            page_size: c.page_size,
            size: c.size as u64 * ps,
            min_size: c.truncated_to as u64 * ps,
            pages: c.pages,
        }
    }

    /// Puts `upper` on top of `lower`.
    fn merge(lower: Option<Overlay>, upper: Overlay) -> Overlay {
        let Some(mut lower) = lower else {
            return upper;
        };
        if lower.page_size != upper.page_size {
            // Only an import changes the page size, and it replaces the file.
            debug_assert_eq!(upper.min_size, 0);
            return upper;
        }
        let ps = upper.page_size as u64;
        lower.pages.retain(|&p, _| p as u64 * ps <= upper.min_size);
        lower.min_size = lower.min_size.min(upper.min_size);
        lower.size = upper.size;
        lower.pages.extend(upper.pages);
        lower
    }
}

/// One database file served to SQLite.
struct PageFile {
    file: Box<dyn StorageFile>,
    base_size: u64,
    page_size: Option<u32>,
    tx: Option<Overlay>,
    staged: Option<Overlay>,
    pending: Option<Overlay>,
    lock: Lock,
    open: bool,
}

impl PageFile {
    fn load(storage: &dyn Storage, name: &str) -> Result<Self> {
        let mut file = storage.open(name)?;
        let base_size = file.size()?;
        let mut header = [0u8; 100];
        let n = file.read_at(&mut header, 0)?;
        let page_size = (n == 100).then(|| header_page_size(&header)).flatten();
        Ok(Self {
            file,
            base_size,
            page_size,
            tx: None,
            staged: None,
            pending: None,
            lock: Lock::None,
            open: false,
        })
    }

    fn layers(&self, view: View) -> impl Iterator<Item = &Overlay> {
        let (tx, staged) = match view {
            View::All => (self.tx.as_ref(), self.staged.as_ref()),
            View::BelowTx => (None, self.staged.as_ref()),
            View::Committed => (None, None),
        };
        tx.into_iter().chain(staged).chain(self.pending.as_ref())
    }

    fn size(&self, view: View) -> u64 {
        self.layers(view).next().map_or(self.base_size, |l| l.size)
    }

    fn read(&mut self, view: View, buf: &mut [u8], offset: u64) -> Result<usize> {
        let size = self.size(view);
        if offset >= size {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(size - offset) as usize;
        let Some(ps) = self.page_size.map(u64::from) else {
            // No page size means no layers: the base is all there is.
            let got = self.file.read_at(&mut buf[..n], offset)?;
            buf[got..n].fill(0);
            return Ok(n);
        };
        let mut done = 0;
        while done < n {
            let pos = offset + done as u64;
            let within = (pos % ps) as usize;
            let chunk = (ps as usize - within).min(n - done);
            let pgno = (pos / ps + 1) as u32;
            let page_start = pos - within as u64;
            let out = &mut buf[done..done + chunk];

            enum Source<'a> {
                Page(&'a [u8]),
                Zeros,
                Base,
            }
            let mut source = Source::Base;
            for layer in self.layers(view) {
                if let Some(page) = layer.pages.get(&pgno) {
                    source = Source::Page(page);
                    break;
                }
                if page_start >= layer.min_size {
                    source = Source::Zeros;
                    break;
                }
            }
            match source {
                Source::Page(page) => out.copy_from_slice(&page[within..within + chunk]),
                Source::Zeros => out.fill(0),
                Source::Base => {
                    let got = self.file.read_at(out, pos)?;
                    out[got..].fill(0);
                }
            }
            done += chunk;
        }
        Ok(n)
    }

    fn write(&mut self, name: &str, buf: &[u8], offset: u64) -> Result<()> {
        if self.lock < Lock::Reserved {
            return Err(Error::invalid(format!(
                "write to {name} without a write lock"
            )));
        }
        let ps = buf.len() as u32;
        if !valid_page_size(ps) || !offset.is_multiple_of(ps as u64) {
            return Err(Error::invalid(format!(
                "write of {} bytes at offset {offset} in {name} is not one whole page",
                buf.len()
            )));
        }
        let current = self.size(View::All);
        match self.page_size {
            Some(p) if p == ps => {}
            Some(p) if current > 0 => {
                return Err(Error::invalid(format!(
                    "{name} has {p}-byte pages; changing the page size of a database \
                     with content is not supported (got a {ps}-byte write)"
                )));
            }
            _ => self.page_size = Some(ps),
        }
        let tx = self.tx.get_or_insert_with(|| Overlay {
            page_size: ps,
            size: current,
            min_size: current,
            pages: BTreeMap::new(),
        });
        tx.pages.insert((offset / ps as u64 + 1) as u32, buf.into());
        tx.size = tx.size.max(offset + ps as u64);
        Ok(())
    }

    fn truncate(&mut self, name: &str, size: u64) -> Result<()> {
        let current = self.size(View::All);
        if size == current {
            return Ok(());
        }
        if self.lock < Lock::Reserved {
            return Err(Error::invalid(format!(
                "truncate of {name} without a write lock"
            )));
        }
        let Some(ps) = self.page_size else {
            return Err(Error::invalid(format!(
                "truncate of {name} before its page size is known"
            )));
        };
        if !size.is_multiple_of(ps as u64) {
            return Err(Error::invalid(format!(
                "truncate of {name} to {size} bytes is not a whole number of pages"
            )));
        }
        let tx = self.tx.get_or_insert_with(|| Overlay {
            page_size: ps,
            size: current,
            min_size: current,
            pages: BTreeMap::new(),
        });
        tx.size = size;
        tx.min_size = tx.min_size.min(size);
        tx.pages.retain(|&p, _| p as u64 * ps as u64 <= size);
        Ok(())
    }

    /// Ends SQLite's write transaction on this file. Returns whether it
    /// committed. A rolled-back transaction is dropped here.
    fn end_tx(&mut self) -> Result<bool> {
        let Some(tx) = self.tx.take() else {
            return Ok(false);
        };
        let Some(page1) = tx.pages.get(&1) else {
            return Ok(false);
        };
        let mut below = [0u8; 100];
        let n = self.read(View::BelowTx, &mut below, 0)?;
        let committed = n < 100 || page1[24..28] != below[24..28];
        if committed {
            self.staged = Some(Overlay::merge(self.staged.take(), tx));
        }
        Ok(committed)
    }

    /// Copies `layer` into the base file and flushes it.
    fn apply(&mut self, layer: &Overlay) -> Result<()> {
        let ps = layer.page_size as u64;
        if layer.min_size < self.base_size {
            self.file.truncate(layer.min_size)?;
        }
        for (&pgno, page) in &layer.pages {
            self.file.write_at(page, (pgno as u64 - 1) * ps)?;
        }
        self.file.truncate(layer.size)?;
        self.file.flush()?;
        self.base_size = layer.size;
        self.page_size = Some(layer.page_size);
        Ok(())
    }

    fn apply_pending(&mut self) -> Result<()> {
        if let Some(pending) = self.pending.take()
            && let Err(e) = self.apply(&pending)
        {
            self.pending = Some(pending);
            return Err(e);
        }
        Ok(())
    }
}

fn valid_page_size(ps: u32) -> bool {
    (512..=65536).contains(&ps) && ps.is_power_of_two()
}

/// The page size recorded in a SQLite header, if the header is one.
fn header_page_size(header: &[u8]) -> Option<u32> {
    if !header.starts_with(b"SQLite format 3\0") {
        return None;
    }
    let raw = u16::from_be_bytes([header[16], header[17]]) as u32;
    let ps = if raw == 1 { 65536 } else { raw };
    valid_page_size(ps).then_some(ps)
}

struct LogFile {
    file: Box<dyn StorageFile>,
    header: Header,
    slot: usize,
    end: u64,
}

/// Receives every commit once it is durable, before its pages are copied into
/// the database files.
///
/// It runs while the volume is locked, so it must not call back into the
/// volume or into SQLite; [`Before`] gives it the files as they were before
/// the commit. An error stops the volume: the commit stays durable in the
/// log, every later call fails, and the next [`Volume::open_observed`] hands
/// the same commit to its observer again. So an observer sees every commit at
/// least once, in order, and must ignore one it has already handled.
pub trait Observer: MaybeSend + 'static {
    /// Handles one commit.
    fn commit(&mut self, commit: &Commit, before: &mut Before<'_>) -> Result<()>;

    /// Called once when [`Volume::open_observed`] has finished recovery:
    /// every commit that was durable before the open has been handed over.
    fn recovered(&mut self) -> Result<()> {
        Ok(())
    }
}

impl<F> Observer for F
where
    F: FnMut(&Commit, &mut Before<'_>) -> Result<()> + MaybeSend + 'static,
{
    fn commit(&mut self, commit: &Commit, before: &mut Before<'_>) -> Result<()> {
        self(commit, before)
    }
}

/// Hands every commit to several observers, in the order they were added:
/// how a volume has both its shared store and its writer-only store
/// captured, each by its own [`Capture`](crate::Capture).
///
/// The first observer that fails stops the commit there, and its error is
/// the volume's, exactly as with one observer: the volume takes no more
/// commits, and the next [`Volume::open_observed`] hands the commit to every
/// observer again. Those that had already handled it ignore it, as every
/// observer must.
///
/// ```
/// # use rusqlite as _; // links SQLite, which the VFS glue needs
/// use moul_vfs::{Capture, Fanout, MemoryStorage, Volume};
///
/// let shared = Capture::open(MemoryStorage::new(), "main.db").unwrap();
/// let writer_only = Capture::open(MemoryStorage::new(), "writer.db").unwrap();
/// let observers = Fanout::new().with(shared.clone()).with(writer_only.clone());
/// let volume = Volume::open_observed(MemoryStorage::new(), observers).unwrap();
/// ```
#[derive(Default)]
pub struct Fanout {
    observers: Vec<Box<dyn Observer>>,
}

impl std::fmt::Debug for Fanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fanout")
            .field("observers", &self.observers.len())
            .finish()
    }
}

impl Fanout {
    /// No observers yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `observer`, which is handed each commit after those added
    /// before it.
    pub fn with(mut self, observer: impl Observer) -> Self {
        self.observers.push(Box::new(observer));
        self
    }
}

impl Observer for Fanout {
    fn commit(&mut self, commit: &Commit, before: &mut Before<'_>) -> Result<()> {
        for observer in &mut self.observers {
            observer.commit(commit, before)?;
        }
        Ok(())
    }

    fn recovered(&mut self) -> Result<()> {
        for observer in &mut self.observers {
            observer.recovered()?;
        }
        Ok(())
    }
}

/// The committed content of the volume's files as it was just before the
/// commit an [`Observer`] is handling.
pub struct Before<'a> {
    state: &'a mut State,
}

impl std::fmt::Debug for Before<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Before").finish_non_exhaustive()
    }
}

impl Before<'_> {
    /// The size of file `name` in bytes. A file that does not exist is empty.
    pub fn size(&mut self, name: &str) -> Result<u64> {
        check_name(name)?;
        Ok(self.state.page_file(name)?.size(View::Committed))
    }

    /// Reads file `name` at `offset` and returns how many bytes were read.
    /// Fewer than asked means the file ended.
    pub fn read(&mut self, name: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        check_name(name)?;
        self.state
            .page_file(name)?
            .read(View::Committed, buf, offset)
    }
}

/// Holds the observer. On native targets it must be `Send`, like the rest of
/// the volume; the bound lives here so the trait object stays simple.
struct ObserverSlot(Option<Box<dyn Observer>>);
// SAFETY: `Observer` requires `MaybeSend`, which is `Send` on native targets,
// and the slot is only reached through the state mutex.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for ObserverSlot {}

pub(crate) struct State {
    storage: Box<dyn Storage>,
    log: LogFile,
    files: BTreeMap<String, PageFile>,
    pub(crate) temps: BTreeMap<String, Arc<Mutex<Vec<u8>>>>,
    /// Files holding a write lock.
    writers: usize,
    /// A file's transaction could not be classified, so the whole group of
    /// transactions now in flight must be dropped when it ends.
    group_failed: Option<Error>,
    next_seq: u64,
    unapplied: Option<Error>,
    poisoned: Option<String>,
    observer: ObserverSlot,
    pub(crate) open_handles: usize,
}

impl State {
    fn recover(storage: Box<dyn Storage>, observer: Option<Box<dyn Observer>>) -> Result<Self> {
        let mut log_file = storage.open(LOG_NAME)?;
        let len = log_file.size()?;
        let (header, slot) = match log::read_header(log_file.as_mut())? {
            Some(found) => found,
            None if len <= HEADER_LEN => {
                let h = Header {
                    generation: 1,
                    applied: 0,
                };
                log_file.truncate(0)?;
                log::write_header(log_file.as_mut(), 0, h)?;
                (h, 0)
            }
            None => {
                return Err(Error::corrupt(format!(
                    "{LOG_NAME} in {} has records but no valid header",
                    storage.describe()
                )));
            }
        };

        let mut state = State {
            storage,
            log: LogFile {
                file: log_file,
                header,
                slot,
                end: HEADER_LEN,
            },
            files: BTreeMap::new(),
            temps: BTreeMap::new(),
            writers: 0,
            group_failed: None,
            next_seq: header.applied + 1,
            unapplied: None,
            poisoned: None,
            observer: ObserverSlot(observer),
            open_handles: 0,
        };

        // Copy in every complete record newer than the applied number, in order.
        let len = state.log.file.size()?;
        let mut offset = HEADER_LEN;
        let mut last = header.applied;
        let mut replayed = false;
        let mut previous: Option<u64> = None;
        while let Some((commit, n)) = log::read_record(state.log.file.as_mut(), offset, len)? {
            if previous.is_some_and(|p| commit.seq.0 != p + 1) {
                return Err(Error::corrupt(format!(
                    "{LOG_NAME} jumps from commit {} to {}",
                    previous.unwrap_or_default(),
                    commit.seq
                )));
            }
            previous = Some(commit.seq.0);
            offset += n;
            if commit.seq.0 <= header.applied {
                continue;
            }
            // The files hold everything before this record, so the observer
            // sees the same `Before` it saw, or would have seen, live.
            state.observe(&commit)?;
            for fc in commit.files {
                let name = fc.name.clone();
                check_name(&name)?;
                let pf = state.page_file(&name)?;
                pf.apply(&Overlay::from_commit(fc))?;
            }
            last = commit.seq.0;
            replayed = true;
        }
        state.next_seq = last + 1;
        state.log.end = offset;
        if let Some(observer) = state.observer.0.as_mut() {
            observer.recovered()?;
        }
        if replayed {
            state.mark_applied()?;
        } else if len > HEADER_LEN {
            // Applied records, or the torn tail of one that never committed.
            state.log.file.truncate(HEADER_LEN)?;
            state.log.file.flush()?;
            state.log.end = HEADER_LEN;
        }
        Ok(state)
    }

    fn page_file(&mut self, name: &str) -> Result<&mut PageFile> {
        if !self.files.contains_key(name) {
            let pf = PageFile::load(self.storage.as_ref(), name)?;
            self.files.insert(name.to_owned(), pf);
        }
        Ok(self.files.get_mut(name).expect("inserted above"))
    }

    fn loaded(&mut self, name: &str) -> Result<&mut PageFile> {
        self.files
            .get_mut(name)
            .ok_or_else(|| Error::invalid(format!("{name} is not open")))
    }

    fn check_poisoned(&self) -> Result<()> {
        match &self.poisoned {
            Some(why) => Err(Error::new(ErrorKind::Poisoned, why.clone())),
            None => Ok(()),
        }
    }

    /// Records that every commit so far is in the database files, then cuts
    /// the log back to its header.
    fn mark_applied(&mut self) -> Result<()> {
        let h = Header {
            generation: self.log.header.generation + 1,
            applied: self.next_seq - 1,
        };
        let slot = 1 - self.log.slot;
        log::write_header(self.log.file.as_mut(), slot, h)?;
        self.log.header = h;
        self.log.slot = slot;
        let cut = self
            .log
            .file
            .truncate(HEADER_LEN)
            .and_then(|()| self.log.file.flush());
        if let Err(e) = cut {
            // The log's length is now unknown, so appending is not safe.
            self.poisoned = Some(format!("cannot cut {LOG_NAME} back: {e}"));
            return Err(e);
        }
        self.log.end = HEADER_LEN;
        Ok(())
    }

    /// Copies every pending layer into its file. Leaves the error in
    /// `unapplied` on failure, with the layers still in place for reads.
    fn try_apply(&mut self) {
        for pf in self.files.values_mut() {
            if let Err(e) = pf.apply_pending() {
                self.unapplied = Some(e);
                return;
            }
        }
        self.unapplied = None;
        if let Err(e) = self.mark_applied() {
            // The pages are in the files; only the log is behind, and replaying
            // it on the next open is harmless.
            self.unapplied = Some(e);
        }
    }

    fn discard_staged(&mut self) {
        for pf in self.files.values_mut() {
            pf.staged = None;
        }
    }

    /// Hands `commit` to the observer, if there is one.
    fn observe(&mut self, commit: &Commit) -> Result<()> {
        let Some(mut observer) = self.observer.0.take() else {
            return Ok(());
        };
        let result = observer.commit(commit, &mut Before { state: self });
        self.observer.0 = Some(observer);
        result
    }

    /// Turns everything staged into one durable commit.
    fn commit_staged(&mut self) -> Result<Option<CommitSeq>> {
        if let Some(e) = self.group_failed.take() {
            self.discard_staged();
            return Err(e);
        }
        if !self.files.values().any(|pf| pf.staged.is_some()) {
            return Ok(None);
        }
        if let Err(e) = self.check_poisoned() {
            self.discard_staged();
            return Err(e);
        }
        if self.unapplied.is_some() {
            self.try_apply();
        }

        let seq = CommitSeq(self.next_seq);
        let files: Vec<FileCommit> = self
            .files
            .iter_mut()
            .filter_map(|(name, pf)| pf.staged.take().map(|s| s.into_commit(name)))
            .collect();
        let record = log::encode_record(seq, &files)?;

        let end = self.log.end;
        let appended = self
            .log
            .file
            .write_at(&record, end)
            .and_then(|()| self.log.file.flush());
        if let Err(e) = appended {
            // Not committed. Take the partial record back out so the next
            // commit, which reuses this number, is not shadowed by it.
            let undo = self
                .log
                .file
                .truncate(end)
                .and_then(|()| self.log.file.flush());
            if let Err(undo) = undo {
                self.poisoned = Some(format!(
                    "a commit failed ({e}) and the log could not be repaired ({undo})"
                ));
            }
            return Err(e);
        }
        self.log.end = end + record.len() as u64;
        self.next_seq += 1;

        let commit = Commit { seq, files };
        let observed = self.observe(&commit);
        for fc in commit.files {
            let pf = self
                .files
                .get_mut(&fc.name)
                .expect("staged files are loaded");
            pf.pending = Some(Overlay::merge(pf.pending.take(), Overlay::from_commit(fc)));
        }
        if let Err(e) = observed {
            // The commit is durable, but whatever the observer keeps (the
            // replica) is behind. Keep the record in the log, so the next open
            // hands it over again, and take no more commits until then.
            self.poisoned = Some(format!(
                "the commit observer failed on commit {seq}: {e}; reopen the volume"
            ));
            return Err(e);
        }
        self.try_apply();
        Ok(Some(seq))
    }

    // ---- Called by the SQLite glue -------------------------------------------

    pub(crate) fn open_page(&mut self, name: &str) -> Result<()> {
        self.check_poisoned()?;
        check_name(name)?;
        if name == LOG_NAME {
            return Err(Error::invalid(format!("{LOG_NAME} is reserved")));
        }
        let pf = self.page_file(name)?;
        if pf.open {
            return Err(Error::new(
                ErrorKind::Busy,
                format!("{name} is already open; a volume serves one connection"),
            ));
        }
        pf.open = true;
        Ok(())
    }

    pub(crate) fn close_page(&mut self, name: &str) -> Result<()> {
        let result = self.unlock(name, Lock::None);
        if let Some(pf) = self.files.get_mut(name) {
            pf.open = false;
        }
        result
    }

    pub(crate) fn read(&mut self, name: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let n = self.loaded(name)?.read(View::All, buf, offset)?;
        // A file written in WAL mode (as Litestream's are) says so in bytes
        // 18 and 19 of its header. This layer runs SQLite with a rollback
        // journal, so show SQLite such a file as a rollback-journal file. The
        // stored bytes are left alone: a follower's file must stay byte for
        // byte the writer's. SQLite writes the header back as it sees it.
        if offset <= 18 && offset + n as u64 >= 20 {
            let at = (18 - offset) as usize;
            if buf[at] == 2 && buf[at + 1] == 2 {
                buf[at] = 1;
                buf[at + 1] = 1;
            }
        }
        Ok(n)
    }

    pub(crate) fn write(&mut self, name: &str, buf: &[u8], offset: u64) -> Result<()> {
        self.check_poisoned()?;
        self.loaded(name)?.write(name, buf, offset)
    }

    pub(crate) fn truncate(&mut self, name: &str, size: u64) -> Result<()> {
        self.check_poisoned()?;
        self.loaded(name)?.truncate(name, size)
    }

    pub(crate) fn size(&mut self, name: &str) -> Result<u64> {
        Ok(self.loaded(name)?.size(View::All))
    }

    pub(crate) fn exists(&self, name: &str) -> bool {
        self.temps.contains_key(name)
            || self
                .files
                .get(name)
                .is_some_and(|pf| pf.size(View::All) > 0)
    }

    pub(crate) fn lock(&mut self, name: &str, level: Lock) -> Result<()> {
        let pf = self.loaded(name)?;
        let was = pf.lock;
        if level <= was {
            return Ok(());
        }
        if level >= Lock::Reserved && was < Lock::Reserved {
            self.check_poisoned()?;
            self.writers += 1;
        }
        self.loaded(name)?.lock = level;
        Ok(())
    }

    pub(crate) fn unlock(&mut self, name: &str, level: Lock) -> Result<()> {
        let Some(pf) = self.files.get_mut(name) else {
            return Ok(());
        };
        let was = pf.lock;
        if level >= was {
            return Ok(());
        }
        pf.lock = level;
        if !(was >= Lock::Reserved && level < Lock::Reserved) {
            return Ok(());
        }
        let ended = pf.end_tx();
        self.writers -= 1;
        if let Err(e) = ended {
            self.group_failed.get_or_insert(e);
        }
        if self.writers == 0 {
            self.commit_staged().map(|_| ())
        } else {
            Ok(())
        }
    }
}

/// The part of a volume that SQLite's callbacks reach.
pub(crate) struct Shared {
    state: Mutex<State>,
}

impl Shared {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The database files of one Moul database and the commit log that keeps them
/// consistent, served to SQLite as a VFS.
///
/// Open a volume, then open SQLite connections with
/// [`vfs_name`](Self::vfs_name) as the VFS. A volume serves one connection:
/// open `main.db` and attach the other stores to it. Close every connection
/// before dropping the volume.
pub struct Volume {
    shared: Arc<Shared>,
    vfs: crate::sqlite::Registration,
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("vfs", &self.vfs.name())
            .finish_non_exhaustive()
    }
}

impl Volume {
    /// Opens the volume in `storage`, repairs it after a crash, and registers
    /// it with SQLite under a fresh VFS name.
    pub fn open(storage: impl Storage + 'static) -> Result<Self> {
        Self::open_with(Box::new(storage), None)
    }

    /// Opens the volume like [`open`](Self::open), with `observer` attached
    /// from the start. Commits that recovery copies in are handed to it too,
    /// so it misses none. Fails if the observer fails on one of them.
    pub fn open_observed(storage: impl Storage + 'static, observer: impl Observer) -> Result<Self> {
        Self::open_with(Box::new(storage), Some(Box::new(observer)))
    }

    fn open_with(storage: Box<dyn Storage>, observer: Option<Box<dyn Observer>>) -> Result<Self> {
        let state = State::recover(storage, observer)?;
        // In the browser there is one thread and storage handles are not
        // `Send`; the `Arc` is shared with SQLite's callbacks, not threads.
        #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
        });
        let vfs = crate::sqlite::Registration::register(shared.clone())?;
        Ok(Self { shared, vfs })
    }

    /// The VFS name to open SQLite connections with.
    pub fn vfs_name(&self) -> &str {
        self.vfs.name()
    }

    /// The last durable commit, or `CommitSeq(0)` before the first.
    pub fn last_commit(&self) -> CommitSeq {
        CommitSeq(self.shared.state().next_seq - 1)
    }

    /// Replaces the [`Observer`]. Commits that recovery copied in before this
    /// call are not handed to it; use [`open_observed`](Self::open_observed)
    /// for an observer that must see every commit.
    pub fn set_observer(&self, observer: impl Observer) {
        self.shared.state().observer = ObserverSlot(Some(Box::new(observer)));
    }

    /// The committed content of file `name`, with the commit it is current
    /// as of. Unlike calling [`last_commit`](Self::last_commit) and
    /// [`export`](Self::export) one after the other, no commit can land
    /// between the two.
    pub fn snapshot(&self, name: &str) -> Result<(CommitSeq, Vec<u8>)> {
        check_name(name)?;
        let mut state = self.shared.state();
        let seq = CommitSeq(state.next_seq - 1);
        let pf = state.page_file(name)?;
        let size = pf.size(View::Committed);
        let mut out = vec![0u8; size as usize];
        let n = pf.read(View::Committed, &mut out, 0)?;
        out.truncate(n);
        Ok((seq, out))
    }

    /// The committed content of database file `name`: a plain SQLite file any
    /// tool can open. A transaction still in progress is not included.
    pub fn export(&self, name: &str) -> Result<Vec<u8>> {
        Ok(self.snapshot(name)?.1)
    }

    /// Replaces database file `name` with `bytes`, a SQLite database file, as
    /// one commit. Either all of it lands or none of it does.
    ///
    /// The file must not be in a transaction. If its page size changes, close
    /// its connection first; otherwise a connection that is open sees the new
    /// content at its next read, because the change counter is bumped.
    ///
    /// This checks the SQLite header only. Check integrity before calling.
    pub fn import(&self, name: &str, bytes: &[u8]) -> Result<CommitSeq> {
        check_name(name)?;
        let ps = header_page_size(bytes)
            .ok_or_else(|| Error::invalid("not a SQLite database file: bad header or page size"))?;
        if bytes.is_empty() || !(bytes.len() as u64).is_multiple_of(ps as u64) {
            return Err(Error::invalid(format!(
                "a database with {ps}-byte pages cannot be {} bytes long",
                bytes.len()
            )));
        }
        let mut state = self.shared.state();
        state.check_poisoned()?;
        if state.writers > 0 {
            return Err(Error::new(
                ErrorKind::Busy,
                "a write transaction is in progress",
            ));
        }
        let pf = state.page_file(name)?;
        if pf.lock > Lock::None {
            return Err(Error::new(
                ErrorKind::Busy,
                format!("{name} is in a transaction; finish it before importing"),
            ));
        }
        if pf.open && pf.page_size.is_some_and(|p| p != ps) {
            return Err(Error::new(
                ErrorKind::Busy,
                format!("close the connection to {name} before importing a different page size"),
            ));
        }

        // Bump the change counter past the current one, so an open connection
        // drops its cache, and keep the in-header size valid.
        let mut current = [0u8; 100];
        let n = pf.read(View::All, &mut current, 0)?;
        let mut page1 = bytes[..ps as usize].to_vec();
        let old = if n == 100 {
            u32::from_be_bytes(current[24..28].try_into().expect("4 bytes"))
        } else {
            0
        };
        let new = u32::from_be_bytes(page1[24..28].try_into().expect("4 bytes"))
            .max(old)
            .wrapping_add(1);
        page1[24..28].copy_from_slice(&new.to_be_bytes());
        page1[92..96].copy_from_slice(&new.to_be_bytes());
        // A file exported from WAL mode says so in its header; this layer only
        // runs rollback-journal modes.
        page1[18] = 1;
        page1[19] = 1;

        let mut pages = BTreeMap::new();
        pages.insert(1, page1.into_boxed_slice());
        for (i, page) in bytes.chunks(ps as usize).enumerate().skip(1) {
            pages.insert(i as u32 + 1, page.into());
        }
        pf.staged = Some(Overlay {
            page_size: ps,
            size: bytes.len() as u64,
            min_size: 0,
            pages,
        });
        pf.page_size = Some(ps);
        let seq = state.commit_staged()?;
        Ok(seq.expect("an import stages a page"))
    }

    /// Writes `commit`'s pages into its file as one commit, exactly as given:
    /// how a follower applies a file from the replica, so that its database
    /// stays byte for byte the writer's. Pages past
    /// [`truncated_to`](FileCommit::truncated_to) that `commit` does not hold
    /// read as zeros afterwards.
    ///
    /// The file ends up `size` pages long. Pass `truncated_to` equal to `size`
    /// to keep the pages the commit does not hold, or 0 to replace the whole
    /// file; the volume never keeps a page past the file's current end.
    ///
    /// Unlike [`import`](Self::import), page 1 is not touched. An open
    /// connection sees the new content at its next read only if the change
    /// counter in page 1 differs from the one it read; when that is not
    /// certain (replacing the whole file), close the connection first.
    pub fn apply_pages(&self, commit: FileCommit) -> Result<CommitSeq> {
        check_name(&commit.name)?;
        let ps = commit.page_size;
        if !moul_ltx::is_valid_page_size(ps) {
            return Err(Error::invalid(format!("{ps} is not a valid page size")));
        }
        if commit.size == 0 {
            return Err(Error::invalid(format!(
                "a commit cannot leave {} empty",
                commit.name
            )));
        }
        if let Some((&last, _)) = commit.pages.last_key_value()
            && last > commit.size
        {
            return Err(Error::invalid(format!(
                "page {last} is past the end of a {}-page file",
                commit.size
            )));
        }
        if commit.pages.values().any(|p| p.len() != ps as usize) {
            return Err(Error::invalid(format!("every page must be {ps} bytes")));
        }
        let mut state = self.shared.state();
        state.check_poisoned()?;
        if state.writers > 0 {
            return Err(Error::new(
                ErrorKind::Busy,
                "a write transaction is in progress",
            ));
        }
        let name = commit.name.clone();
        let pf = state.page_file(&name)?;
        if pf.lock > Lock::None {
            return Err(Error::new(
                ErrorKind::Busy,
                format!("{name} is in a transaction; finish it before applying pages"),
            ));
        }
        if pf.open && pf.page_size.is_some_and(|p| p != ps) {
            return Err(Error::new(
                ErrorKind::Busy,
                format!("close the connection to {name} before changing its page size"),
            ));
        }
        let mut overlay = Overlay::from_commit(commit);
        overlay.min_size = overlay.min_size.min(pf.size(View::All));
        pf.staged = Some(overlay);
        pf.page_size = Some(ps);
        let seq = state.commit_staged()?;
        Ok(seq.expect("applying pages stages a page"))
    }

    /// The committed size of file `name` in bytes, as
    /// [`export`](Self::export) sees it.
    pub fn committed_size(&self, name: &str) -> Result<u64> {
        check_name(name)?;
        Ok(self.shared.state().page_file(name)?.size(View::Committed))
    }

    /// Reads the committed content of file `name` at `offset`, as
    /// [`export`](Self::export) sees it, and returns how many bytes were
    /// read. Fewer than asked means the file ended.
    pub fn read_committed(&self, name: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        check_name(name)?;
        self.shared
            .state()
            .page_file(name)?
            .read(View::Committed, buf, offset)
    }

    /// Retries copying durable commits into the database files after an
    /// earlier failure. Reads are correct meanwhile, because they see the
    /// commits in memory; this only shortens what a reopen must replay.
    pub fn checkpoint(&self) -> Result<()> {
        let mut state = self.shared.state();
        state.check_poisoned()?;
        if state.unapplied.is_some() {
            state.try_apply();
        }
        match &state.unapplied {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Whether an earlier failure means the volume must be reopened.
    pub fn is_poisoned(&self) -> bool {
        self.shared.state().poisoned.is_some()
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        let open = self.shared.state().open_handles;
        self.vfs.release(open == 0);
    }
}
