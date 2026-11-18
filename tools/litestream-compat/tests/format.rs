//! LTX files, page by page: Moul and the Go library read each other's files
//! and agree on what they hold.

use litestream_compat::{FileJson, HeaderJson, PageJson, harness_or_skip, hex, unhex};
use moul_ltx::{
    CHECKSUM_FLAG, Checksum, Encoder, FLAG_NO_CHECKSUM, Header, Ltx, Txid, compact_to_vec,
    lock_pgno, page_checksum,
};

const PAGE_SIZES: [u32; 8] = [512, 1024, 2048, 4096, 8192, 16_384, 32_768, 65_536];

/// Deterministic page contents of several kinds: zeros, noise, text, runs.
fn page(page_size: u32, pgno: u32, version: u32) -> Vec<u8> {
    let n = page_size as usize;
    let mut seed = (pgno as u64) << 32 | version as u64 | 1;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed as u8
    };
    match (pgno + version) % 4 {
        0 => vec![0; n],
        1 => (0..n).map(|_| next()).collect(),
        2 => format!("page {pgno} version {version}; ")
            .into_bytes()
            .into_iter()
            .cycle()
            .take(n)
            .collect(),
        _ => (0..n).map(|i| (i / 97) as u8 ^ version as u8).collect(),
    }
}

struct Spec {
    header: Header,
    pages: Vec<(u32, Vec<u8>)>,
    post_apply: Checksum,
}

impl Spec {
    fn json(&self) -> FileJson {
        let h = &self.header;
        FileJson {
            header: HeaderJson {
                flags: h.flags,
                page_size: h.page_size,
                commit: h.commit,
                min_txid: h.min_txid.0,
                max_txid: h.max_txid.0,
                timestamp: h.timestamp,
                pre_apply_checksum: h.pre_apply_checksum.0,
                node_id: h.node_id,
            },
            pages: self
                .pages
                .iter()
                .map(|(pgno, d)| PageJson {
                    pgno: *pgno,
                    data: hex(d),
                })
                .collect(),
            post_apply_checksum: self.post_apply.0,
            file_checksum: 0,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut enc = Encoder::new(Vec::new(), self.header).unwrap();
        for (pgno, d) in &self.pages {
            enc.write_page(*pgno, d).unwrap();
        }
        enc.set_post_apply_checksum(self.post_apply);
        enc.finish().unwrap().0
    }
}

/// A snapshot and an incremental file at `page_size`, with and without
/// database checksums.
fn specs(page_size: u32) -> Vec<Spec> {
    let mut out = Vec::new();
    for tracked in [false, true] {
        let flags = if tracked { 0 } else { FLAG_NO_CHECKSUM };
        let snapshot: Vec<(u32, Vec<u8>)> = (1..=9).map(|p| (p, page(page_size, p, 1))).collect();
        let sum = snapshot
            .iter()
            .fold(Checksum(CHECKSUM_FLAG), |acc, (p, d)| {
                acc.xor(page_checksum(*p, d))
            });
        out.push(Spec {
            header: Header {
                flags,
                page_size,
                commit: 9,
                min_txid: Txid(1),
                max_txid: Txid(1),
                timestamp: 1_731_500_000_000,
                ..Header::default()
            },
            pages: snapshot,
            post_apply: if tracked { sum } else { Checksum(0) },
        });
        out.push(Spec {
            header: Header {
                flags,
                page_size,
                commit: 12,
                min_txid: Txid(2),
                max_txid: Txid(7),
                timestamp: 1_731_500_000_999,
                pre_apply_checksum: if tracked { sum } else { Checksum(0) },
                node_id: 42,
                ..Header::default()
            },
            pages: [2, 3, 10, 12].map(|p| (p, page(page_size, p, 2))).to_vec(),
            post_apply: if tracked {
                Checksum(CHECKSUM_FLAG | 0x1234)
            } else {
                Checksum(0)
            },
        });
    }
    out
}

#[test]
fn go_reads_files_moul_writes() {
    let go = harness_or_skip!();
    for ps in PAGE_SIZES {
        for spec in specs(ps) {
            let ours = spec.encode();
            let dumped = go
                .dump(&ours)
                .unwrap_or_else(|e| panic!("page size {ps}: Go refused our file: {e}"));
            let mut expected = spec.json();
            expected.file_checksum = Ltx::decode(&ours).unwrap().trailer().file_checksum.0;
            assert_eq!(dumped, expected, "page size {ps}");
        }
    }
}

#[test]
fn moul_reads_files_go_writes() {
    let go = harness_or_skip!();
    for ps in PAGE_SIZES {
        for spec in specs(ps) {
            let theirs = go.encode(&spec.json()).unwrap();
            let ltx = Ltx::decode(&theirs).unwrap_or_else(|e| panic!("page size {ps}: {e}"));
            let mut got = FileJson::from_ltx(&ltx);
            // The file checksum also covers the page index, which records
            // compressed sizes, so it differs between encoders.
            got.file_checksum = 0;
            assert_eq!(got, spec.json(), "page size {ps}");
        }
    }
}

#[test]
fn damaged_files_are_refused_by_both() {
    let go = harness_or_skip!();
    let spec = &specs(4096)[1];
    let ours = spec.encode();
    for at in [0, 20, 101, ours.len() / 2, ours.len() - 30, ours.len() - 1] {
        let mut bad = ours.clone();
        bad[at] ^= 0x10;
        assert!(Ltx::decode(&bad).is_err(), "Moul took damage at byte {at}");
        assert!(go.dump(&bad).is_err(), "Go took damage at byte {at}");
    }
}

#[test]
fn page_checksums_agree() {
    let go = harness_or_skip!();
    for (pgno, ps) in [(1, 512), (7, 4096), (262_146, 4096)] {
        let data = page(ps, pgno, 3);
        assert_eq!(go.page_checksum(pgno, &data), page_checksum(pgno, &data).0);
    }
}

/// Runs of files, each a list of (TXID range, commit, pages).
fn runs(ps: u32) -> Vec<Vec<Vec<u8>>> {
    let f = |min: u64, max: u64, commit: u32, pages: &[u32], v: u32| {
        let header = Header::new(ps, commit, Txid(min), Txid(max), min as i64 * 1000);
        let data: Vec<(u32, Vec<u8>)> = pages.iter().map(|&p| (p, page(ps, p, v))).collect();
        moul_ltx::encode(header, data.iter().map(|(p, d)| (*p, d.as_slice()))).unwrap()
    };
    vec![
        // A snapshot, then growth and rewrites.
        vec![
            f(1, 1, 4, &[1, 2, 3, 4], 1),
            f(2, 2, 6, &[1, 5, 6], 2),
            f(3, 5, 6, &[2, 6], 3),
        ],
        // Incremental only, with a shrink at the end.
        vec![
            f(10, 10, 8, &[1, 7, 8], 4),
            f(11, 11, 8, &[3], 5),
            f(12, 12, 5, &[1, 5], 6),
        ],
        // Overlapping ranges, as compaction levels produce.
        vec![f(20, 25, 3, &[1, 2, 3], 7), f(22, 30, 3, &[2], 8)],
    ]
}

#[test]
fn compactors_agree() {
    let go = harness_or_skip!();
    for ps in [512, 4096, 65_536] {
        for run in runs(ps) {
            let inputs: Vec<&[u8]> = run.iter().map(Vec::as_slice).collect();
            let decoded: Vec<Ltx<'_>> = run.iter().map(|b| Ltx::decode(b).unwrap()).collect();
            let ours = compact_to_vec(&decoded).unwrap();
            let theirs = go.compact(&inputs).unwrap();

            // Each reads the other's output, and both hold the same thing.
            let mut ours_by_go = go.dump(&ours).unwrap();
            let mut theirs_by_us = FileJson::from_ltx(&Ltx::decode(&theirs).unwrap());
            ours_by_go.file_checksum = 0;
            theirs_by_us.file_checksum = 0;
            assert_eq!(ours_by_go, theirs_by_us, "page size {ps}");
        }
    }
}

#[test]
fn the_lock_page_is_never_written() {
    let go = harness_or_skip!();
    let ps = 512;
    let lock = lock_pgno(ps);

    // Moul will not encode it.
    let header = Header::new(ps, lock + 2, Txid(5), Txid(5), 0);
    let mut enc = Encoder::new(Vec::new(), header).unwrap();
    enc.write_page(lock - 1, &page(ps, lock - 1, 1)).unwrap();
    assert!(enc.write_page(lock, &page(ps, lock, 1)).is_err());

    // Pages either side of it travel both ways.
    let spec = Spec {
        header,
        pages: [lock - 1, lock + 1, lock + 2]
            .map(|p| (p, page(ps, p, 1)))
            .to_vec(),
        post_apply: Checksum(0),
    };
    let ours = spec.encode();
    assert_eq!(go.dump(&ours).unwrap().pages.len(), 3);
    let theirs = go.encode(&spec.json()).unwrap();
    assert_eq!(Ltx::decode(&theirs).unwrap().page_count(), 3);

    // Go refuses a file that holds it, and so does Moul.
    let mut with_lock = spec.json();
    with_lock.pages.insert(
        1,
        PageJson {
            pgno: lock,
            data: hex(&page(ps, lock, 1)),
        },
    );
    assert!(go.encode(&with_lock).is_err());
}

#[test]
fn pages_survive_the_round_trip_exactly() {
    let go = harness_or_skip!();
    let spec = &specs(8192)[0];
    let dumped = go.dump(&spec.encode()).unwrap();
    for ((pgno, data), p) in spec.pages.iter().zip(&dumped.pages) {
        assert_eq!((*pgno, data.clone()), (p.pgno, unhex(&p.data)));
    }
}

#[test]
fn go_compacts_a_snapshot_across_the_lock_page() {
    let go = harness_or_skip!();
    let ps = 65_536;
    let lock = lock_pgno(ps);
    let commit = lock + 2;
    let mark = |pgno: u32, v: u8| {
        let mut p = vec![0u8; ps as usize];
        p[..4].copy_from_slice(&pgno.to_be_bytes());
        p[100] = v;
        p
    };
    let mut enc = Encoder::new(Vec::new(), Header::new(ps, commit, Txid(1), Txid(1), 0)).unwrap();
    for pgno in (1..=commit).filter(|&p| p != lock) {
        enc.write_page(pgno, &mark(pgno, 1)).unwrap();
    }
    let snapshot = enc.finish().unwrap().0;
    let next = moul_ltx::encode(
        Header::new(ps, commit, Txid(2), Txid(2), 0),
        [(lock + 1, &mark(lock + 1, 2)[..])],
    )
    .unwrap();

    let theirs = go.compact(&[&snapshot, &next]).unwrap();
    let ltx = Ltx::decode(&theirs).unwrap();
    assert!(ltx.header().is_snapshot());
    assert_eq!(ltx.page_count() as u32, commit - 1);
    assert!(ltx.find(lock).is_none());
    let mut buf = Vec::new();
    ltx.read_page(ltx.find(lock + 1).unwrap(), &mut buf)
        .unwrap();
    assert_eq!(buf, mark(lock + 1, 2));
}
