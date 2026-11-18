//! Compaction, snapshots and retention: what the writer does when idle.
//!
//! Everything here is decided from the replica's listing, so it keeps no
//! state of its own and any writer, or the CLI, can run it. Ages are
//! measured in the replica's own clock (the newest modification time it
//! lists, usually the head record just written), never the device's, so a
//! device with a wrong clock neither prunes early nor compacts on every pass. Compaction only
//! ever merges files already in the replica, so a writer that has just lost
//! its role and compacts anyway still writes correct files. Every deletion
//! is checked first: the newest TXID must stay restorable without the file.

use moul_ltx::{
    FileInfo, Header, Ltx, SNAPSHOT_LEVEL, Txid, compact_to_vec, encode, is_contiguous,
};

use crate::Result;
use crate::layout::{file_infos, ltx_key};
use crate::replica::{Condition, Put, Replica};
use crate::restore::download_all;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// When to compact, snapshot and prune. Times are in milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retention {
    /// How often level-0 files are merged into a level-1 file.
    pub level1_every: i64,
    /// How often level-1 files are merged into a level-2 file.
    pub level2_every: i64,
    /// How often a snapshot is written.
    pub snapshot_every: i64,
    /// How long level-0 and level-1 files are kept once compacted. Followers
    /// that sleep longer than this start again from a snapshot.
    pub compacted_kept: i64,
    /// How far back the database can be restored to.
    pub history: i64,
}

impl Default for Retention {
    /// Moul's defaults: level 1 every 5 minutes, level 2 every hour, a
    /// snapshot every 24 hours, level-0 files kept 24 hours, 7 days of
    /// history.
    fn default() -> Self {
        Self {
            level1_every: 5 * MINUTE,
            level2_every: HOUR,
            snapshot_every: DAY,
            compacted_kept: DAY,
            history: 7 * DAY,
        }
    }
}

/// What a maintenance pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Files written: level, first and last TXID.
    pub written: Vec<(u8, Txid, Txid)>,
    /// Keys deleted.
    pub deleted: Vec<String>,
}

/// The highest TXID level `level` covers, and when its newest file was made.
fn coverage(files: &[FileInfo], level: u8) -> (Txid, i64) {
    files
        .iter()
        .filter(|f| f.level == level)
        .fold((Txid(0), i64::MIN), |(t, c), f| {
            (t.max(f.max_txid), c.max(f.created_at))
        })
}

/// Merges the files of `from` that follow `level`'s coverage into one file
/// at `level`, if it is due.
async fn compact_level(
    replica: &dyn Replica,
    files: &[FileInfo],
    from: u8,
    level: u8,
    every: i64,
    now: i64,
    report: &mut Report,
) -> Result<()> {
    let (covered, last) = coverage(files, level);
    if last != i64::MIN && now - last < every {
        return Ok(());
    }
    let mut run: Vec<FileInfo> = files
        .iter()
        .filter(|f| f.level == from && f.max_txid > covered)
        .copied()
        .collect();
    run.sort_by_key(|f| (f.min_txid, f.max_txid));
    // The longest run that follows on from what the level covers.
    let mut chosen = Vec::new();
    let mut at = covered;
    for f in run {
        if at.0 == 0 && chosen.is_empty() || is_contiguous(at, f.min_txid, f.max_txid) {
            at = f.max_txid;
            chosen.push(f);
        } else if f.max_txid <= at {
            continue;
        } else {
            break;
        }
    }
    if chosen.is_empty() {
        return Ok(());
    }
    let bytes = download_all(replica, &chosen).await?;
    let decoded = bytes
        .iter()
        .map(|b| Ltx::decode(b))
        .collect::<moul_ltx::Result<Vec<_>>>()?;
    let merged = compact_to_vec(&decoded)?;
    let h = *Ltx::decode(&merged)?.header();
    let key = ltx_key(level, h.min_txid, h.max_txid);
    if let Put::Written { .. } = replica.put(&key, &merged, Condition::IfNoneMatch).await? {
        report.written.push((level, h.min_txid, h.max_txid));
    }
    Ok(())
}

/// Writes a snapshot of the database at `head` if one is due. `local` is the
/// writer's own copy of the database at `head`, which saves downloading.
async fn snapshot(
    replica: &dyn Replica,
    files: &[FileInfo],
    head: Txid,
    local: Option<&[u8]>,
    retention: &Retention,
    now: i64,
    report: &mut Report,
) -> Result<()> {
    let (covered, last) = coverage(files, SNAPSHOT_LEVEL);
    if head.0 == 0 || covered >= head || (last != i64::MIN && now - last < retention.snapshot_every)
    {
        return Ok(());
    }
    let db = match local {
        Some(db) => db.to_vec(),
        None => {
            let plan = moul_ltx::restore::plan(files, Some(head))?;
            let bytes = download_all(replica, &plan).await?;
            let decoded = bytes
                .iter()
                .map(|b| Ltx::decode(b))
                .collect::<moul_ltx::Result<Vec<_>>>()?;
            moul_ltx::restore::restore_to_vec(&decoded)?
        }
    };
    if db.len() < 100 {
        return Ok(());
    }
    let raw = u16::from_be_bytes([db[16], db[17]]);
    let ps = if raw == 1 { 65_536 } else { raw as u32 };
    let pages = (db.len() / ps as usize) as u32;
    let lock = moul_ltx::lock_pgno(ps);
    let header = Header::new(ps, pages, Txid(1), head, now);
    let bytes = encode(
        header,
        (1..=pages).filter(|&p| p != lock).map(|p| {
            let at = (p as usize - 1) * ps as usize;
            (p, &db[at..at + ps as usize])
        }),
    )?;
    let key = ltx_key(SNAPSHOT_LEVEL, Txid(1), head);
    if let Put::Written { .. } = replica.put(&key, &bytes, Condition::IfNoneMatch).await? {
        report.written.push((SNAPSHOT_LEVEL, Txid(1), head));
    }
    Ok(())
}

/// The files retention removes from `files`.
fn expired(files: &[FileInfo], retention: &Retention, now: i64) -> Vec<FileInfo> {
    let mut out: Vec<FileInfo> = Vec::new();
    let old = |f: &FileInfo, age: i64| now - f.created_at >= age;
    // Compacted level-0 and level-1 files, once old enough that followers
    // have had time to read them.
    for level in [0u8, 1] {
        let (covered, _) = coverage(files, level + 1);
        out.extend(
            files
                .iter()
                .filter(|f| {
                    f.level == level && f.max_txid <= covered && old(f, retention.compacted_kept)
                })
                .copied(),
        );
    }
    // History: keep every snapshot younger than `history`, the newest one
    // older than that (the base for the oldest moments still kept), and the
    // newest of all.
    let mut snaps: Vec<&FileInfo> = files.iter().filter(|f| f.level == SNAPSHOT_LEVEL).collect();
    snaps.sort_by_key(|f| (f.max_txid, f.created_at));
    if let Some(newest) = snaps.last().copied() {
        let base = snaps.iter().copied().rfind(|f| old(f, retention.history));
        let floor = snaps
            .iter()
            .copied()
            .filter(|f| {
                !old(f, retention.history)
                    || std::ptr::eq(*f, newest)
                    || base.is_some_and(|b| std::ptr::eq(*f, b))
            })
            .map(|f| f.max_txid)
            .min()
            .unwrap_or(newest.max_txid);
        out.extend(snaps.iter().filter(|f| f.max_txid < floor).map(|f| **f));
        // Ranges that end before the oldest snapshot kept serve no restore.
        out.extend(
            files
                .iter()
                .filter(|f| {
                    f.level < SNAPSHOT_LEVEL
                        && f.max_txid <= floor
                        && old(f, retention.compacted_kept)
                })
                .copied(),
        );
    }
    out.sort_by_key(|f| (f.level, f.min_txid, f.max_txid));
    out.dedup();
    out
}

/// The replica's LTX files, and the time now by the replica's clock: the
/// newest modification time it lists, or `fallback` when it lists nothing.
async fn listing(replica: &dyn Replica, fallback: i64) -> Result<(Vec<FileInfo>, i64)> {
    let objects = replica.list("").await?;
    let now = objects.iter().map(|o| o.modified).max().unwrap_or(fallback);
    Ok((file_infos(&objects), now))
}

async fn list_files(replica: &dyn Replica) -> Result<Vec<FileInfo>> {
    Ok(listing(replica, 0).await?.0)
}

/// Runs one maintenance pass on `replica`, whose newest TXID is `head`.
/// `now` is the device's clock, used only when the replica is empty.
pub async fn maintain(
    replica: &dyn Replica,
    head: Txid,
    local: Option<&[u8]>,
    retention: &Retention,
    now: i64,
) -> Result<Report> {
    let mut report = Report::default();
    let (files, now) = listing(replica, now).await?;
    compact_level(
        replica,
        &files,
        0,
        1,
        retention.level1_every,
        now,
        &mut report,
    )
    .await?;
    let files = if report.written.is_empty() {
        files
    } else {
        list_files(replica).await?
    };
    let before = report.written.len();
    compact_level(
        replica,
        &files,
        1,
        2,
        retention.level2_every,
        now,
        &mut report,
    )
    .await?;
    let files = if report.written.len() == before {
        files
    } else {
        list_files(replica).await?
    };
    let before = report.written.len();
    snapshot(replica, &files, head, local, retention, now, &mut report).await?;
    let files = if report.written.len() == before {
        files
    } else {
        list_files(replica).await?
    };

    let doomed = expired(&files, retention, now);
    if doomed.is_empty() {
        return Ok(report);
    }
    // Never prune what the newest state needs.
    let remaining: Vec<FileInfo> = files
        .iter()
        .filter(|f| !doomed.contains(f))
        .copied()
        .collect();
    if moul_ltx::restore::plan(&remaining, None).is_err() {
        return Ok(report);
    }
    for f in doomed {
        let key = f.path();
        replica.delete(&key).await?;
        report.deleted.push(key);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(level: u8, min: u64, max: u64, created_at: i64) -> FileInfo {
        FileInfo {
            level,
            min_txid: Txid(min),
            max_txid: Txid(max),
            size: 1,
            created_at,
        }
    }

    #[test]
    fn retention_keeps_what_restores_need() {
        let r = Retention::default();
        let now = 10 * DAY;
        let mut files = vec![
            info(9, 1, 5, 0),
            info(9, 1, 50, 5 * DAY),
            info(9, 1, 90, 9 * DAY),
            info(2, 6, 50, 5 * DAY),
            info(2, 51, 90, 9 * DAY),
            info(1, 51, 80, 8 * DAY),
            info(1, 81, 95, now - HOUR),
            info(0, 80, 80, 8 * DAY),
            info(0, 95, 95, now - HOUR),
            info(0, 96, 96, now),
        ];
        assert_eq!(
            expired(&files, &r, now),
            vec![info(0, 80, 80, 8 * DAY), info(1, 51, 80, 8 * DAY)],
            "the snapshot from before the window is the base for its oldest moments"
        );
        // A newer base makes the older snapshot, and the range after it, useless.
        files.push(info(9, 1, 30, 2 * DAY));
        assert_eq!(
            expired(&files, &r, now),
            vec![
                info(0, 80, 80, 8 * DAY),
                info(1, 51, 80, 8 * DAY),
                info(9, 1, 5, 0)
            ]
        );
    }
}
