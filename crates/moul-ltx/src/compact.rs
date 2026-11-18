//! Merging consecutive LTX files into one, and turning a run that starts with
//! a snapshot into a database file.

use std::collections::BTreeMap;
use std::io::Write;

use crate::decode::Ltx;
use crate::encode::Encoder;
use crate::format::{FLAG_NO_CHECKSUM, Header, Trailer, Txid};
use crate::{Error, Result};

/// Where a merged page comes from.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// Page `entry` of input `file`.
    Page { file: usize, entry: usize },
    /// The database was cut below this page and grew again without writing
    /// it, so it reads as zeros.
    Zeros,
}

/// The pages of several consecutive files, each from the newest file that
/// holds it.
pub(crate) struct Merged<'f, 'a> {
    files: &'f [Ltx<'a>],
    pub(crate) header: Header,
    pages: BTreeMap<u32, Source>,
}

/// Whether a file of `min..=max` follows on from a database at `prev`: it may
/// repeat transactions already applied, but must add at least one and leave
/// no gap. The same rule as the Go library's `IsContiguous`.
pub fn is_contiguous(prev: Txid, min: Txid, max: Txid) -> bool {
    min.0 <= prev.0 + 1 && max > prev
}

impl<'f, 'a> Merged<'f, 'a> {
    pub(crate) fn new(files: &'f [Ltx<'a>]) -> Result<Self> {
        let (first, last) = match files {
            [] => return Err(Error::invalid("nothing to merge: no input files")),
            [first, .., last] => (first.header(), last.header()),
            [only] => (only.header(), only.header()),
        };
        for pair in files.windows(2) {
            let (a, b) = (pair[0].header(), pair[1].header());
            if a.page_size != b.page_size {
                return Err(Error::discontiguous(format!(
                    "page size changes from {} to {} at TXID {}",
                    a.page_size, b.page_size, b.min_txid
                )));
            }
            if !is_contiguous(a.max_txid, b.min_txid, b.max_txid) {
                return Err(Error::discontiguous(format!(
                    "files do not follow on: {}-{} then {}-{}",
                    a.min_txid, a.max_txid, b.min_txid, b.max_txid
                )));
            }
        }

        let mut pages = BTreeMap::new();
        let mut min_commit = u32::MAX;
        for (fi, f) in files.iter().enumerate() {
            let commit = f.header().commit;
            // A smaller database drops every page past its end.
            pages.split_off(&(commit + 1));
            min_commit = min_commit.min(commit);
            for (ei, e) in f.pages().iter().enumerate() {
                pages.insert(
                    e.pgno,
                    Source::Page {
                        file: fi,
                        entry: ei,
                    },
                );
            }
        }
        let lock = first.lock_pgno();
        for pgno in min_commit.saturating_add(1)..=last.commit {
            if pgno != lock {
                pages.entry(pgno).or_insert(Source::Zeros);
            }
        }

        let header = Header {
            flags: FLAG_NO_CHECKSUM,
            page_size: first.page_size,
            commit: last.commit,
            min_txid: first.min_txid,
            max_txid: last.max_txid,
            timestamp: last.timestamp,
            ..Header::default()
        };
        Ok(Self {
            files,
            header,
            pages,
        })
    }

    /// Calls `f` with every page in order.
    pub(crate) fn for_each(&self, mut f: impl FnMut(u32, &[u8]) -> Result<()>) -> Result<()> {
        let ps = self.header.page_size as usize;
        let zeros = vec![0u8; ps];
        let mut buf = Vec::with_capacity(ps);
        for (&pgno, src) in &self.pages {
            match *src {
                Source::Page { file, entry } => {
                    let ltx = &self.files[file];
                    ltx.read_page(&ltx.pages()[entry], &mut buf)?;
                    f(pgno, &buf)?;
                }
                Source::Zeros => f(pgno, &zeros)?,
            }
        }
        Ok(())
    }
}

/// Merges consecutive files into one file of their whole TXID range, written
/// to `w`. Each page comes from the newest file that holds it, and the result
/// tracks no checksums, as Litestream's compaction does.
///
/// Inputs must share a page size and follow on from each other (see
/// [`is_contiguous`]). If the first starts at TXID 1, the output is a
/// snapshot, which is also how a snapshot is rebuilt from a level's files.
pub fn compact<W: Write>(files: &[Ltx<'_>], w: W) -> Result<(W, Header, Trailer)> {
    let merged = Merged::new(files)?;
    let header = merged.header;
    let mut enc = Encoder::new(w, header)?;
    merged.for_each(|pgno, data| enc.write_page(pgno, data))?;
    let (w, trailer) = enc.finish()?;
    Ok((w, header, trailer))
}

/// [`compact`] into memory.
pub fn compact_to_vec(files: &[Ltx<'_>]) -> Result<Vec<u8>> {
    Ok(compact(files, Vec::new())?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encode;

    fn page(fill: u8) -> Vec<u8> {
        vec![fill; 512]
    }

    fn file(min: u64, max: u64, commit: u32, pages: &[(u32, u8)]) -> Vec<u8> {
        let header = Header::new(512, commit, Txid(min), Txid(max), min as i64);
        let data: Vec<(u32, Vec<u8>)> = pages.iter().map(|&(p, f)| (p, page(f))).collect();
        encode(header, data.iter().map(|(p, d)| (*p, d.as_slice()))).unwrap()
    }

    fn contents(bytes: &[u8]) -> (Header, Vec<(u32, u8)>) {
        let ltx = Ltx::decode(bytes).unwrap();
        let pages = ltx
            .iter_pages()
            .map(|r| {
                let (p, d) = r.unwrap();
                assert!(d.iter().all(|&b| b == d[0]));
                (p, d[0])
            })
            .collect();
        (*ltx.header(), pages)
    }

    #[test]
    fn newest_page_wins() {
        let a = file(1, 1, 3, &[(1, 1), (2, 1), (3, 1)]);
        let b = file(2, 2, 3, &[(2, 2)]);
        let c = file(3, 4, 4, &[(1, 3), (4, 3)]);
        let inputs = [&a, &b, &c].map(|b| Ltx::decode(b).unwrap());
        let (h, pages) = contents(&compact_to_vec(&inputs).unwrap());
        assert_eq!((h.min_txid, h.max_txid, h.commit), (Txid(1), Txid(4), 4));
        assert_eq!(h.timestamp, 3);
        assert_eq!(pages, vec![(1, 3), (2, 2), (3, 1), (4, 3)]);
    }

    #[test]
    fn a_shrink_drops_pages_and_regrowth_reads_as_zeros() {
        let a = file(5, 5, 4, &[(3, 1), (4, 1)]);
        let b = file(6, 6, 2, &[(1, 2)]);
        let c = file(7, 7, 4, &[(3, 3)]);
        let inputs = [&a, &b, &c].map(|b| Ltx::decode(b).unwrap());
        let (h, pages) = contents(&compact_to_vec(&inputs).unwrap());
        assert_eq!(h.commit, 4);
        assert_eq!(pages, vec![(1, 2), (3, 3), (4, 0)]);
    }

    #[test]
    fn gaps_and_page_size_changes_are_refused() {
        let a = file(1, 1, 1, &[(1, 1)]);
        let gap = file(3, 3, 1, &[(1, 2)]);
        let inputs = [&a, &gap].map(|b| Ltx::decode(b).unwrap());
        assert_eq!(
            compact_to_vec(&inputs).unwrap_err().kind(),
            crate::ErrorKind::Discontiguous
        );
        let other = encode(
            Header::new(1024, 1, Txid(2), Txid(2), 0),
            [(1, &[0u8; 1024][..])],
        )
        .unwrap();
        let inputs = [&a, &other].map(|b| Ltx::decode(b).unwrap());
        assert!(compact_to_vec(&inputs).is_err());
    }

    #[test]
    fn overlapping_ranges_are_allowed() {
        let a = file(1, 3, 1, &[(1, 1)]);
        let b = file(2, 5, 1, &[(1, 2)]);
        let inputs = [&a, &b].map(|b| Ltx::decode(b).unwrap());
        let (h, pages) = contents(&compact_to_vec(&inputs).unwrap());
        assert_eq!((h.min_txid, h.max_txid), (Txid(1), Txid(5)));
        assert_eq!(pages, vec![(1, 2)]);
    }

    #[test]
    fn growth_without_a_write_still_makes_a_whole_snapshot() {
        let s = file(1, 1, 1, &[(1, 1)]);
        // The database grows to two pages, but page 2 is never written.
        let b = file(2, 2, 2, &[(1, 2)]);
        let inputs = [&s, &b].map(|b| Ltx::decode(b).unwrap());
        let (h, pages) = contents(&compact_to_vec(&inputs).unwrap());
        assert!(h.is_snapshot());
        assert_eq!(pages, vec![(1, 2), (2, 0)]);
    }
}
