//! The device side of sync: one writer pushes, every other device follows,
//! and the writer role moves only by an explicit hand-off.
//!
//! # Rules
//!
//! - **A TXID is written once.** The writer uploads `ltx/0/<txid>-<txid>.ltx`
//!   with `If-None-Match: *`. A refusal means another device wrote that TXID,
//!   unless the object holds exactly these bytes (an earlier upload whose
//!   answer was lost).
//! - **The writer record moves by compare-and-swap.** `moul/writer.json` is
//!   created with `If-None-Match: *` and replaced with `If-Match`, and each
//!   new writer adds one to its generation.
//! - **A new writer fences.** Right after taking over, it uploads a file that
//!   rewrites page 1 unchanged as the next TXID. The previous writer's next
//!   upload then finds that TXID taken, whatever it believes. A take-over
//!   that stopped before head named the new generation is finished the
//!   next time the device connects, before it writes.
//! - **Head is a hint, guarded too.** `moul/head.json` is replaced with
//!   `If-Match`, so a writer that lost its role cannot move it back.
//!   Followers poll it, then fetch files by name or from a listing. A
//!   writer whose head write failed writes it again before it releases the
//!   files head does not name yet: a restart resumes from head.
//! - **Nothing is discarded silently.** A device that finds its own unpushed
//!   changes are not in the replica keeps a copy of its database through
//!   [`Local::save_conflict`] before taking the replica's state. So does a
//!   device that joins a replica while holding data of its own.
//! - **No write lands while the database is replaced.** The host is told
//!   the device follows ([`Local::role_changed`]) before sync reads what to
//!   keep. It is told the device writes only once a new writer has fenced,
//!   or a writer that connects again knows it still does. A take-over whose
//!   fence is not in yet stays read-only across a restart
//!   ([`DeviceState::unfenced`]).
//! - **A writer pushes only on top of the replica's history.** A writer
//!   that connects again checks that the files it holds up to head are the
//!   replica's; one whose storage was reset numbered its own from 1, and
//!   keeps them as a copy instead. Once it has taken the replica's state it
//!   checks the writer record still names it: a take-over since may have
//!   fenced. A writer missing the file for a TXID it has to push checks the
//!   replica's state there is its own before it pushes on, and keeps a copy
//!   and takes the replica's state if not.

use moul_ltx::{FileInfo, Ltx, Txid};
use moul_vfs::{ApplyMode, Capture};
use serde::Serialize;

use crate::layout::{
    HEAD_KEY, Head, WRITER_KEY, WriterRecord, get_json, list_files, ltx_key, put_json,
};
use crate::local::{ConflictCopy, DeviceState, Local, Role};
use crate::maintain::{Report, Retention, maintain};
use crate::replica::{Condition, Put, Replica};
use crate::restore::{Step, download, next_step, restore_db};
use crate::time::rfc3339;
use crate::{Error, ErrorKind, Result};

/// How a device syncs.
pub struct Options {
    /// A name for people, recorded when this device writes.
    pub label: String,
    /// How often a follower checks for new files, and an idle writer checks
    /// it still writes, in milliseconds.
    pub poll_every: i64,
    /// How often an idle writer checks whether compaction or pruning is
    /// due, in milliseconds.
    pub maintain_every: i64,
    /// Compaction and retention.
    pub retention: Retention,
    /// Never become the writer, not even of an empty replica: `moul pull`.
    pub follow_only: bool,
    /// The time, in milliseconds since the epoch.
    pub clock: Box<dyn Fn() -> i64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            label: String::new(),
            poll_every: 3_000,
            maintain_every: 60_000,
            retention: Retention::default(),
            follow_only: false,
            clock: Box::new(moul_vfs::system_clock),
        }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("label", &self.label)
            .field("poll_every", &self.poll_every)
            .field("follow_only", &self.follow_only)
            .finish_non_exhaustive()
    }
}

/// Where a device stands, for a status screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// The replica, without credentials.
    pub replica: String,
    /// Whether the device has reached the replica since it started.
    pub connected: bool,
    /// The device's role.
    pub role: Role,
    /// This device's ID.
    pub device: String,
    /// The writer, as last read.
    pub writer: Option<WriterRecord>,
    /// The last TXID in the local database.
    pub position: u64,
    /// The last TXID this device knows the replica holds.
    pub remote: u64,
    /// Local TXIDs not yet in the replica.
    pub pending: u64,
    /// When the device last finished a push or a pull.
    pub last_sync_at: Option<i64>,
    /// The last error, cleared by the next success.
    pub last_error: Option<String>,
    /// Conflict copies kept since the device started.
    pub conflicts: Vec<String>,
}

/// Syncs one device's shared store with one replica.
pub struct Sync {
    replica: Box<dyn Replica>,
    options: Options,
    state: DeviceState,
    /// The role the host was last given through [`Local::role_changed`].
    /// It differs from the state's role while the role changes (the host is
    /// made read-only before the database is replaced, and a writer again
    /// only once it is fenced), and after a change that failed half way:
    /// the next call then settles the role again by connecting.
    told: Option<Role>,
    connected: bool,
    writer: Option<WriterRecord>,
    head: Option<Head>,
    head_etag: Option<String>,
    pushed: Txid,
    fence: Option<Txid>,
    last_poll: Option<i64>,
    last_maintain: Option<i64>,
    last_sync_at: Option<i64>,
    last_error: Option<String>,
    conflicts: Vec<String>,
    report: Option<Report>,
}

impl std::fmt::Debug for Sync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sync")
            .field("replica", &self.replica.describe())
            .field("state", &self.state)
            .field("pushed", &self.pushed)
            .finish_non_exhaustive()
    }
}

/// A random-enough ID for a device or a replica: 16 hex digits.
pub fn new_id() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let a = (js_sys::Math::random() * 4_294_967_296.0) as u64;
        let b = (js_sys::Math::random() * 4_294_967_296.0) as u64;
        format!("{:016x}", (a << 32) | b)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use sha2::{Digest, Sha256};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let local = 0u8;
        let seed = format!(
            "{nanos}:{}:{}:{:?}:{:p}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            std::thread::current().id(),
            &local
        );
        let d = Sha256::digest(seed.as_bytes());
        d[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The newest TXID in a listing.
fn newest(files: &[FileInfo]) -> Txid {
    files.iter().map(|f| f.max_txid).max().unwrap_or(Txid(0))
}

fn conflict_name(now: i64, from: Txid, to: Txid) -> String {
    let at = rfc3339(now).replace([':', '.'], "-");
    format!("conflict-{at}-txid-{}-{}.db", from.0, to.0)
}

impl Sync {
    /// Syncs through `replica`.
    pub fn new(replica: impl Replica + 'static, options: Options) -> Self {
        Self {
            replica: Box::new(replica),
            options,
            state: DeviceState::default(),
            told: None,
            connected: false,
            writer: None,
            head: None,
            head_etag: None,
            pushed: Txid(0),
            fence: None,
            last_poll: None,
            last_maintain: None,
            last_sync_at: None,
            last_error: None,
            conflicts: Vec::new(),
            report: None,
        }
    }

    /// The replica.
    pub fn replica(&self) -> &dyn Replica {
        self.replica.as_ref()
    }

    /// The device's role.
    pub fn role(&self) -> Role {
        self.state.role
    }

    /// What the last maintenance pass did.
    pub fn last_report(&self) -> Option<&Report> {
        self.report.as_ref()
    }

    /// Where the device stands. `capture` gives the local position.
    pub fn status(&self, capture: &Capture) -> Status {
        let position = capture.position().txid;
        let remote = self
            .head
            .as_ref()
            .map_or(self.pushed.0, |h| h.txid.max(self.pushed.0));
        Status {
            replica: self.replica.describe(),
            connected: self.connected,
            role: self.state.role,
            device: self.state.device.clone(),
            writer: self.writer.clone(),
            position: position.0,
            remote,
            pending: if self.state.role == Role::Writer {
                position.0.saturating_sub(self.pushed.0)
            } else {
                0
            },
            last_sync_at: self.last_sync_at,
            last_error: self.last_error.clone(),
            conflicts: self.conflicts.clone(),
        }
    }

    fn now(&self) -> i64 {
        (self.options.clock)()
    }

    fn me(&self) -> &str {
        &self.state.device
    }

    fn set_role(&mut self, local: &mut dyn Local, role: Role, generation: u64) -> Result<()> {
        self.save_role(local, role, generation)?;
        self.tell(local, role);
        Ok(())
    }

    /// Records `role` in the device state without telling the host. A
    /// device that no longer writes has no take-over to finish.
    fn save_role(&mut self, local: &mut dyn Local, role: Role, generation: u64) -> Result<()> {
        self.state.role = role;
        self.state.generation = generation;
        if role != Role::Writer {
            self.state.unfenced = false;
        }
        local.save_state(&self.state)
    }

    /// Records that the fence is in: the host need not stay read-only after
    /// a restart any more.
    fn fenced(&mut self, local: &mut dyn Local) -> Result<()> {
        if self.state.unfenced {
            self.state.unfenced = false;
            local.save_state(&self.state)?;
        }
        Ok(())
    }

    /// Tells the host the device's role, if that is news to it.
    fn tell(&mut self, local: &mut dyn Local, role: Role) {
        if self.told != Some(role) {
            local.role_changed(role);
            self.told = Some(role);
        }
    }

    /// Whether the device reached the replica and the host has the role the
    /// device state records. Not after a role change that failed half way.
    fn settled(&self) -> bool {
        self.connected && self.told == Some(self.state.role)
    }

    /// Loads the device state, giving the device an ID on first use, and
    /// tells the host the role the device last had, so that a writer can
    /// keep writing while the replica is out of reach. A device whose
    /// take-over has not fenced yet is told it follows: finishing the
    /// take-over may replace the database, and the next call does that
    /// before the device writes.
    pub fn load(&mut self, local: &mut dyn Local) -> Result<DeviceState> {
        let mut state = local.load_state()?.unwrap_or_default();
        if state.device.is_empty() {
            state.device = new_id();
            local.save_state(&state)?;
        }
        self.state = state.clone();
        let role = if state.unfenced {
            Role::Follower
        } else {
            state.role
        };
        local.role_changed(role);
        self.told = Some(role);
        Ok(state)
    }

    /// Runs `f`, recording its error as the last error.
    async fn record<T>(&mut self, result: Result<T>) -> Result<T> {
        match &result {
            Ok(_) => self.last_error = None,
            Err(e) => self.last_error = Some(e.to_string()),
        }
        result
    }

    /// Reaches the replica and settles this device's role: creates the
    /// replica if it is empty, joins it if the device is new to it, or picks
    /// up where the device left off.
    pub async fn connect(&mut self, local: &mut dyn Local) -> Result<()> {
        let result = self.connect_inner(local).await;
        self.record(result).await
    }

    async fn connect_inner(&mut self, local: &mut dyn Local) -> Result<()> {
        if self.state.device.is_empty() {
            self.load(local)?;
        }
        for _ in 0..4 {
            let found = get_json::<WriterRecord>(self.replica.as_ref(), WRITER_KEY).await?;
            match found {
                None => {
                    let files = list_files(self.replica.as_ref()).await?;
                    if files.is_empty() && !self.options.follow_only {
                        if self.create(local).await? {
                            self.connected = true;
                            return Ok(());
                        }
                        continue; // another device created it first
                    }
                    if files.is_empty() {
                        return Err(Error::new(
                            ErrorKind::NotFound,
                            format!("{} holds no database", self.replica.describe()),
                        ));
                    }
                    // LTX files but no writer record: Litestream wrote it.
                    self.writer = None;
                    let id = format!("ext:{}", self.replica.describe());
                    if self.state.replica.as_deref() != Some(id.as_str()) {
                        self.join(local, &id, 0, "joined the replica").await?;
                    } else {
                        self.set_role(local, Role::Follower, 0)?;
                        self.catch_up(local).await?;
                    }
                }
                Some((rec, _)) => {
                    self.writer = Some(rec.clone());
                    if self.state.replica.as_deref() != Some(rec.replica.as_str()) {
                        self.join(local, &rec.replica, rec.generation, "joined the replica")
                            .await?;
                    } else if rec.device == self.state.device {
                        self.resume_writing(local, &rec).await?;
                    } else if self.state.role == Role::Writer
                        || local.capture().first_held().is_some()
                    {
                        // It wrote, as the writer or while disconnected, and
                        // another device writes now.
                        self.lose_role(local).await?;
                    } else {
                        self.set_role(local, Role::Follower, rec.generation)?;
                        self.catch_up(local).await?;
                    }
                }
            }
            self.connected = true;
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::Conflict,
            "the writer record kept changing while connecting; try again",
        ))
    }

    /// Creates the replica with this device as its writer. Returns `false`
    /// if another device created it first.
    async fn create(&mut self, local: &mut dyn Local) -> Result<bool> {
        let rec = WriterRecord {
            device: self.me().to_owned(),
            label: self.options.label.clone(),
            generation: 1,
            since: 0,
            replica: new_id(),
            at: self.now(),
        };
        if put_json(
            self.replica.as_ref(),
            WRITER_KEY,
            &rec,
            Condition::IfNoneMatch,
        )
        .await?
            == Put::Refused
        {
            return Ok(false);
        }
        // Start the numbering at 1 with the whole database as it is.
        local.rebase()?;
        self.state.replica = Some(rec.replica.clone());
        self.writer = Some(rec);
        self.pushed = Txid(0);
        self.head = None;
        self.head_etag = None;
        self.fence = None;
        self.set_role(local, Role::Writer, 1)?;
        self.push(local).await?;
        Ok(true)
    }

    /// This device is the writer of record: carry on pushing, or finish
    /// taking over if that stopped half way.
    async fn resume_writing(&mut self, local: &mut dyn Local, rec: &WriterRecord) -> Result<()> {
        let head = self.refresh_head().await?;
        // Read-only until it is decided: the database may yet be replaced,
        // and a write in between would be lost with it. Not before head is
        // read, so that a writer whose replica is out of reach keeps
        // writing; a write that lands while head is read comes before the
        // position is read below, so it counts. Should anything from here
        // fail, the device stays read-only until the next call settles its
        // role.
        self.tell(local, Role::Follower);
        if let Some(h) = &head
            && h.generation > rec.generation
        {
            // Another device took over since the record was read.
            self.pushed = Txid(rec.since);
            self.state.generation = rec.generation;
            return self.lose_role(local).await;
        }
        let finished = head
            .as_ref()
            .is_some_and(|h| h.writer == rec.device && h.generation == rec.generation);
        if !finished && rec.since > 0 {
            // The record names this device, but head is not of its
            // generation: taking over stopped before the fence was in, and
            // the previous writer may still be writing.
            return self.finish_take_over(local, rec).await;
        }
        let mut pushed = match head {
            Some(h) if finished => Txid(h.txid),
            // No head of this generation, and nothing to finish (the record
            // has `since` 0): this device created the replica and its first
            // head was lost. The replica's files may be past what any head
            // says, pushed by this device before a reset or not; the check
            // below finds which.
            _ => newest(&list_files(self.replica.as_ref()).await?),
        };
        let position = local.capture().position().txid;
        // Behind the replica this device wrote, or holding files up to it
        // that are not the replica's: its storage was reset, and it numbered
        // what it wrote since from 1. Anything written since is kept.
        if position < pushed || !self.holds_replicas_history(local, pushed).await? {
            if local.has_data()? {
                self.keep_copy(
                    local,
                    Txid(1),
                    position,
                    "this device's storage was reset; the replica holds other changes",
                )
                .await?;
            }
            self.resync(local).await?;
            if !self.still_writes(rec.generation).await? {
                // Another device took over since the record was read, and
                // its fence may be in what was just taken: not this
                // device's to count as pushed, nor to write head over.
                self.pushed = local.capture().position().txid;
                self.state.generation = rec.generation;
                return self.lose_role(local).await;
            }
            // The replica may hold files past head: pushed before the reset,
            // their head lost. They are this device's, and are not held now.
            pushed = pushed.max(local.capture().position().txid);
        }
        self.pushed = pushed;
        // Head of its generation is written only once the fence is in.
        self.state.unfenced = false;
        self.set_role(local, Role::Writer, rec.generation)?;
        self.push(local).await
    }

    /// Whether the files the device holds up to `upto` are the replica's.
    /// Files it released or applied are the replica's; a device whose
    /// storage was reset numbers its own from 1 and holds every one, so its
    /// newest held at or before `upto` is not. One the replica no longer
    /// holds at level 0 does not count as the replica's: an idle writer
    /// releases its files before it prunes the replica's.
    async fn holds_replicas_history(&self, local: &mut dyn Local, upto: Txid) -> Result<bool> {
        let capture = local.capture();
        let Some(first) = capture.first_held().filter(|&t| t <= upto) else {
            return Ok(true);
        };
        // The files held run on without a gap from `first`, so `upto` is
        // held unless they stop before it.
        let (txid, mine) = match capture.read(upto) {
            Ok(bytes) => (upto, bytes),
            Err(e) if e.kind() == moul_vfs::ErrorKind::NotFound => (first, capture.read(first)?),
            Err(e) => return Err(e.into()),
        };
        Ok(matches!(
            self.replica.get(&ltx_key(0, txid, txid)).await?,
            Some(obj) if obj.bytes == mine
        ))
    }

    /// Whether the writer record still names this device at `generation`.
    /// A writer that took the replica's state checks this before counting
    /// it as pushed: a device that took over since wrote the record before
    /// its fence.
    async fn still_writes(&mut self, generation: u64) -> Result<bool> {
        let found = get_json::<WriterRecord>(self.replica.as_ref(), WRITER_KEY).await?;
        let Some((rec, _)) = found else {
            return Ok(false);
        };
        let mine = rec.device == self.state.device && rec.generation == generation;
        self.writer = Some(rec);
        Ok(mine)
    }

    /// Finishes taking over as `rec`, the writer record that names this
    /// device. If the replica holds nothing past what the device holds as
    /// the replica's, the device's next files go up as they are: the first
    /// fences. Otherwise the device takes the replica's newest state,
    /// keeping a copy of anything local the replica does not hold, then
    /// fences. Then it writes head, and only then is the host told the
    /// device writes.
    async fn finish_take_over(&mut self, local: &mut dyn Local, rec: &WriterRecord) -> Result<()> {
        self.save_role(local, Role::Writer, rec.generation)?;
        let since = Txid(rec.since);
        let position = local.capture().position().txid;
        // The storage was reset since the take-over, if it holds less than the
        // record's `since` or holds files up to `since` that are not the
        // replica's (a reset device that wrote as far as `since` again).
        let reset = position < since || !self.holds_replicas_history(local, since).await?;
        let unheld = if reset {
            // Anything in it is local.
            local.has_data()?.then_some(Txid(1))
        } else {
            self.first_unheld(local, since).await?
        };
        match unheld {
            Some(from) if !reset && self.upload_next(local, from).await? => {
                // Nothing conflicts: the fence this device wrote, or what
                // it wrote after the fence landed, is in, and fences.
                self.pushed = from;
                self.fenced(local)?;
                self.push(local).await?;
                // A writer now, unless pushing found another had taken over.
                let role = self.state.role;
                self.tell(local, role);
                return Ok(());
            }
            Some(from) => {
                // The replica holds another file at `from`: the previous
                // writer's, landed before the fence. A fence of this
                // device's is no change of its own: one this session wrote,
                // or the last file of a device that has been read-only
                // since it took over.
                let only_fence =
                    from == position && (self.fence == Some(from) || self.state.unfenced);
                if !only_fence {
                    self.keep_copy(
                        local,
                        from,
                        position,
                        "these changes were made while this device took over writing, and the \
                         replica does not hold them",
                    )
                    .await?;
                }
                self.resync(local).await?;
            }
            None if reset => self.resync(local).await?,
            None => {
                self.catch_up(local).await?;
                if self.state.role != Role::Writer {
                    return Ok(()); // another device took over meanwhile
                }
            }
        }
        self.pushed = local.capture().position().txid;
        self.fence_and_write_head(local).await
    }

    /// Uploads the local file for `txid`, the first the replica does not
    /// hold as it is, unless the replica's history already reaches `txid`.
    /// Returns whether the replica now holds it.
    async fn upload_next(&mut self, local: &mut dyn Local, txid: Txid) -> Result<bool> {
        let files = list_files(self.replica.as_ref()).await?;
        if files.iter().any(|f| f.max_txid >= txid) {
            return Ok(false);
        }
        let bytes = local.capture().read(txid)?;
        let key = ltx_key(0, txid, txid);
        Ok(
            match self
                .replica
                .put(&key, &bytes, Condition::IfNoneMatch)
                .await?
            {
                Put::Written { .. } => true,
                Put::Refused => {
                    matches!(self.replica.get(&key).await?, Some(obj) if obj.bytes == bytes)
                }
            },
        )
    }

    /// The first local TXID after `after` whose file the replica does not
    /// hold as it is, if any. Files applied from the replica are skipped.
    async fn first_unheld(&self, local: &mut dyn Local, after: Txid) -> Result<Option<Txid>> {
        let capture = local.capture();
        let position = capture.position().txid;
        for t in after.0 + 1..=position.0 {
            let txid = Txid(t);
            let mine = match capture.read(txid) {
                Ok(b) => b,
                Err(e) if e.kind() == moul_vfs::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            match self.replica.get(&ltx_key(0, txid, txid)).await? {
                Some(obj) if obj.bytes == mine => continue,
                _ => return Ok(Some(txid)),
            }
        }
        Ok(None)
    }

    /// Joins the replica `id`: keeps the local database aside if it holds
    /// anything, then takes the replica's newest state.
    async fn join(
        &mut self,
        local: &mut dyn Local,
        id: &str,
        generation: u64,
        reason: &str,
    ) -> Result<()> {
        // Read-only first: a write after the copy is taken would be in
        // neither the copy nor the replica's state that replaces it. Should
        // joining fail, the device stays read-only until the next call
        // settles its role again.
        self.tell(local, Role::Follower);
        if local.has_data()? {
            let to = local.capture().position().txid;
            self.keep_copy(local, Txid(1), to, reason).await?;
        }
        self.resync(local).await?;
        self.state.replica = Some(id.to_owned());
        self.pushed = local.capture().position().txid;
        self.set_role(local, Role::Follower, generation)?;
        self.last_sync_at = Some(self.now());
        self.catch_up(local).await
    }

    async fn keep_copy(
        &mut self,
        local: &mut dyn Local,
        from: Txid,
        to: Txid,
        reason: &str,
    ) -> Result<()> {
        let now = self.now();
        let copy = ConflictCopy {
            name: conflict_name(now, from, to),
            bytes: local.export()?,
            from,
            to,
            reason: reason.to_owned(),
        };
        let kept = local.save_conflict(copy).await?;
        self.conflicts.push(kept);
        Ok(())
    }

    /// Replaces the local database with the replica's newest state.
    async fn resync(&mut self, local: &mut dyn Local) -> Result<()> {
        let files = list_files(self.replica.as_ref()).await?;
        if files.is_empty() {
            // Nothing to take yet: the next file the writer pushes is TXID 1.
            local.capture().forget()?;
            return Ok(());
        }
        let plan = moul_ltx::restore::plan(&files, None)?;
        for (i, info) in plan.iter().enumerate() {
            let bytes = download(self.replica.as_ref(), info).await?;
            let mode = if i == 0 {
                ApplyMode::Replace
            } else {
                ApplyMode::Follow
            };
            local.apply(&bytes, mode)?;
        }
        self.fence = None;
        Ok(())
    }

    async fn refresh_head(&mut self) -> Result<Option<Head>> {
        match get_json::<Head>(self.replica.as_ref(), HEAD_KEY).await? {
            Some((h, etag)) => {
                self.head = Some(h.clone());
                self.head_etag = Some(etag);
                Ok(Some(h))
            }
            None => {
                self.head = None;
                self.head_etag = None;
                Ok(None)
            }
        }
    }

    /// Whether `head` shows that another device writes now.
    fn superseded_by(&self, head: &Head) -> bool {
        head.generation > self.state.generation
            || (head.generation == self.state.generation && head.writer != self.state.device)
    }

    /// Pulls every file after the local position.
    async fn catch_up(&mut self, local: &mut dyn Local) -> Result<()> {
        let head = self.refresh_head().await?;
        if let Some(h) = &head {
            if self.state.replica.as_deref() != Some(h.replica.as_str())
                && self
                    .state
                    .replica
                    .as_deref()
                    .is_some_and(|r| !r.starts_with("ext:"))
            {
                // The replica was created anew under the same name.
                let (id, generation) = (h.replica.clone(), h.generation);
                return Box::pin(self.join(local, &id, generation, "the replica was replaced"))
                    .await;
            }
            if h.generation > self.state.generation {
                if let Some((rec, _)) =
                    get_json::<WriterRecord>(self.replica.as_ref(), WRITER_KEY).await?
                {
                    self.writer = Some(rec);
                }
                if self.state.role == Role::Writer && self.superseded_by(h) {
                    return Box::pin(self.lose_role(local)).await;
                }
                let generation = h.generation;
                self.set_role(local, self.state.role, generation)?;
            }
        }
        let target = head.map(|h| Txid(h.txid));
        let mut rounds = 0u32;
        loop {
            rounds += 1;
            if rounds > 100_000 {
                return Err(Error::replica("catching up did not converge"));
            }
            let position = local.capture().position().txid;
            if target.is_some_and(|t| t <= position) {
                break;
            }
            if position.0 > 0 {
                let key = ltx_key(0, Txid(position.0 + 1), Txid(position.0 + 1));
                if let Some(obj) = self.replica.get(&key).await? {
                    local.apply(&obj.bytes, ApplyMode::Follow)?;
                    continue;
                }
            }
            let files = list_files(self.replica.as_ref()).await?;
            match next_step(&files, position)? {
                Step::UpToDate => break,
                Step::Apply(info) => {
                    let bytes = download(self.replica.as_ref(), &info).await?;
                    local.apply(&bytes, ApplyMode::Follow)?;
                }
                Step::Rejoin(plan) => {
                    for (i, info) in plan.iter().enumerate() {
                        let bytes = download(self.replica.as_ref(), info).await?;
                        let mode = if i == 0 {
                            ApplyMode::Replace
                        } else {
                            ApplyMode::Follow
                        };
                        local.apply(&bytes, mode)?;
                    }
                }
            }
        }
        if self.state.role != Role::Writer {
            self.pushed = local.capture().position().txid;
        }
        self.last_sync_at = Some(self.now());
        Ok(())
    }

    /// Uploads every local TXID the replica does not hold yet, then head.
    async fn push(&mut self, local: &mut dyn Local) -> Result<()> {
        let capture = local.capture();
        let position = capture.position().txid;
        if position > self.pushed {
            if let Some(h) = self.refresh_head().await?
                && self.superseded_by(&h)
            {
                return self.lose_role(local).await;
            }
            let mut t = self.pushed.0;
            while t < position.0 {
                t += 1;
                let txid = Txid(t);
                let bytes = match capture.read(txid) {
                    Ok(bytes) => bytes,
                    Err(e) if e.kind() == moul_vfs::ErrorKind::NotFound => {
                        match self.adopt_unheld(local, txid).await? {
                            Some(last) => {
                                self.pushed = last;
                                t = last.0;
                                continue;
                            }
                            // The replica's state replaced the database.
                            None if self.state.role == Role::Writer => break,
                            None => return Ok(()),
                        }
                    }
                    Err(e) => return Err(e.into()),
                };
                let key = ltx_key(0, txid, txid);
                match self
                    .replica
                    .put(&key, &bytes, Condition::IfNoneMatch)
                    .await?
                {
                    Put::Written { .. } => self.pushed = txid,
                    Put::Refused => match self.replica.get(&key).await? {
                        Some(obj) if obj.bytes == bytes => self.pushed = txid,
                        _ => return self.lose_role(local).await,
                    },
                }
            }
        }
        if self.head.as_ref().is_none_or(|h| h.txid < self.pushed.0) && self.pushed.0 > 0 {
            self.write_head(local).await?;
        }
        if self.state.role == Role::Writer {
            capture.release(self.pushed)?;
            self.last_sync_at = Some(self.now());
        }
        Ok(())
    }

    /// For a writer that does not hold the file for `from`, a TXID it has
    /// not pushed as far as it knows: it applied the file from the replica,
    /// or pushed and released it before a restart resumed from an older
    /// head. The files from `from` to the first one it holds (or to its
    /// position) are the replica's if the replica's state at the last of
    /// them, with the files held after it applied, is the local database;
    /// returns that last TXID then. If not, the device's history has left
    /// the replica's: it keeps a copy, takes the replica's state, and
    /// writes on from it if the record still names it, or follows; returns
    /// `None`. Never the case unless something failed half way, so the
    /// cost of a restore does not matter.
    async fn adopt_unheld(&mut self, local: &mut dyn Local, from: Txid) -> Result<Option<Txid>> {
        let capture = local.capture();
        // The files held run on without a gap to the position.
        let last = match capture.first_held() {
            Some(first) if first > from => Txid(first.0 - 1),
            _ => capture.position().txid,
        };
        let theirs = match restore_db(self.replica.as_ref(), Some(last)).await {
            Ok((_, db)) => Some(db),
            // The replica does not reach `last`, or cannot be read up to it.
            Err(e) if e.kind() == ErrorKind::Replica => None,
            Err(e) => return Err(e),
        };
        // The position and the database together, with no wait between, so
        // that no write lands between them.
        let position = capture.position().txid;
        let mine = local.export()?;
        if let Some(mut db) = theirs {
            let mut at = last;
            while at < position {
                let next = Txid(at.0 + 1);
                let Ok(bytes) = capture.read(next) else {
                    break;
                };
                moul_ltx::restore::apply(&mut db, at, &Ltx::decode(&bytes)?)?;
                at = next;
            }
            if at == position && db == mine {
                return Ok(Some(last));
            }
        }
        // Read-only before the copy is taken: a write after it would be in
        // neither the copy nor the replica's state that replaces it.
        self.tell(local, Role::Follower);
        let position = local.capture().position().txid;
        self.keep_copy(
            local,
            from,
            position,
            "this device's history differs from the replica's, which replaced it",
        )
        .await?;
        self.resync(local).await?;
        self.pushed = local.capture().position().txid;
        if self.still_writes(self.state.generation).await? {
            self.tell(local, Role::Writer);
        } else {
            self.lose_role(local).await?;
        }
        Ok(None)
    }

    async fn write_head(&mut self, local: &mut dyn Local) -> Result<()> {
        for _ in 0..3 {
            let head = Head {
                txid: self.pushed.0,
                writer: self.me().to_owned(),
                generation: self.state.generation,
                replica: self.state.replica.clone().unwrap_or_default(),
                at: self.now(),
            };
            let condition = match &self.head_etag {
                Some(etag) => Condition::IfMatch(etag.clone()),
                None => Condition::IfNoneMatch,
            };
            match put_json(self.replica.as_ref(), HEAD_KEY, &head, condition).await? {
                Put::Written { etag } => {
                    self.head = Some(head);
                    self.head_etag = Some(etag);
                    return Ok(());
                }
                Put::Refused => match self.refresh_head().await? {
                    Some(h) if self.superseded_by(&h) => return self.lose_role(local).await,
                    // An older writer's late head, or our own lost answer.
                    _ => continue,
                },
            }
        }
        Err(Error::new(
            ErrorKind::Conflict,
            "moul/head.json kept changing; will retry",
        ))
    }

    /// Another device writes now. Keeps a copy of anything local the replica
    /// does not hold, then follows.
    async fn lose_role(&mut self, local: &mut dyn Local) -> Result<()> {
        // Read-only before reading the position: a write after it would be
        // counted as pushed, or left out of the copy, and the new writer's
        // files applied on top of it. Should this fail, the device stays
        // read-only until the next call settles its role again.
        self.tell(local, Role::Follower);
        let rec = get_json::<WriterRecord>(self.replica.as_ref(), WRITER_KEY).await?;
        let generation = rec
            .as_ref()
            .map_or(self.state.generation, |(r, _)| r.generation);
        self.writer = rec.map(|(r, _)| r);
        let position = local.capture().position().txid;
        if let Some(from) = self.first_unheld(local, self.pushed).await? {
            let only_fence = self.fence == Some(from) && from == position;
            if !only_fence {
                self.keep_copy(
                    local,
                    from,
                    position,
                    "another device took over writing before these changes were pushed",
                )
                .await?;
            }
            self.resync(local).await?;
        }
        self.fence = None;
        self.set_role(local, Role::Follower, generation)?;
        self.pushed = local.capture().position().txid;
        Box::pin(self.catch_up(local)).await
    }

    /// Stops syncing: the device keeps its data and may write to it. If it
    /// connects to the same replica later, anything it wrote meanwhile is
    /// pushed if it still writes, or kept as a conflict copy if not.
    pub fn disconnect(&mut self, local: &mut dyn Local) -> Result<()> {
        self.connected = false;
        self.set_role(local, Role::None, self.state.generation)
    }

    /// Becomes the writer: catches up, swaps the writer record, and fences.
    pub async fn take_over(&mut self, local: &mut dyn Local) -> Result<()> {
        let result = self.take_over_inner(local).await;
        self.record(result).await
    }

    async fn take_over_inner(&mut self, local: &mut dyn Local) -> Result<()> {
        if !self.settled() {
            self.connect_inner(local).await?;
        }
        if self.state.role == Role::Writer {
            return Ok(());
        }
        self.catch_up(local).await?;
        let found = get_json::<WriterRecord>(self.replica.as_ref(), WRITER_KEY).await?;
        let (generation, condition, replica) = match found {
            Some((rec, etag)) => (rec.generation, Condition::IfMatch(etag), rec.replica),
            None => (
                0,
                Condition::IfNoneMatch,
                self.state.replica.clone().unwrap_or_default(),
            ),
        };
        let position = local.capture().position().txid;
        if let Some(h) = &self.head
            && h.txid > position.0
        {
            return Err(Error::invalid(format!(
                "this device is at TXID {position} but the replica is at {}; catch up first",
                h.txid
            )));
        }
        let rec = WriterRecord {
            device: self.me().to_owned(),
            label: self.options.label.clone(),
            generation: generation + 1,
            since: position.0,
            replica: replica.clone(),
            at: self.now(),
        };
        if put_json(self.replica.as_ref(), WRITER_KEY, &rec, condition).await? == Put::Refused {
            return Err(Error::new(
                ErrorKind::Conflict,
                "another device changed the writer at the same moment; try again",
            ));
        }
        self.state.replica = Some(replica);
        self.writer = Some(rec.clone());
        // The host stays read-only until the fence is in: fencing may
        // replace the database with the previous writer's last upload.
        // Should it fail from here, the next call finishes taking over
        // before the device writes (see `resume_writing`), and the device
        // state keeps the host read-only until then across a restart.
        self.state.unfenced = true;
        self.save_role(local, Role::Writer, rec.generation)?;
        self.pushed = position;
        self.fence_and_write_head(local).await
    }

    /// The last steps of taking over, from a local database the replica
    /// holds up to `pushed`: uploads a fence, so the previous writer's next
    /// upload is refused, and writes head of the new generation, which
    /// followers and the previous writer read. Only then is the host told
    /// the device writes.
    async fn fence_and_write_head(&mut self, local: &mut dyn Local) -> Result<()> {
        if self.pushed.0 > 0 {
            let capture = local.capture();
            let mut fenced = false;
            for _ in 0..5 {
                // A newer take-over (whose record lands before its fence) may
                // have come in meanwhile: fencing on top of it would make two
                // writers. Follow it instead.
                if !self.still_writes(self.state.generation).await? {
                    self.pushed = local.capture().position().txid;
                    return self.lose_role(local).await;
                }
                let fence = local.fence()?;
                self.fence = Some(fence);
                let bytes = capture.read(fence)?;
                let key = ltx_key(0, fence, fence);
                match self
                    .replica
                    .put(&key, &bytes, Condition::IfNoneMatch)
                    .await?
                {
                    Put::Written { .. } => {
                        self.pushed = fence;
                        fenced = true;
                        break;
                    }
                    Put::Refused => {
                        // The previous writer's last upload landed after we
                        // caught up. Take it, and fence after it.
                        self.resync(local).await?;
                        self.pushed = local.capture().position().txid;
                    }
                }
            }
            if !fenced {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "the previous writer is still pushing; try again",
                ));
            }
        }
        self.fenced(local)?;
        self.write_head(local).await?;
        local.capture().release(self.pushed)?;
        // A writer now, unless writing head found another had taken over.
        let role = self.state.role;
        self.tell(local, role);
        self.last_sync_at = Some(self.now());
        Ok(())
    }

    /// One full round: a writer pushes and, if due, maintains the replica;
    /// a follower pulls. Connects first if needed.
    pub async fn sync(&mut self, local: &mut dyn Local) -> Result<()> {
        let result = self.sync_inner(local, true).await;
        self.record(result).await
    }

    /// What to call about once a second while the app is open: pushes as
    /// soon as there is something to push, polls and maintains when due.
    pub async fn tick(&mut self, local: &mut dyn Local) -> Result<()> {
        let result = self.sync_inner(local, false).await;
        self.record(result).await
    }

    async fn sync_inner(&mut self, local: &mut dyn Local, force: bool) -> Result<()> {
        if !self.settled() {
            self.connect_inner(local).await?;
            self.last_poll = Some(self.now());
        }
        let now = self.now();
        let due = |last: Option<i64>, every: i64| force || last.is_none_or(|l| now - l >= every);
        match self.state.role {
            Role::Writer => {
                // Head behind what was pushed (its write failed) counts as
                // pending: it is written before the files are released
                // below, or a restart, resuming from head, would find files
                // to push that it no longer holds.
                let head_behind =
                    self.pushed.0 > 0 && self.head.as_ref().is_none_or(|h| h.txid < self.pushed.0);
                let pending = local.capture().position().txid > self.pushed || head_behind;
                if pending {
                    self.push(local).await?;
                } else if due(self.last_poll, self.options.poll_every) {
                    self.last_poll = Some(now);
                    if let Some(h) = self.refresh_head().await?
                        && self.superseded_by(&h)
                    {
                        return self.lose_role(local).await;
                    }
                }
                let idle = local.capture().position().txid == self.pushed;
                if self.state.role == Role::Writer
                    && idle
                    && due(self.last_maintain, self.options.maintain_every)
                {
                    self.last_maintain = Some(now);
                    // Every file is pushed: drop any a busy push kept, before
                    // maintenance prunes the replica's copies of them (see
                    // `holds_replicas_history`).
                    local.capture().release(self.pushed)?;
                    let snapshot = local.export()?;
                    let report = maintain(
                        self.replica.as_ref(),
                        self.pushed,
                        Some(&snapshot),
                        &self.options.retention,
                        now,
                    )
                    .await?;
                    self.report = Some(report);
                }
            }
            Role::Follower | Role::None => {
                if due(self.last_poll, self.options.poll_every) {
                    self.last_poll = Some(now);
                    self.catch_up(local).await?;
                }
            }
        }
        Ok(())
    }
}
