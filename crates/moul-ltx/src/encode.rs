//! Writing LTX files.

use std::io::Write;

use crate::crc64::Crc64;
use crate::format::{CHECKSUM_FLAG, Checksum, HEADER_SIZE, Header, PAGE_HEADER_SIZE, Trailer};
use crate::{Error, Result, lz4};

/// Writes one LTX file, page by page, to any [`Write`].
///
/// Pages go in ascending page number order. A snapshot (a file whose
/// `min_txid` is 1) must hold every page from 1 to `commit` except the lock
/// page, in order. Nothing is buffered beyond the current page, so a snapshot
/// of a large database streams.
///
/// ```
/// use moul_ltx::{Encoder, Header, Txid, Ltx};
///
/// let header = Header::new(512, 2, Txid(1), Txid(1), 0);
/// let mut enc = Encoder::new(Vec::new(), header).unwrap();
/// enc.write_page(1, &[1; 512]).unwrap();
/// enc.write_page(2, &[2; 512]).unwrap();
/// let (bytes, _trailer) = enc.finish().unwrap();
///
/// let file = Ltx::decode(&bytes).unwrap();
/// assert_eq!(file.page_count(), 2);
/// ```
pub struct Encoder<W: Write> {
    w: W,
    header: Header,
    hash: Crc64,
    /// Bytes written so far.
    n: u64,
    /// (pgno, offset, size) of each page frame.
    index: Vec<(u32, u64, u64)>,
    prev_pgno: u32,
    post_apply_checksum: Checksum,
    scratch: Vec<u8>,
}

impl<W: Write> std::fmt::Debug for Encoder<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("header", &self.header)
            .field("pages", &self.index.len())
            .finish_non_exhaustive()
    }
}

impl<W: Write> Encoder<W> {
    /// Validates `header` and writes it.
    pub fn new(mut w: W, header: Header) -> Result<Self> {
        header.validate()?;
        let bytes = header.to_bytes();
        w.write_all(&bytes)?;
        let mut hash = Crc64::new();
        hash.update(&bytes);
        Ok(Self {
            w,
            header,
            hash,
            n: HEADER_SIZE as u64,
            index: Vec::new(),
            prev_pgno: 0,
            post_apply_checksum: Checksum(0),
            scratch: Vec::new(),
        })
    }

    /// The header this file was started with.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Sets the trailer's post-apply checksum. Only for files that track
    /// checksums; leave it unset for files with [`FLAG_NO_CHECKSUM`](crate::FLAG_NO_CHECKSUM).
    pub fn set_post_apply_checksum(&mut self, sum: Checksum) {
        self.post_apply_checksum = sum;
    }

    /// Writes page `pgno`.
    pub fn write_page(&mut self, pgno: u32, data: &[u8]) -> Result<()> {
        let h = &self.header;
        if pgno == 0 {
            return Err(Error::invalid("page numbers start at 1"));
        }
        if pgno > h.commit {
            return Err(Error::invalid(format!(
                "page {pgno} is past the database size of {} pages",
                h.commit
            )));
        }
        if data.len() != h.page_size as usize {
            return Err(Error::invalid(format!(
                "page {pgno} is {} bytes, expected {}",
                data.len(),
                h.page_size
            )));
        }
        let lock = h.lock_pgno();
        if pgno == lock {
            return Err(Error::invalid(format!("page {pgno} is the lock page")));
        }
        if h.is_snapshot() {
            let expected = match self.prev_pgno {
                p if p + 1 == lock => p + 2,
                p => p + 1,
            };
            if pgno != expected {
                return Err(Error::invalid(format!(
                    "a snapshot must hold every page in order: expected page {expected}, got {pgno}"
                )));
            }
        } else if pgno <= self.prev_pgno {
            return Err(Error::invalid(format!(
                "pages out of order: {} then {pgno}",
                self.prev_pgno
            )));
        }

        let offset = self.n;
        let mut head = [0u8; PAGE_HEADER_SIZE];
        head[..4].copy_from_slice(&pgno.to_be_bytes());
        self.w.write_all(&head)?;
        self.hash.update(&head);
        self.scratch.clear();
        lz4::compress_frame(data, &mut self.scratch);
        self.w.write_all(&self.scratch)?;
        // The checksum covers the page as it is, not as it is stored.
        self.hash.update(data);
        self.n += (PAGE_HEADER_SIZE + self.scratch.len()) as u64;
        self.index.push((pgno, offset, self.n - offset));
        self.prev_pgno = pgno;
        Ok(())
    }

    fn put(&mut self, b: &[u8]) -> Result<()> {
        self.w.write_all(b)?;
        self.hash.update(b);
        self.n += b.len() as u64;
        Ok(())
    }

    /// Writes the end marker, page index and trailer, and returns the writer
    /// and the trailer.
    pub fn finish(mut self) -> Result<(W, Trailer)> {
        let h = self.header;
        if h.is_snapshot() {
            let lock = h.lock_pgno();
            let last = if h.commit == lock {
                h.commit - 1
            } else {
                h.commit
            };
            if self.prev_pgno != last {
                return Err(Error::invalid(format!(
                    "a snapshot of {} pages ends at page {}",
                    h.commit, self.prev_pgno
                )));
            }
        }
        self.put(&[0u8; PAGE_HEADER_SIZE])?;

        let start = self.n;
        let mut buf = Vec::with_capacity(self.index.len() * 8 + 1);
        for &(pgno, offset, size) in &self.index {
            put_uvarint(&mut buf, pgno as u64);
            put_uvarint(&mut buf, offset);
            put_uvarint(&mut buf, size);
        }
        put_uvarint(&mut buf, 0);
        self.put(&buf)?;
        let index_size = self.n - start;
        self.put(&index_size.to_be_bytes())?;

        let post = self.post_apply_checksum;
        self.put(&post.0.to_be_bytes())?;
        let trailer = Trailer {
            post_apply_checksum: post,
            file_checksum: Checksum(CHECKSUM_FLAG | self.hash.finish()),
        };
        trailer
            .validate(&h)
            .map_err(|e| Error::invalid(e.message().to_owned()))?;
        self.w.write_all(&trailer.file_checksum.0.to_be_bytes())?;
        self.w.flush()?;
        Ok((self.w, trailer))
    }
}

pub(crate) fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Encodes a whole file in memory from `pages`, given in page number order.
pub fn encode<'a>(
    header: Header,
    pages: impl IntoIterator<Item = (u32, &'a [u8])>,
) -> Result<Vec<u8>> {
    let mut enc = Encoder::new(Vec::new(), header)?;
    for (pgno, data) in pages {
        enc.write_page(pgno, data)?;
    }
    Ok(enc.finish()?.0)
}
