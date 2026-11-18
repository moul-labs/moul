//! Reading LTX files.

use crate::crc64::Crc64;
use crate::format::{
    CHECKSUM_FLAG, Checksum, HEADER_SIZE, Header, PAGE_HEADER_SIZE, TRAILER_SIZE, Trailer,
    page_checksum,
};
use crate::{Error, Result, lz4};

/// Where one page sits in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageEntry {
    /// Its page number.
    pub pgno: u32,
    /// Offset of its page header from the start of the file.
    pub offset: u64,
    /// Bytes of page header and compressed frame.
    pub size: u64,
}

/// A verified LTX file in memory.
///
/// [`decode`](Self::decode) reads the whole file once: it checks the header,
/// every page frame, the page index and the file checksum. Pages are then
/// decompressed again on demand, so holding an `Ltx` costs no more memory
/// than the bytes themselves.
#[derive(Clone, Debug)]
pub struct Ltx<'a> {
    bytes: &'a [u8],
    header: Header,
    trailer: Trailer,
    pages: Vec<PageEntry>,
}

fn read_uvarint(b: &[u8], at: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b
            .get(*at)
            .ok_or_else(|| Error::corrupt("page index ends inside a number"))?;
        *at += 1;
        if shift == 63 && byte > 1 {
            break;
        }
        v |= ((byte & 0x7F) as u64) << shift;
        if byte < 0x80 {
            return Ok(v);
        }
    }
    Err(Error::corrupt("page index number overflows"))
}

impl<'a> Ltx<'a> {
    /// Reads and verifies `bytes` as one LTX file.
    pub fn decode(bytes: &'a [u8]) -> Result<Self> {
        let header = Header::from_bytes(bytes)?;
        header
            .validate()
            .map_err(|e| Error::corrupt(format!("bad LTX header: {e}")))?;
        let ps = header.page_size as usize;
        let lock = header.lock_pgno();
        let corrupt = |what: String| {
            Error::corrupt(format!(
                "LTX {}-{}: {what}",
                header.min_txid, header.max_txid
            ))
        };

        let mut hash = Crc64::new();
        hash.update(&bytes[..HEADER_SIZE]);
        let mut at = HEADER_SIZE;
        let mut pages = Vec::new();
        let mut page = Vec::with_capacity(ps);
        let mut prev = 0u32;
        let mut rolling = Checksum(CHECKSUM_FLAG);
        loop {
            let head = bytes
                .get(at..at + PAGE_HEADER_SIZE)
                .ok_or_else(|| corrupt("file ends inside the page block".into()))?;
            hash.update(head);
            let pgno = u32::from_be_bytes(head[..4].try_into().expect("4 bytes"));
            let flags = u16::from_be_bytes([head[4], head[5]]);
            if pgno == 0 && flags == 0 {
                at += PAGE_HEADER_SIZE;
                break;
            }
            if pgno == 0 || flags != 0 {
                return Err(corrupt(format!("bad page header at offset {at}")));
            }
            if pgno <= prev {
                return Err(corrupt(format!("pages out of order: {prev} then {pgno}")));
            }
            if pgno > header.commit {
                return Err(corrupt(format!(
                    "page {pgno} is past the database size of {} pages",
                    header.commit
                )));
            }
            if pgno == lock {
                return Err(corrupt(format!("holds the lock page {pgno}")));
            }
            if header.is_snapshot() {
                let expected = if prev + 1 == lock { prev + 2 } else { prev + 1 };
                if pgno != expected {
                    return Err(corrupt(format!(
                        "snapshot skips from page {prev} to {pgno}"
                    )));
                }
            }
            let frame_at = at + PAGE_HEADER_SIZE;
            let n = lz4::decompress_frame(&bytes[frame_at..], &mut page, ps)
                .map_err(|e| corrupt(format!("page {pgno}: {e}")))?;
            if page.len() != ps {
                return Err(corrupt(format!(
                    "page {pgno} holds {} bytes, expected {ps}",
                    page.len()
                )));
            }
            hash.update(&page);
            if header.is_snapshot() && !header.no_checksum() {
                rolling = rolling.xor(page_checksum(pgno, &page));
            }
            let size = (PAGE_HEADER_SIZE + n) as u64;
            pages.push(PageEntry {
                pgno,
                offset: at as u64,
                size,
            });
            at += size as usize;
            prev = pgno;
        }
        if header.is_snapshot() {
            let last = if header.commit == lock {
                header.commit - 1
            } else {
                header.commit
            };
            if prev != last {
                return Err(corrupt(format!(
                    "snapshot of {} pages ends at page {prev}",
                    header.commit
                )));
            }
        }

        // Page index, its size, then the trailer.
        if bytes.len() < at + 8 + TRAILER_SIZE {
            return Err(corrupt("file ends before its page index".into()));
        }
        let index_end = bytes.len() - TRAILER_SIZE - 8;
        let index_size =
            u64::from_be_bytes(bytes[index_end..index_end + 8].try_into().expect("8 bytes"));
        if index_size != (index_end - at) as u64 {
            return Err(corrupt(format!(
                "page index is {} bytes but says {index_size}",
                index_end - at
            )));
        }
        let mut i = at;
        for entry in &pages {
            let pgno = read_uvarint(bytes, &mut i)?;
            let offset = read_uvarint(bytes, &mut i)?;
            let size = read_uvarint(bytes, &mut i)?;
            if (pgno, offset, size) != (entry.pgno as u64, entry.offset, entry.size) {
                return Err(corrupt(format!(
                    "page index entry for page {pgno} does not match the page block"
                )));
            }
        }
        if read_uvarint(bytes, &mut i)? != 0 || i != index_end {
            return Err(corrupt("page index does not end where it should".into()));
        }
        let tail = &bytes[at..bytes.len() - 8];
        hash.update(tail);

        let trailer_at = bytes.len() - TRAILER_SIZE;
        let u64_at = |i: usize| u64::from_be_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
        let trailer = Trailer {
            post_apply_checksum: Checksum(u64_at(trailer_at)),
            file_checksum: Checksum(u64_at(trailer_at + 8)),
        };
        trailer.validate(&header)?;
        if Checksum(CHECKSUM_FLAG | hash.finish()) != trailer.file_checksum {
            return Err(corrupt("file checksum mismatch".into()));
        }
        if header.is_snapshot() && !header.no_checksum() && rolling != trailer.post_apply_checksum {
            return Err(corrupt(format!(
                "snapshot checksum is {rolling} but the trailer says {}",
                trailer.post_apply_checksum
            )));
        }
        Ok(Self {
            bytes,
            header,
            trailer,
            pages,
        })
    }

    /// The header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The trailer.
    pub fn trailer(&self) -> &Trailer {
        &self.trailer
    }

    /// The file's bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// How many pages the file holds.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The pages the file holds, in page number order.
    pub fn pages(&self) -> &[PageEntry] {
        &self.pages
    }

    /// The entry for page `pgno`, if the file holds it.
    pub fn find(&self, pgno: u32) -> Option<&PageEntry> {
        self.pages
            .binary_search_by_key(&pgno, |e| e.pgno)
            .ok()
            .map(|i| &self.pages[i])
    }

    /// Decompresses the page at `entry` into `buf`, which must be one page
    /// long.
    pub fn read_page(&self, entry: &PageEntry, buf: &mut Vec<u8>) -> Result<()> {
        let at = entry.offset as usize + PAGE_HEADER_SIZE;
        let end = (entry.offset + entry.size) as usize;
        let frame = self
            .bytes
            .get(at..end)
            .ok_or_else(|| Error::corrupt("page entry points past the file"))?;
        lz4::decompress_frame(frame, buf, self.header.page_size as usize)?;
        Ok(())
    }

    /// Every page, decompressed, in page number order.
    pub fn iter_pages(&self) -> impl Iterator<Item = Result<(u32, Vec<u8>)>> + '_ {
        self.pages.iter().map(|e| {
            let mut buf = Vec::with_capacity(self.header.page_size as usize);
            self.read_page(e, &mut buf).map(|()| (e.pgno, buf))
        })
    }
}
