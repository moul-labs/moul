//! A database past 1 GiB has a lock page, which LTX files skip and a restore
//! writes as zeros. With 64 KiB pages that is page 16,385.

use std::io::{self, Write};

use moul_ltx::{Crc64, Encoder, Header, Ltx, Txid, lock_pgno, restore};

const PS: u32 = 65_536;

/// Checksums what is written, and checks the lock page is zeros, without
/// holding a gigabyte in memory.
struct Sink {
    n: u64,
    crc: Crc64,
    lock_nonzero: bool,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let lock_start = (lock_pgno(PS) as u64 - 1) * PS as u64;
        let from = lock_start.max(self.n);
        let to = (lock_start + PS as u64).min(self.n + buf.len() as u64);
        if from < to {
            let part = &buf[(from - self.n) as usize..(to - self.n) as usize];
            self.lock_nonzero |= part.iter().any(|&b| b != 0);
        }
        self.crc.update(buf);
        self.n += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn page(pgno: u32) -> Vec<u8> {
    let mut p = vec![0u8; PS as usize];
    p[..4].copy_from_slice(&pgno.to_be_bytes());
    p
}

#[test]
fn a_restore_across_the_lock_page() {
    let lock = lock_pgno(PS);
    let commit = lock + 2;
    let mut enc = Encoder::new(Vec::new(), Header::new(PS, commit, Txid(1), Txid(1), 0)).unwrap();
    for pgno in (1..=commit).filter(|&p| p != lock) {
        enc.write_page(pgno, &page(pgno)).unwrap();
    }
    let (snapshot, _) = enc.finish().unwrap();

    // An incremental file that rewrites the pages either side of it.
    let mut enc = Encoder::new(Vec::new(), Header::new(PS, commit, Txid(2), Txid(2), 0)).unwrap();
    for pgno in [lock - 1, lock + 1] {
        let mut p = page(pgno);
        p[100] = 0xAB;
        enc.write_page(pgno, &p).unwrap();
    }
    let (next, _) = enc.finish().unwrap();

    let files = [Ltx::decode(&snapshot).unwrap(), Ltx::decode(&next).unwrap()];
    assert!(files[0].find(lock).is_none());

    let mut sink = Sink {
        n: 0,
        crc: Crc64::new(),
        lock_nonzero: false,
    };
    restore::restore(&files, &mut sink).unwrap();
    assert_eq!(sink.n, commit as u64 * PS as u64);
    assert!(sink.n > 1 << 30);
    assert!(!sink.lock_nonzero, "the lock page must read as zeros");

    // The same database, built the slow way.
    let mut expected = Crc64::new();
    for pgno in 1..=commit {
        let mut p = if pgno == lock {
            vec![0u8; PS as usize]
        } else {
            page(pgno)
        };
        if pgno == lock - 1 || pgno == lock + 1 {
            p[100] = 0xAB;
        }
        expected.update(&p);
    }
    assert_eq!(sink.crc.finish(), expected.finish());

    // A snapshot that holds the lock page, or skips another, is refused.
    let mut enc = Encoder::new(Vec::new(), Header::new(PS, commit, Txid(1), Txid(1), 0)).unwrap();
    for pgno in 1..lock {
        enc.write_page(pgno, &page(pgno)).unwrap();
    }
    assert!(enc.write_page(lock, &page(lock)).is_err());
    assert!(enc.write_page(lock + 2, &page(lock + 2)).is_err());
}
