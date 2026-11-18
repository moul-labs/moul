//! The commit log: the file whose flush is the commit point.
//!
//! ```text
//! offset 0     header slot A (64 bytes)
//! offset 64    header slot B (64 bytes)
//! offset 128   record, record, ...
//! ```
//!
//! A header slot holds a generation and the highest commit sequence number
//! already written into the database files. The slot with the higher valid
//! generation wins, and updates go to the other slot, so a torn header write
//! leaves the previous one readable.
//!
//! ```text
//! slot:   magic "MOULLOG1" | version u32 | reserved u32 | generation u64
//!         | applied u64 | reserved (24 bytes) | crc64 of bytes 0..56
//! record: magic "MREC" | body length u32 | seq u64 | flags u32 | file count u32
//!         | files ... | crc64 of everything before it
//! file:   name length u16 | name | page size u32 | truncated to (pages) u32
//!         | size (pages) u32 | page count u32 | (page number u32, page) ...
//! ```
//!
//! Integers are big-endian, as in LTX. A record whose checksum does not match
//! was cut short by a crash before its flush returned, so it never committed.

use std::collections::BTreeMap;

use crate::crc64::Crc64;
use crate::storage::{StorageFile, read_exact_at};
use crate::{Error, Result};

pub(crate) const HEADER_LEN: u64 = 128;
const SLOT_LEN: usize = 64;
const SLOT_MAGIC: &[u8; 8] = b"MOULLOG1";
const RECORD_MAGIC: &[u8; 4] = b"MREC";
const VERSION: u32 = 1;
const RECORD_HEAD: usize = 24;
const TRAILER: usize = 8;

/// A local commit number. It counts commits on this device and is not a
/// replica transaction ID: LTX TXIDs are assigned when commits are packed for
/// the replica, which lets a device later step back commits the writer never
/// confirmed without leaving holes in the replica's numbering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitSeq(pub u64);

impl std::fmt::Display for CommitSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// What one commit did to one database file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCommit {
    /// The file's name in the volume, such as `main.db`.
    pub name: String,
    /// Page size in bytes.
    pub page_size: u32,
    /// The smallest size, in pages, the file had during the commit. Pages past
    /// it and not in `pages` are gone. Equal to the size before the commit when
    /// nothing was truncated.
    pub truncated_to: u32,
    /// The file's size in pages after the commit. This is LTX's `commit` field.
    pub size: u32,
    /// The pages the commit wrote, by page number from 1, in order.
    pub pages: BTreeMap<u32, Box<[u8]>>,
}

/// One durable commit, as the volume hands it to its observer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// Its local sequence number.
    pub seq: CommitSeq,
    /// The files it changed. A commit that spans stores lists each of them.
    pub files: Vec<FileCommit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub generation: u64,
    pub applied: u64,
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn encode_slot(h: Header) -> [u8; SLOT_LEN] {
    let mut out = Vec::with_capacity(SLOT_LEN);
    out.extend_from_slice(SLOT_MAGIC);
    put_u32(&mut out, VERSION);
    put_u32(&mut out, 0);
    put_u64(&mut out, h.generation);
    put_u64(&mut out, h.applied);
    out.resize(56, 0);
    let crc = Crc64::checksum(&out);
    put_u64(&mut out, crc);
    out.try_into().expect("slot is 64 bytes")
}

fn decode_slot(b: &[u8]) -> Option<Header> {
    if b.len() != SLOT_LEN || &b[..8] != SLOT_MAGIC {
        return None;
    }
    if Crc64::checksum(&b[..56]) != u64::from_be_bytes(b[56..64].try_into().ok()?) {
        return None;
    }
    if u32::from_be_bytes(b[8..12].try_into().ok()?) != VERSION {
        return None;
    }
    Some(Header {
        generation: u64::from_be_bytes(b[16..24].try_into().ok()?),
        applied: u64::from_be_bytes(b[24..32].try_into().ok()?),
    })
}

/// Reads the header. `Ok(None)` means there is no valid slot, which is what a
/// new log, or one whose first header write was torn, looks like.
pub(crate) fn read_header(file: &mut dyn StorageFile) -> Result<Option<(Header, usize)>> {
    let mut buf = [0u8; HEADER_LEN as usize];
    let n = file.read_at(&mut buf, 0)?;
    let slots = [&buf[..SLOT_LEN], &buf[SLOT_LEN..]];
    let mut best: Option<(Header, usize)> = None;
    for (i, slot) in slots.iter().enumerate() {
        if n < (i + 1) * SLOT_LEN {
            break;
        }
        if let Some(h) = decode_slot(slot)
            && best.is_none_or(|(b, _)| h.generation > b.generation)
        {
            best = Some((h, i));
        }
    }
    Ok(best)
}

/// Writes `h` into slot `slot` (0 or 1) and flushes.
pub(crate) fn write_header(file: &mut dyn StorageFile, slot: usize, h: Header) -> Result<()> {
    file.write_at(&encode_slot(h), (slot * SLOT_LEN) as u64)?;
    file.flush()
}

/// Encodes a record.
pub(crate) fn encode_record(seq: CommitSeq, files: &[FileCommit]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(RECORD_MAGIC);
    put_u32(&mut out, 0); // body length, filled in below
    put_u64(&mut out, seq.0);
    put_u32(&mut out, 0); // flags: none defined yet
    put_u32(&mut out, files.len() as u32);
    for f in files {
        let name = f.name.as_bytes();
        put_u16(&mut out, name.len() as u16);
        out.extend_from_slice(name);
        put_u32(&mut out, f.page_size);
        put_u32(&mut out, f.truncated_to);
        put_u32(&mut out, f.size);
        put_u32(&mut out, f.pages.len() as u32);
        for (&pgno, page) in &f.pages {
            if page.len() != f.page_size as usize {
                return Err(Error::invalid(format!(
                    "page {pgno} of {} is {} bytes, expected {}",
                    f.name,
                    page.len(),
                    f.page_size
                )));
            }
            put_u32(&mut out, pgno);
            out.extend_from_slice(page);
        }
    }
    let body = u32::try_from(out.len() - RECORD_HEAD)
        .map_err(|_| Error::invalid("commit is larger than 4 GiB"))?;
    out[4..8].copy_from_slice(&body.to_be_bytes());
    let crc = Crc64::checksum(&out);
    put_u64(&mut out, crc);
    Ok(out)
}

/// Reads the record at `offset`. `Ok(None)` means there is no complete, valid
/// record there: the log ends, or its last record was torn.
pub(crate) fn read_record(
    file: &mut dyn StorageFile,
    offset: u64,
    file_len: u64,
) -> Result<Option<(Commit, u64)>> {
    if offset + (RECORD_HEAD + TRAILER) as u64 > file_len {
        return Ok(None);
    }
    let mut head = [0u8; RECORD_HEAD];
    read_exact_at(file, &mut head, offset)?;
    if &head[..4] != RECORD_MAGIC {
        return Ok(None);
    }
    let body = u32::from_be_bytes(head[4..8].try_into().expect("4 bytes")) as u64;
    let total = RECORD_HEAD as u64 + body + TRAILER as u64;
    if offset + total > file_len {
        return Ok(None);
    }
    let mut buf = vec![0u8; total as usize];
    read_exact_at(file, &mut buf, offset)?;
    let (content, trailer) = buf.split_at(buf.len() - TRAILER);
    if Crc64::checksum(content) != u64::from_be_bytes(trailer.try_into().expect("8 bytes")) {
        return Ok(None);
    }
    // The checksum matched, so a parse failure past this point is not a torn
    // write but bytes this code did not write.
    let commit = parse_record(content).ok_or_else(|| {
        Error::corrupt(format!("commit log record at offset {offset} is malformed"))
    })?;
    Ok(Some((commit, total)))
}

fn parse_record(b: &[u8]) -> Option<Commit> {
    let mut r = Reader { b, at: 8 };
    let seq = CommitSeq(r.u64()?);
    let _flags = r.u32()?;
    let count = r.u32()?;
    let mut files = Vec::with_capacity(count.min(16) as usize);
    for _ in 0..count {
        let name_len = r.u16()? as usize;
        let name = std::str::from_utf8(r.take(name_len)?).ok()?.to_owned();
        let page_size = r.u32()?;
        let truncated_to = r.u32()?;
        let size = r.u32()?;
        let page_count = r.u32()?;
        let mut pages = BTreeMap::new();
        for _ in 0..page_count {
            let pgno = r.u32()?;
            let page = r.take(page_size as usize)?;
            pages.insert(pgno, page.into());
        }
        files.push(FileCommit {
            name,
            page_size,
            truncated_to,
            size,
            pages,
        });
    }
    (r.at == b.len()).then_some(Commit { seq, files })
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.b.get(self.at..end)?;
        self.at = end;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStorage;
    use crate::storage::Storage;

    fn sample() -> Vec<FileCommit> {
        let mut pages = BTreeMap::new();
        pages.insert(1, vec![7u8; 512].into_boxed_slice());
        pages.insert(3, vec![9u8; 512].into_boxed_slice());
        vec![
            FileCommit {
                name: "main.db".into(),
                page_size: 512,
                truncated_to: 2,
                size: 3,
                pages,
            },
            FileCommit {
                name: "writer.db".into(),
                page_size: 512,
                truncated_to: 0,
                size: 0,
                pages: BTreeMap::new(),
            },
        ]
    }

    #[test]
    fn record_round_trip() {
        let storage = MemoryStorage::new();
        let mut f = storage.open("log").unwrap();
        let rec = encode_record(CommitSeq(42), &sample()).unwrap();
        f.write_at(&rec, HEADER_LEN).unwrap();
        let len = f.size().unwrap();
        let (commit, n) = read_record(f.as_mut(), HEADER_LEN, len).unwrap().unwrap();
        assert_eq!(n, rec.len() as u64);
        assert_eq!(commit.seq, CommitSeq(42));
        assert_eq!(commit.files, sample());
    }

    #[test]
    fn a_torn_record_reads_as_absent() {
        let storage = MemoryStorage::new();
        let mut f = storage.open("log").unwrap();
        let rec = encode_record(CommitSeq(1), &sample()).unwrap();
        for cut in [10, rec.len() / 2, rec.len() - 1] {
            f.truncate(0).unwrap();
            f.write_at(&rec[..cut], HEADER_LEN).unwrap();
            let len = f.size().unwrap();
            assert!(read_record(f.as_mut(), HEADER_LEN, len).unwrap().is_none());
        }
        let mut flipped = rec.clone();
        flipped[100] ^= 1;
        f.write_at(&flipped, HEADER_LEN).unwrap();
        let len = f.size().unwrap();
        assert!(read_record(f.as_mut(), HEADER_LEN, len).unwrap().is_none());
    }

    #[test]
    fn newer_valid_header_slot_wins() {
        let storage = MemoryStorage::new();
        let mut f = storage.open("log").unwrap();
        assert!(read_header(f.as_mut()).unwrap().is_none());
        write_header(
            f.as_mut(),
            0,
            Header {
                generation: 1,
                applied: 5,
            },
        )
        .unwrap();
        write_header(
            f.as_mut(),
            1,
            Header {
                generation: 2,
                applied: 6,
            },
        )
        .unwrap();
        assert_eq!(read_header(f.as_mut()).unwrap().unwrap().0.applied, 6);
        // Tear slot B: slot A is still there.
        f.write_at(&[0xFF; 8], 64 + 20).unwrap();
        let (h, slot) = read_header(f.as_mut()).unwrap().unwrap();
        assert_eq!((h.applied, slot), (5, 0));
    }
}
