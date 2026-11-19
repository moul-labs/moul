//! What the engine's thread does between requests.
//!
//! | Chore | Every | Does |
//! | --- | --- | --- |
//! | Streams | 15 s | ends live streams whose token expired or was revoked |
//! | Housekeeping | 10 min | gives back jobs whose runner stopped, deletes finished effects older than a week and the request log and visits past their retention |
//! | Replicate | 1 s | pushes new commits to the replica as LTX files, Litestream's format, and compacts it; while this server writes, the writer-only store too, under `moul/writer-only/` |
//! | Release | 30 s | without a replica, drops the level-0 files nobody will read |
//!
//! Replication runs on the engine's thread, between requests, because it
//! reads the database's own files; a push is one upload of a small file.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use moul_core::Role as EngineRole;
use moul_sync::{
    Local, Options, Prefixed, Replica, Role, StoreLocal, Sync, WriterOnlyLocal, block_on,
};
use moul_vfs::Capture;

use crate::db::{Chore, Db};

/// Ends live streams whose credentials no longer hold.
pub struct Streams {
    last: Instant,
}

impl Default for Streams {
    fn default() -> Self {
        Streams {
            last: Instant::now(),
        }
    }
}

impl Chore for Streams {
    fn name(&self) -> &str {
        "streams"
    }
    fn due_in(&self, _: &Db) -> Duration {
        Duration::from_secs(15).saturating_sub(self.last.elapsed())
    }
    fn run(&mut self, db: &mut Db) {
        self.last = Instant::now();
        if db.stream_count() > 0 {
            db.engine.check_subscriptions();
        }
    }
    fn finish(&mut self, _: &mut Db) {}
}

/// Tidies effects and logs.
#[derive(Default)]
pub struct Housekeeping {
    last: Option<Instant>,
}

impl Chore for Housekeeping {
    fn name(&self) -> &str {
        "housekeeping"
    }
    fn due_in(&self, _: &Db) -> Duration {
        match self.last {
            None => Duration::ZERO,
            Some(t) => Duration::from_secs(600).saturating_sub(t.elapsed()),
        }
    }
    fn run(&mut self, db: &mut Db) {
        self.last = Some(Instant::now());
        if db.engine.role() == EngineRole::Follower {
            return;
        }
        db.engine.tidy_effects(moul_core::effects::LEASE_MS);
        let settings = db.engine.settings();
        let days = |k: &str| {
            settings
                .get(k)
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|d| *d >= 1)
                .unwrap_or(30)
        };
        let now = moul_vfs::system_clock();
        let conn = db.engine.store().conn();
        let cutoff = |d: i64| moul_core::time::rfc3339(now - d * 86_400_000);
        let _ = conn.execute(
            "DELETE FROM writer._requests WHERE createdAt <= ?1",
            [cutoff(days("request_log_retention_days"))],
        );
        let _ = conn.execute(
            "DELETE FROM writer._visits WHERE started_at <= ?1",
            [cutoff(days("visit_retention_days"))],
        );
    }
    fn finish(&mut self, _: &mut Db) {}
}

/// Without a replica: level-0 files pile up for nobody, so drop them,
/// unless this database once joined a replica (it keeps them to push when
/// replication is turned back on, as a browser does). The same for the
/// writer-only store's files, by whether that store joined one.
pub struct Release {
    capture: Capture,
    writer_capture: Capture,
    last: Instant,
}

impl Release {
    /// Releases the files of `capture` (`main.db`) and `writer_capture`
    /// (`writer.db`).
    pub fn new(capture: Capture, writer_capture: Capture) -> Release {
        Release {
            capture,
            writer_capture,
            last: Instant::now() - Duration::from_secs(60),
        }
    }
}

impl Chore for Release {
    fn name(&self) -> &str {
        "release"
    }
    fn due_in(&self, _: &Db) -> Duration {
        Duration::from_secs(30).saturating_sub(self.last.elapsed())
    }
    fn run(&mut self, db: &mut Db) {
        self.last = Instant::now();
        let joined = moul_sync::store_load_state(db.engine.store())
            .ok()
            .flatten()
            .is_some_and(|s| s.replica.is_some());
        if !joined {
            release_all(&self.capture);
        }
        let writer_only_joined = WriterOnlyLocal {
            store: db.engine.store_mut(),
            capture: self.writer_capture.clone(),
            conflicts: None,
        }
        .load_state()
        .ok()
        .flatten()
        .is_some_and(|s| s.replica.is_some());
        if !writer_only_joined {
            release_all(&self.writer_capture);
        }
    }
}

/// Drops every level-0 file `capture` holds.
fn release_all(capture: &Capture) {
    let at = capture.position().txid;
    let _ = capture.release(at);
}

/// Logs a sync's result when it changes: a new error, or working again.
fn report(last: &mut Option<String>, replica: &dyn Replica, result: moul_sync::Result<()>) {
    match result {
        Ok(()) => {
            if last.take().is_some() {
                eprintln!("moul: replication to {} works again", replica.describe());
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if last.as_deref() != Some(msg.as_str()) {
                eprintln!("moul: replication to {}: {msg}", replica.describe());
            }
            *last = Some(msg);
        }
    }
}

/// Pushes to a replica, as a device does: the shared store, and while this
/// server writes it, the writer-only store under `moul/writer-only/`, so
/// that a restore brings back root users, password hashes, keys, settings
/// and the outbox.
///
/// The writer-only store has a sync of its own over the same transport,
/// which only pushes ([`Options::push_only`]): it never takes another
/// device's writer-only store in place of this server's. While another
/// device writes the shared store, this server's writer-only store (its own
/// request log, say) stays local and its level-0 files are dropped.
pub struct Replicate {
    sync: Sync,
    capture: Capture,
    writer_only: Sync,
    writer_capture: Capture,
    conflicts: PathBuf,
    last: Instant,
    error: Option<String>,
    writer_only_error: Option<String>,
    follow_only: bool,
}

impl Replicate {
    /// Replicates `capture` (`main.db`) and `writer_capture` (`writer.db`)
    /// through `replica`, keeping conflict copies in `conflicts`.
    pub fn new(
        replica: Box<dyn Replica>,
        capture: Capture,
        writer_capture: Capture,
        conflicts: PathBuf,
        label: &str,
        follow_only: bool,
    ) -> Replicate {
        let replica: Rc<dyn Replica> = Rc::from(replica);
        Replicate {
            sync: Sync::new(
                replica.clone(),
                Options {
                    label: label.to_owned(),
                    follow_only,
                    ..Options::default()
                },
            ),
            capture,
            writer_only: Sync::new(
                Prefixed::writer_only(replica),
                Options {
                    label: label.to_owned(),
                    push_only: true,
                    ..Options::default()
                },
            ),
            writer_capture,
            conflicts,
            last: Instant::now() - Duration::from_secs(60),
            error: None,
            writer_only_error: None,
            follow_only,
        }
    }

    /// Pushes the writer-only store if this server writes the shared one;
    /// otherwise drops its level-0 files, which nobody will read.
    fn replicate_writer_only(&mut self, db: &mut Db) {
        if self.follow_only || self.sync.role() != Role::Writer {
            release_all(&self.writer_capture);
            return;
        }
        let result = {
            let mut local = WriterOnlyLocal {
                store: db.engine.store_mut(),
                capture: self.writer_capture.clone(),
                conflicts: Some(self.conflicts.clone()),
            };
            block_on(self.writer_only.tick(&mut local))
        };
        report(
            &mut self.writer_only_error,
            self.writer_only.replica(),
            result,
        );
    }
}

impl Chore for Replicate {
    fn name(&self) -> &str {
        "replicate"
    }
    fn due_in(&self, _: &Db) -> Duration {
        Duration::from_secs(1).saturating_sub(self.last.elapsed())
    }
    fn run(&mut self, db: &mut Db) {
        self.last = Instant::now();
        let before = db.engine.store().volume().last_commit();
        let result = {
            let mut local = StoreLocal {
                store: db.engine.store_mut(),
                capture: self.capture.clone(),
                conflicts: Some(self.conflicts.clone()),
            };
            block_on(self.sync.tick(&mut local))
        };
        report(&mut self.error, self.sync.replica(), result);
        // Another device took over the replica: this server only follows
        // now, and refuses writes. Or it took the role back.
        let want = match self.sync.role() {
            Role::Follower => Some(EngineRole::Follower),
            // A read-only server never writes, whatever sync says.
            Role::Writer if self.follow_only => None,
            Role::Writer => Some(EngineRole::Writer),
            Role::None => None,
        };
        if let Some(role) = want
            && role != db.engine.role()
        {
            eprintln!(
                "moul: this server now {} the replica",
                if role == EngineRole::Writer {
                    "writes"
                } else {
                    "follows another writer of"
                }
            );
            let _ = db.engine.set_role(role);
        }
        // Pages from the writer arrived underneath SQLite.
        if db.engine.store().volume().last_commit() != before
            && db.engine.role() == EngineRole::Follower
        {
            db.engine.catch_up();
        }
        self.replicate_writer_only(db);
    }
}
