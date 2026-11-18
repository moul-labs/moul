//! Where things live in a replica, and Moul's own records beside the LTX
//! files.
//!
//! ```text
//! ltx/0/0000000000000001-0000000000000001.ltx   one per TXID
//! ltx/1/…, ltx/2/…                              compacted ranges
//! ltx/9/0000000000000001-00000000000000a4.ltx   snapshots
//! moul/head.json                                newest TXID, for cheap polling
//! moul/writer.json                              which device writes
//! ```
//!
//! Litestream reads `ltx/` and ignores `moul/`.

use moul_ltx::{FileInfo, Txid, file_name, parse_file_name};
use serde::{Deserialize, Serialize};

use crate::replica::{Condition, ObjectInfo, Put, Replica};
use crate::{Error, Result};

/// The folder of LTX files.
pub const LTX_PREFIX: &str = "ltx/";
/// The head record.
pub const HEAD_KEY: &str = "moul/head.json";
/// The writer record.
pub const WRITER_KEY: &str = "moul/writer.json";

/// The key of an LTX file.
pub fn ltx_key(level: u8, min: Txid, max: Txid) -> String {
    format!("ltx/{level}/{}", file_name(min, max))
}

/// Reads `ltx/<level>/<min>-<max>.ltx`.
pub fn parse_ltx_key(key: &str) -> Option<(u8, Txid, Txid)> {
    let rest = key.strip_prefix(LTX_PREFIX)?;
    let (level, name) = rest.split_once('/')?;
    let level: u8 = level.parse().ok().filter(|l| *l <= 9)?;
    let (min, max) = parse_file_name(name)?;
    Some((level, min, max))
}

/// The LTX files in a listing.
pub fn file_infos(objects: &[ObjectInfo]) -> Vec<FileInfo> {
    objects
        .iter()
        .filter_map(|o| {
            let (level, min, max) = parse_ltx_key(&o.key)?;
            Some(FileInfo {
                level,
                min_txid: min,
                max_txid: max,
                size: o.size,
                created_at: o.modified,
            })
        })
        .collect()
}

/// Lists every LTX file in `replica`.
pub async fn list_files(replica: &dyn Replica) -> Result<Vec<FileInfo>> {
    Ok(file_infos(&replica.list(LTX_PREFIX).await?))
}

/// Which device writes, and since when. Changed only with a compare-and-swap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriterRecord {
    /// The writing device's ID.
    pub device: String,
    /// A name for people, such as "Laptop".
    #[serde(default)]
    pub label: String,
    /// Counts hand-offs: each new writer adds one.
    pub generation: u64,
    /// The TXID the device had when it took over.
    pub since: u64,
    /// The replica's ID, chosen when it was created. A device that holds a
    /// different one is new to this replica.
    pub replica: String,
    /// When the record was written, in milliseconds since the epoch.
    pub at: i64,
}

/// The newest TXID, written by the writer after each push.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Head {
    /// The newest TXID in the replica.
    pub txid: u64,
    /// The device that pushed it.
    pub writer: String,
    /// That device's writer generation.
    pub generation: u64,
    /// The replica's ID.
    pub replica: String,
    /// When it was pushed, in milliseconds since the epoch.
    pub at: i64,
}

/// Reads and parses a JSON record, with its entity tag.
pub(crate) async fn get_json<T: for<'de> Deserialize<'de>>(
    replica: &dyn Replica,
    key: &str,
) -> Result<Option<(T, String)>> {
    let Some(obj) = replica.get(key).await? else {
        return Ok(None);
    };
    let value = serde_json::from_slice(&obj.bytes).map_err(|e| {
        Error::replica(format!("{key} in {} is not valid: {e}", replica.describe()))
    })?;
    Ok(Some((value, obj.etag)))
}

/// Writes a JSON record if `condition` holds.
pub(crate) async fn put_json<T: Serialize>(
    replica: &dyn Replica,
    key: &str,
    value: &T,
    condition: Condition,
) -> Result<Put> {
    let bytes = serde_json::to_vec_pretty(value).expect("records serialize");
    replica.put(key, &bytes, condition).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let k = ltx_key(9, Txid(1), Txid(0xa4));
        assert_eq!(k, "ltx/9/0000000000000001-00000000000000a4.ltx");
        assert_eq!(parse_ltx_key(&k), Some((9, Txid(1), Txid(0xa4))));
        assert_eq!(
            parse_ltx_key("ltx/10/0000000000000001-0000000000000001.ltx"),
            None
        );
        assert_eq!(parse_ltx_key("moul/head.json"), None);
    }
}
