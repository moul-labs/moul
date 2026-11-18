//! The fixed parts of an LTX v3 file: header, page headers, trailer, names.
//!
//! ```text
//! header        100 bytes
//! page frames   (page header: pgno u32, flags u16) (one LZ4 frame), ...
//! end marker    6 zero bytes, a page header for page 0
//! page index    (uvarint pgno, uvarint offset, uvarint size) ..., uvarint 0
//! index size    u64: bytes in the page index, end marker of the index included
//! trailer       post-apply checksum u64, file checksum u64
//! ```
//!
//! Integers are big-endian. The file checksum is CRC-64/ISO over everything
//! before it, with each page counted *uncompressed*, and its top bit set.
//! This matches `github.com/superfly/ltx` v0.5, which Litestream 0.5 uses.

use std::fmt;
use std::str::FromStr;

use crate::crc64::Crc64;
use crate::{Error, Result};

/// The first four bytes of every LTX file.
pub const MAGIC: &[u8; 4] = b"LTX1";
/// The format version this crate reads and writes. The magic is the same for
/// every version, so it is implied, not stored.
pub const VERSION: u32 = 3;
/// Bytes in the header.
pub const HEADER_SIZE: usize = 100;
/// Bytes in a page header.
pub const PAGE_HEADER_SIZE: usize = 6;
/// Bytes in the trailer.
pub const TRAILER_SIZE: usize = 16;
/// The largest SQLite page size.
pub const MAX_PAGE_SIZE: u32 = 65_536;

/// Header flag: the file tracks no database checksums, so the pre-apply and
/// post-apply checksums are zero. Litestream 0.5 sets it on every file.
pub const FLAG_NO_CHECKSUM: u32 = 1 << 1;
const FLAG_MASK: u32 = FLAG_NO_CHECKSUM;

/// The byte offset SQLite reserves for its locks: the page that holds it is
/// never written, and LTX files never carry it.
pub const PENDING_BYTE: u64 = 0x4000_0000;

/// The page that holds SQLite's lock bytes for `page_size`, the one page no
/// LTX file may contain.
pub fn lock_pgno(page_size: u32) -> u32 {
    (PENDING_BYTE / page_size as u64) as u32 + 1
}

/// Whether `size` is a SQLite page size: a power of two from 512 to 65,536.
pub fn is_valid_page_size(size: u32) -> bool {
    (512..=MAX_PAGE_SIZE).contains(&size) && size.is_power_of_two()
}

/// A transaction ID: the position of a database in its replica.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Txid(pub u64);

impl fmt::Display for Txid {
    /// Sixteen lower-case hex digits, as in file names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for Txid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::invalid(format!("{s:?} is not a 16-digit TXID")));
        }
        u64::from_str_radix(s, 16)
            .map(Txid)
            .map_err(|e| Error::invalid(format!("{s:?} is not a TXID: {e}")))
    }
}

/// A database checksum, as LTX tracks them when [`FLAG_NO_CHECKSUM`] is off:
/// an XOR of [`page_checksum`]s with the top bit set, or zero for none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Checksum(pub u64);

/// The top bit every non-zero checksum has set.
pub const CHECKSUM_FLAG: u64 = 1 << 63;

impl Checksum {
    /// Adds or removes a page's checksum from a rolling database checksum.
    pub fn xor(self, page: Checksum) -> Checksum {
        Checksum(CHECKSUM_FLAG | (self.0 ^ page.0))
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// The checksum of one page: CRC-64/ISO of its number and bytes.
pub fn page_checksum(pgno: u32, data: &[u8]) -> Checksum {
    let mut crc = Crc64::new();
    crc.update(&pgno.to_be_bytes());
    crc.update(data);
    Checksum(CHECKSUM_FLAG | crc.finish())
}

/// The header of an LTX file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// Flags; only [`FLAG_NO_CHECKSUM`] is defined.
    pub flags: u32,
    /// Page size in bytes.
    pub page_size: u32,
    /// The database's size in pages after the file is applied.
    pub commit: u32,
    /// The first transaction the file holds.
    pub min_txid: Txid,
    /// The last transaction the file holds.
    pub max_txid: Txid,
    /// When the file was written, in milliseconds since the Unix epoch.
    pub timestamp: i64,
    /// The database checksum before the file is applied.
    pub pre_apply_checksum: Checksum,
    /// Offset of the WAL segment the file came from; zero if none.
    pub wal_offset: i64,
    /// Size of that WAL segment; zero if none.
    pub wal_size: i64,
    /// The WAL header's first salt; zero if none.
    pub wal_salt1: u32,
    /// The WAL header's second salt; zero if none.
    pub wal_salt2: u32,
    /// The node that wrote the file; zero if unset.
    pub node_id: u64,
}

impl Header {
    /// A header for a file of transactions `min..=max` with no checksums, as
    /// Litestream writes them.
    pub fn new(page_size: u32, commit: u32, min: Txid, max: Txid, timestamp: i64) -> Self {
        Self {
            flags: FLAG_NO_CHECKSUM,
            page_size,
            commit,
            min_txid: min,
            max_txid: max,
            timestamp,
            ..Self::default()
        }
    }

    /// A file that starts at TXID 1 holds the whole database: every page from
    /// 1 to `commit`, but the lock page.
    pub fn is_snapshot(&self) -> bool {
        self.min_txid == Txid(1)
    }

    /// Whether the file tracks no database checksums.
    pub fn no_checksum(&self) -> bool {
        self.flags & FLAG_NO_CHECKSUM != 0
    }

    /// The page that holds SQLite's lock bytes at this page size.
    pub fn lock_pgno(&self) -> u32 {
        lock_pgno(self.page_size)
    }

    /// Checks the header the way the Go library does before writing or
    /// after reading one.
    pub fn validate(&self) -> Result<()> {
        if self.flags & !FLAG_MASK != 0 {
            return Err(Error::invalid(format!(
                "unknown header flags {:#010x}",
                self.flags
            )));
        }
        if !is_valid_page_size(self.page_size) {
            return Err(Error::invalid(format!(
                "invalid page size {}",
                self.page_size
            )));
        }
        if self.min_txid.0 == 0 || self.max_txid.0 == 0 {
            return Err(Error::invalid("transaction IDs start at 1"));
        }
        if self.min_txid > self.max_txid {
            return Err(Error::invalid(format!(
                "transaction IDs out of order: {}..{}",
                self.min_txid, self.max_txid
            )));
        }
        if self.wal_offset < 0 || self.wal_size < 0 {
            return Err(Error::invalid("WAL offset and size cannot be negative"));
        }
        if (self.wal_salt1 != 0 || self.wal_salt2 != 0) && self.wal_offset == 0 {
            return Err(Error::invalid("a WAL salt needs a WAL offset"));
        }
        if self.wal_offset == 0 && self.wal_size != 0 {
            return Err(Error::invalid("a WAL size needs a WAL offset"));
        }
        let pre = self.pre_apply_checksum.0;
        if self.is_snapshot() || self.no_checksum() {
            if pre != 0 {
                return Err(Error::invalid(
                    "a snapshot, or a file without checksums, has no pre-apply checksum",
                ));
            }
        } else if pre & CHECKSUM_FLAG == 0 {
            return Err(Error::invalid("pre-apply checksum is missing or malformed"));
        }
        Ok(())
    }

    /// The 100 bytes of the header.
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..4].copy_from_slice(MAGIC);
        b[4..8].copy_from_slice(&self.flags.to_be_bytes());
        b[8..12].copy_from_slice(&self.page_size.to_be_bytes());
        b[12..16].copy_from_slice(&self.commit.to_be_bytes());
        b[16..24].copy_from_slice(&self.min_txid.0.to_be_bytes());
        b[24..32].copy_from_slice(&self.max_txid.0.to_be_bytes());
        b[32..40].copy_from_slice(&self.timestamp.to_be_bytes());
        b[40..48].copy_from_slice(&self.pre_apply_checksum.0.to_be_bytes());
        b[48..56].copy_from_slice(&self.wal_offset.to_be_bytes());
        b[56..64].copy_from_slice(&self.wal_size.to_be_bytes());
        b[64..68].copy_from_slice(&self.wal_salt1.to_be_bytes());
        b[68..72].copy_from_slice(&self.wal_salt2.to_be_bytes());
        b[72..80].copy_from_slice(&self.node_id.to_be_bytes());
        b
    }

    /// Reads a header from the first 100 bytes of `b`, without validating it.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        if b.len() < HEADER_SIZE {
            return Err(Error::corrupt("too short for an LTX header"));
        }
        if &b[0..4] != MAGIC {
            return Err(Error::corrupt("not an LTX file: bad magic"));
        }
        let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().expect("4 bytes"));
        let u64_at = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().expect("8 bytes"));
        Ok(Self {
            flags: u32_at(4),
            page_size: u32_at(8),
            commit: u32_at(12),
            min_txid: Txid(u64_at(16)),
            max_txid: Txid(u64_at(24)),
            timestamp: u64_at(32) as i64,
            pre_apply_checksum: Checksum(u64_at(40)),
            wal_offset: u64_at(48) as i64,
            wal_size: u64_at(56) as i64,
            wal_salt1: u32_at(64),
            wal_salt2: u32_at(68),
            node_id: u64_at(72),
        })
    }
}

/// The trailer of an LTX file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trailer {
    /// The database checksum after the file is applied; zero without checksums.
    pub post_apply_checksum: Checksum,
    /// CRC-64/ISO of the file, with the top bit set.
    pub file_checksum: Checksum,
}

impl Trailer {
    pub(crate) fn validate(&self, header: &Header) -> Result<()> {
        let post = self.post_apply_checksum.0;
        if header.no_checksum() {
            if post != 0 {
                return Err(Error::corrupt(
                    "post-apply checksum set on a file without checksums",
                ));
            }
        } else if post & CHECKSUM_FLAG == 0 {
            return Err(Error::corrupt(
                "post-apply checksum is missing or malformed",
            ));
        }
        if self.file_checksum.0 & CHECKSUM_FLAG == 0 {
            return Err(Error::corrupt("file checksum is missing or malformed"));
        }
        Ok(())
    }
}

/// The name of the file holding transactions `min..=max`:
/// `0000000000000001-0000000000000005.ltx`.
pub fn file_name(min: Txid, max: Txid) -> String {
    format!("{min}-{max}.ltx")
}

/// The TXID range a file name stands for, or `None` if it is not an LTX name.
pub fn parse_file_name(name: &str) -> Option<(Txid, Txid)> {
    let stem = name.strip_suffix(".ltx")?;
    let (min, max) = stem.split_once('-')?;
    let lower = |s: &str| !s.bytes().any(|b| b.is_ascii_uppercase());
    if !lower(min) || !lower(max) {
        return None;
    }
    Some((min.parse().ok()?, max.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = Header {
            flags: 0,
            page_size: 4096,
            commit: 12,
            min_txid: Txid(3),
            max_txid: Txid(9),
            timestamp: 1_700_000_000_123,
            pre_apply_checksum: Checksum(CHECKSUM_FLAG | 5),
            wal_offset: 32,
            wal_size: 4128,
            wal_salt1: 7,
            wal_salt2: 8,
            node_id: 99,
        };
        h.validate().unwrap();
        assert_eq!(Header::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn validation_follows_the_go_rules() {
        let ok = Header::new(4096, 1, Txid(1), Txid(1), 0);
        ok.validate().unwrap();
        for bad in [
            Header {
                page_size: 1000,
                ..ok
            },
            Header { flags: 1, ..ok },
            Header {
                min_txid: Txid(0),
                ..ok
            },
            Header {
                min_txid: Txid(2),
                ..ok
            },
            Header {
                pre_apply_checksum: Checksum(CHECKSUM_FLAG),
                ..ok
            },
            Header { wal_size: 5, ..ok },
            Header {
                flags: 0,
                min_txid: Txid(2),
                max_txid: Txid(2),
                ..ok
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lock_page_numbers() {
        assert_eq!(lock_pgno(512), 2_097_153);
        assert_eq!(lock_pgno(4096), 262_145);
        assert_eq!(lock_pgno(65_536), 16_385);
    }

    #[test]
    fn file_names() {
        let name = file_name(Txid(1), Txid(0xa4));
        assert_eq!(name, "0000000000000001-00000000000000a4.ltx");
        assert_eq!(parse_file_name(&name), Some((Txid(1), Txid(0xa4))));
        assert_eq!(
            parse_file_name("0000000000000001-00000000000000A4.ltx"),
            None
        );
        assert_eq!(parse_file_name("1-2.ltx"), None);
        assert_eq!(
            parse_file_name("0000000000000001-0000000000000002.ltx.tmp"),
            None
        );
    }

    #[test]
    fn page_checksum_matches_go() {
        // ltx.ChecksumPage(1, make([]byte, 512)) in Go.
        assert_eq!(page_checksum(1, &[0u8; 512]), Checksum(0xefb1f44fecd99000));
    }
}
