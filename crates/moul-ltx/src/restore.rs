//! Rebuilding a database from a replica's files, and following one.
//!
//! A replica keeps files by level: level 0 holds one file per sync batch,
//! levels 1 to 8 hold compacted ranges, and level 9 holds snapshots. To
//! restore, [`plan`] picks the newest usable snapshot and then the files that
//! extend it furthest without a gap, exactly as Litestream 0.5's
//! `CalcRestorePlan` does, and [`restore`] merges them into a database file.

use std::io::Write;

use crate::compact::{Merged, is_contiguous};
use crate::decode::Ltx;
use crate::format::{Header, Txid, file_name};
use crate::{Error, Result};

/// The level that holds snapshots.
pub const SNAPSHOT_LEVEL: u8 = 9;

/// A file in a replica, as a listing describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// Its level, 0 to 9.
    pub level: u8,
    /// The first transaction it holds.
    pub min_txid: Txid,
    /// The last transaction it holds.
    pub max_txid: Txid,
    /// Its size in bytes.
    pub size: u64,
    /// When it was written, in milliseconds since the Unix epoch. Breaks ties
    /// only: the earlier file is preferred.
    pub created_at: i64,
}

impl FileInfo {
    /// The path of the file inside a replica: `ltx/<level>/<min>-<max>.ltx`.
    pub fn path(&self) -> String {
        format!(
            "ltx/{}/{}",
            self.level,
            file_name(self.min_txid, self.max_txid)
        )
    }
}

/// Whether `next` is a better step than `curr`, by Litestream's rule: reach
/// further, then start earlier, then come from a higher level, then be older.
fn better(curr: &FileInfo, next: &FileInfo) -> bool {
    if next.max_txid != curr.max_txid {
        return next.max_txid > curr.max_txid;
    }
    if next.min_txid != curr.min_txid {
        return next.min_txid < curr.min_txid;
    }
    if next.level != curr.level {
        return next.level > curr.level;
    }
    next.created_at < curr.created_at
}

/// Chooses the files to restore from, in the order to apply them: the newest
/// snapshot at or before `target`, then the files that extend it furthest.
/// With no `target`, it restores to the newest transaction, and a gap after
/// the files it chose is an error, since a newer state exists but cannot be
/// reached.
pub fn plan(files: &[FileInfo], target: Option<Txid>) -> Result<Vec<FileInfo>> {
    let within = |f: &FileInfo| target.is_none_or(|t| f.max_txid <= t);
    let mut snapshots: Vec<&FileInfo> =
        files.iter().filter(|f| f.level == SNAPSHOT_LEVEL).collect();
    snapshots.sort_by_key(|f| (f.min_txid, f.max_txid));
    let mut chosen: Vec<FileInfo> = snapshots
        .into_iter()
        .rfind(|f| within(f))
        .into_iter()
        .copied()
        .collect();

    let mut current = chosen.last().map_or(Txid(0), |f| f.max_txid);
    let others: Vec<&FileInfo> = files.iter().filter(|f| f.level < SNAPSHOT_LEVEL).collect();
    if target.is_none_or(|t| current < t) {
        loop {
            let next = others
                .iter()
                .filter(|f| f.min_txid.0 <= current.0 + 1 && f.max_txid > current && within(f))
                .copied()
                .reduce(|best, f| if better(best, f) { f } else { best });
            let Some(next) = next else { break };
            chosen.push(*next);
            current = next.max_txid;
            if target.is_some_and(|t| current >= t) {
                break;
            }
        }
    }

    if chosen.is_empty() {
        return Err(Error::discontiguous(
            "the replica holds no files to restore from",
        ));
    }
    if target.is_none()
        && let Some(gap) = others.iter().find(|f| f.min_txid.0 > current.0 + 1)
    {
        return Err(Error::discontiguous(format!(
            "files reach TXID {current} but the next one starts at {}",
            gap.min_txid
        )));
    }
    if let Some(t) = target
        && current < t
    {
        return Err(Error::discontiguous(format!(
            "TXID {t} is not available: the replica reaches {current}"
        )));
    }
    Ok(chosen)
}

/// Writes the database that `files` add up to into `w`, and returns the
/// header of the merged range. The first file must be a snapshot (start at
/// TXID 1) and the rest must follow on. The lock page, which no LTX file
/// holds, is written as zeros, as SQLite leaves it.
pub fn restore<W: Write>(files: &[Ltx<'_>], mut w: W) -> Result<Header> {
    let merged = Merged::new(files)?;
    let h = merged.header;
    if !h.is_snapshot() {
        return Err(Error::discontiguous(format!(
            "a restore starts from a snapshot, but the first file starts at TXID {}",
            h.min_txid
        )));
    }
    let lock = h.lock_pgno();
    let zeros = vec![0u8; h.page_size as usize];
    let mut next = 1u32;
    merged.for_each(|pgno, data| {
        if pgno != next {
            return Err(Error::discontiguous(format!(
                "the files never write page {next} of {}",
                h.commit
            )));
        }
        w.write_all(data)?;
        next = pgno + 1;
        if next == lock && lock <= h.commit {
            w.write_all(&zeros)?;
            next += 1;
        }
        Ok(())
    })?;
    if next != h.commit + 1 {
        return Err(Error::discontiguous(format!(
            "the files never write page {next} of {}",
            h.commit
        )));
    }
    w.flush()?;
    Ok(h)
}

/// [`restore`] into memory.
pub fn restore_to_vec(files: &[Ltx<'_>]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    restore(files, &mut out)?;
    Ok(out)
}

/// Applies one more file to a database restored to `position`, as a follower
/// does, and returns the new position. The file must follow on from
/// `position`; a snapshot replaces the database whatever the position.
pub fn apply(db: &mut Vec<u8>, position: Txid, file: &Ltx<'_>) -> Result<Txid> {
    let h = file.header();
    if !h.is_snapshot() && !is_contiguous(position, h.min_txid, h.max_txid) {
        return Err(Error::discontiguous(format!(
            "file {}-{} does not follow on from TXID {position}",
            h.min_txid, h.max_txid
        )));
    }
    let ps = h.page_size as usize;
    if !h.is_snapshot() && !db.is_empty() && !db.len().is_multiple_of(ps) {
        return Err(Error::discontiguous(format!(
            "a database of {} bytes does not have {ps}-byte pages",
            db.len()
        )));
    }
    db.resize(h.commit as usize * ps, 0);
    let mut buf = Vec::with_capacity(ps);
    for e in file.pages() {
        file.read_page(e, &mut buf)?;
        let at = (e.pgno as usize - 1) * ps;
        db[at..at + ps].copy_from_slice(&buf);
    }
    Ok(h.max_txid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encode;

    fn info(level: u8, min: u64, max: u64) -> FileInfo {
        FileInfo {
            level,
            min_txid: Txid(min),
            max_txid: Txid(max),
            size: 1000,
            created_at: 0,
        }
    }

    fn ranges(plan: &[FileInfo]) -> Vec<(u8, u64, u64)> {
        plan.iter()
            .map(|f| (f.level, f.min_txid.0, f.max_txid.0))
            .collect()
    }

    #[test]
    fn level_zero_only() {
        let files = [info(0, 1, 1), info(0, 2, 2), info(0, 3, 3)];
        assert_eq!(
            ranges(&plan(&files, None).unwrap()),
            vec![(0, 1, 1), (0, 2, 2), (0, 3, 3)]
        );
        assert_eq!(
            ranges(&plan(&files, Some(Txid(2))).unwrap()),
            vec![(0, 1, 1), (0, 2, 2)]
        );
    }

    #[test]
    fn snapshot_then_the_furthest_reaching_files() {
        let files = [
            info(9, 1, 10),
            info(9, 1, 20),
            info(1, 11, 20),
            info(1, 21, 30),
            info(0, 21, 21),
            info(0, 22, 22),
            info(2, 1, 25),
            info(0, 31, 31),
        ];
        assert_eq!(
            ranges(&plan(&files, None).unwrap()),
            vec![(9, 1, 20), (1, 21, 30), (0, 31, 31)]
        );
        // A target before the newest snapshot uses the older one, and a
        // target nothing reaches exactly is not available, as in Litestream.
        assert_eq!(
            ranges(&plan(&files, Some(Txid(10))).unwrap()),
            vec![(9, 1, 10)]
        );
        assert!(plan(&files, Some(Txid(15))).is_err());
        assert_eq!(
            ranges(&plan(&files, Some(Txid(25))).unwrap()),
            vec![(9, 1, 20), (2, 1, 25)]
        );
    }

    #[test]
    fn gaps_are_errors() {
        let files = [info(0, 1, 1), info(0, 3, 3)];
        assert!(plan(&files, None).is_err());
        assert_eq!(
            ranges(&plan(&files, Some(Txid(1))).unwrap()),
            vec![(0, 1, 1)]
        );
        assert!(plan(&files, Some(Txid(3))).is_err());
        assert!(plan(&[], None).is_err());
    }

    #[test]
    fn restore_and_follow() {
        let snap = encode(
            Header::new(512, 2, Txid(1), Txid(1), 0),
            [(1, &[1u8; 512][..]), (2, &[2u8; 512][..])],
        )
        .unwrap();
        let next = encode(
            Header::new(512, 3, Txid(2), Txid(2), 0),
            [(3, &[3u8; 512][..])],
        )
        .unwrap();
        let shrink = encode(
            Header::new(512, 1, Txid(3), Txid(3), 0),
            [(1, &[4u8; 512][..])],
        )
        .unwrap();
        let files = [&snap, &next, &shrink].map(|b| Ltx::decode(b).unwrap());

        let db = restore_to_vec(&files).unwrap();
        assert_eq!(db, vec![4u8; 512]);

        let mut followed = Vec::new();
        let mut pos = Txid(0);
        assert!(apply(&mut followed, pos, &files[1]).is_err());
        for f in &files[..2] {
            pos = apply(&mut followed, pos, f).unwrap();
        }
        assert_eq!(pos, Txid(2));
        assert_eq!(followed, restore_to_vec(&files[..2]).unwrap());
        pos = apply(&mut followed, pos, &files[2]).unwrap();
        assert_eq!((pos, followed), (Txid(3), db));
        assert!(restore_to_vec(&files[1..]).is_err());
    }
}
