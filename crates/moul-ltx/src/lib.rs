#![doc = include_str!("../README.md")]

mod compact;
mod crc64;
mod decode;
mod encode;
mod error;
mod format;
pub mod lz4;
pub mod restore;

pub use compact::{compact, compact_to_vec, is_contiguous};
pub use crc64::Crc64;
pub use decode::{Ltx, PageEntry};
pub use encode::{Encoder, encode};
pub use error::{Error, ErrorKind, Result};
pub use format::{
    CHECKSUM_FLAG, Checksum, FLAG_NO_CHECKSUM, HEADER_SIZE, Header, MAGIC, MAX_PAGE_SIZE,
    PAGE_HEADER_SIZE, PENDING_BYTE, TRAILER_SIZE, Trailer, Txid, VERSION, file_name,
    is_valid_page_size, lock_pgno, page_checksum, parse_file_name,
};
pub use restore::{FileInfo, SNAPSHOT_LEVEL};
