//! A replica Litestream 0.5.13 wrote (`fixtures/litestream`), restored with
//! nothing but this crate. Its files come from Litestream's LZ4 encoder, so
//! this runs that check where Go is not installed. The compatibility suite in
//! `tools/litestream-compat` regenerates the fixture and checks it against
//! Litestream itself.

use std::fs;
use std::path::{Path, PathBuf};

use moul_ltx::{FileInfo, Ltx, Txid, parse_file_name, restore};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/litestream")
}

fn list(replica: &Path) -> Vec<FileInfo> {
    let mut out = Vec::new();
    for level in 0..=9u8 {
        let Ok(entries) = fs::read_dir(replica.join("ltx").join(level.to_string())) else {
            continue;
        };
        for e in entries {
            let e = e.unwrap();
            if let Some((min, max)) = e.file_name().to_str().and_then(parse_file_name) {
                out.push(FileInfo {
                    level,
                    min_txid: min,
                    max_txid: max,
                    size: e.metadata().unwrap().len(),
                    created_at: 0,
                });
            }
        }
    }
    out
}

fn restore_to(replica: &Path, target: Option<Txid>) -> Vec<u8> {
    let plan = restore::plan(&list(replica), target).unwrap();
    let files: Vec<Vec<u8>> = plan
        .iter()
        .map(|f| fs::read(replica.join(f.path())).unwrap())
        .collect();
    let decoded: Vec<Ltx<'_>> = files.iter().map(|b| Ltx::decode(b).unwrap()).collect();
    restore::restore_to_vec(&decoded).unwrap()
}

#[test]
fn restores_a_replica_litestream_wrote() {
    let replica = root().join("replica");
    let files = list(&replica);
    assert!(
        files.iter().any(|f| f.level == 9),
        "the fixture has a snapshot"
    );
    let expected = fs::read(root().join("expected.db")).unwrap();
    assert!(restore_to(&replica, None) == expected);
}

#[test]
fn every_file_decodes_and_snapshots_are_whole() {
    let replica = root().join("replica");
    for f in list(&replica) {
        let bytes = fs::read(replica.join(f.path())).unwrap();
        let ltx = Ltx::decode(&bytes).unwrap_or_else(|e| panic!("{}: {e}", f.path()));
        let h = ltx.header();
        assert_eq!((h.min_txid, h.max_txid), (f.min_txid, f.max_txid));
        assert!(
            h.no_checksum(),
            "Litestream 0.5 writes no database checksums"
        );
        if h.is_snapshot() {
            assert_eq!(ltx.page_count() as u32, h.commit);
        }
    }
}

#[test]
fn following_level_zero_ends_where_the_restore_does() {
    let replica = root().join("replica");
    let mut level0: Vec<FileInfo> = list(&replica)
        .into_iter()
        .filter(|f| f.level == 0)
        .collect();
    level0.sort_by_key(|f| f.min_txid);
    let mut db = Vec::new();
    let mut pos = Txid(0);
    for f in &level0 {
        let bytes = fs::read(replica.join(f.path())).unwrap();
        pos = restore::apply(&mut db, pos, &Ltx::decode(&bytes).unwrap()).unwrap();
        assert!(db == restore_to(&replica, Some(pos)), "TXID {pos}");
    }
    assert!(db == fs::read(root().join("expected.db")).unwrap());
}
