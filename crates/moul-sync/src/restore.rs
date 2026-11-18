//! Rebuilding a database from a replica, and choosing what a follower
//! downloads next.

use moul_ltx::{FileInfo, Header, Ltx, SNAPSHOT_LEVEL, Txid, is_contiguous, restore};

use crate::layout::list_files;
use crate::replica::Replica;
use crate::{Error, ErrorKind, Result};

/// Downloads `info`'s file.
pub async fn download(replica: &dyn Replica, info: &FileInfo) -> Result<Vec<u8>> {
    let key = info.path();
    replica.get(&key).await?.map(|o| o.bytes).ok_or_else(|| {
        Error::new(
            ErrorKind::NotFound,
            format!(
                "{key} is gone from {}; it was pruned while reading",
                replica.describe()
            ),
        )
    })
}

/// Downloads every file in `plan`, in order.
pub async fn download_all(replica: &dyn Replica, plan: &[FileInfo]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(plan.len());
    for info in plan {
        out.push(download(replica, info).await?);
    }
    Ok(out)
}

/// The database `replica` holds at `target` (the newest TXID if `None`), as
/// a SQLite file, with the header of the range it was built from.
pub async fn restore_db(replica: &dyn Replica, target: Option<Txid>) -> Result<(Header, Vec<u8>)> {
    let files = list_files(replica).await?;
    let plan = moul_ltx::restore::plan(&files, target)?;
    let bytes = download_all(replica, &plan).await?;
    let decoded = bytes
        .iter()
        .map(|b| Ltx::decode(b))
        .collect::<moul_ltx::Result<Vec<_>>>()?;
    let mut db = Vec::new();
    let header = restore::restore(&decoded, &mut db)?;
    Ok((header, db))
}

/// Whether `next` is a better step than `curr`: reach further, then come
/// from a lower level (smaller files for a follower that is close behind).
fn better(curr: &FileInfo, next: &FileInfo) -> bool {
    if next.max_txid != curr.max_txid {
        return next.max_txid > curr.max_txid;
    }
    next.level < curr.level
}

/// What a follower at `position` should do next, given `files`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Apply this file on top of the database.
    Apply(FileInfo),
    /// Nothing follows on from `position` (the files it needs were pruned),
    /// so start again from a snapshot with this plan.
    Rejoin(Vec<FileInfo>),
    /// Up to date with what the files hold.
    UpToDate,
}

/// Chooses the next file for a follower at `position`: the one that follows
/// on and reaches furthest, from any level but the snapshots.
pub fn next_step(files: &[FileInfo], position: Txid) -> Result<Step> {
    let newest = files.iter().map(|f| f.max_txid).max().unwrap_or(Txid(0));
    if newest <= position {
        return Ok(Step::UpToDate);
    }
    let next = files
        .iter()
        .filter(|f| {
            f.level < SNAPSHOT_LEVEL
                && position.0 > 0
                && is_contiguous(position, f.min_txid, f.max_txid)
        })
        .reduce(|best, f| if better(best, f) { f } else { best });
    match next {
        Some(f) => Ok(Step::Apply(*f)),
        None => Ok(Step::Rejoin(moul_ltx::restore::plan(files, None)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(level: u8, min: u64, max: u64) -> FileInfo {
        FileInfo {
            level,
            min_txid: Txid(min),
            max_txid: Txid(max),
            size: 1,
            created_at: 0,
        }
    }

    #[test]
    fn follower_steps() {
        let files = [
            info(9, 1, 10),
            info(1, 1, 12),
            info(0, 11, 11),
            info(0, 12, 12),
            info(0, 13, 13),
        ];
        assert_eq!(
            next_step(&files, Txid(10)).unwrap(),
            Step::Apply(info(1, 1, 12))
        );
        assert_eq!(
            next_step(&files, Txid(12)).unwrap(),
            Step::Apply(info(0, 13, 13))
        );
        assert_eq!(next_step(&files, Txid(13)).unwrap(), Step::UpToDate);
        // Asleep past retention: nothing starts early enough.
        let pruned = [info(9, 1, 20), info(0, 21, 21)];
        assert_eq!(
            next_step(&pruned, Txid(5)).unwrap(),
            Step::Rejoin(vec![info(9, 1, 20), info(0, 21, 21)])
        );
        // A new device has nothing to follow on from.
        assert!(matches!(
            next_step(&pruned, Txid(0)).unwrap(),
            Step::Rejoin(_)
        ));
    }
}
