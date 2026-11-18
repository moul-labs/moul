//! `litestream restore` rebuilds replicas that Moul's sync wrote: level-0
//! files from two writers with a hand-off and its fence between them, and
//! the compactions, snapshots and pruning the writer did along the way.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use litestream_compat::{harness_or_skip, scratch_dir};
use moul_ltx::Txid;
use moul_store::Store;
use moul_sync::{DirReplica, Options, Retention, Role, StoreLocal, Sync, block_on};
use moul_vfs::{Capture, MemoryStorage, Volume};

// Maintenance measures ages by the replica's clock, which for a folder is
// the real one, so the test runs on a short real-time schedule.
const TICK: u64 = 25;

struct Device {
    store: Store,
    capture: Capture,
    sync: Sync,
}

impl Device {
    fn new(name: &str, replica: &std::path::Path, clock: &Arc<AtomicI64>) -> Device {
        let capture = Capture::open(MemoryStorage::new(), "main.db").unwrap();
        let volume = Volume::open_observed(MemoryStorage::new(), capture.clone()).unwrap();
        let clock = clock.clone();
        Device {
            store: Store::open_follower(volume).unwrap(),
            capture,
            sync: Sync::new(
                DirReplica::new(replica),
                Options {
                    label: name.into(),
                    poll_every: 0,
                    maintain_every: 0,
                    retention: Retention {
                        level1_every: 4 * TICK as i64,
                        level2_every: 12 * TICK as i64,
                        snapshot_every: 20 * TICK as i64,
                        compacted_kept: 16 * TICK as i64,
                        history: 30 * TICK as i64,
                    },
                    follow_only: false,
                    clock: Box::new(move || clock.load(Ordering::SeqCst)),
                },
            ),
        }
    }

    fn run(&mut self, take_over: bool) {
        let mut local = StoreLocal {
            store: &mut self.store,
            capture: self.capture.clone(),
            conflicts: None,
        };
        if take_over {
            block_on(self.sync.take_over(&mut local)).unwrap();
        } else {
            block_on(self.sync.sync(&mut local)).unwrap();
        }
    }
}

#[test]
fn litestream_restores_what_moul_sync_wrote() {
    let h = harness_or_skip!();
    let dir = scratch_dir("sync-replica");
    let clock = Arc::new(AtomicI64::new(moul_vfs::system_clock()));
    let mut a = Device::new("laptop", &dir, &clock);
    let mut b = Device::new("phone", &dir, &clock);
    a.store
        .conn()
        .execute_batch("CREATE TABLE t (n INTEGER, body BLOB)")
        .unwrap();
    a.run(false);
    b.run(false);

    let mut states: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for i in 0..60i64 {
        std::thread::sleep(std::time::Duration::from_millis(TICK));
        clock.store(moul_vfs::system_clock(), Ordering::SeqCst);
        if i == 30 {
            b.run(false);
            b.run(true); // hand-off: the phone writes from here on
        }
        let writer = if i < 30 { &mut a } else { &mut b };
        writer
            .store
            .conn()
            .execute(
                "INSERT INTO t VALUES (?1, randomblob(?2))",
                rusqlite::params![i, 200 + (i * 997) % 9000],
            )
            .unwrap();
        if i % 7 == 6 {
            writer
                .store
                .conn()
                .execute_batch("DELETE FROM t WHERE n % 3 = 0; VACUUM;")
                .unwrap();
        }
        writer.run(false);
        states.insert(
            writer.capture.position().txid.0,
            writer.store.export().unwrap(),
        );
    }
    a.run(false);
    assert_eq!(
        (a.sync.role(), b.sync.role()),
        (Role::Follower, Role::Writer)
    );

    let files = litestream_compat::list(&dir);
    for level in [0u8, 1, 2, 9] {
        assert!(
            files.iter().any(|f| f.level == level),
            "no level {level}: {files:?}"
        );
    }
    let newest = files.iter().map(|f| f.max_txid).max().unwrap();
    let restored = h.restore(&dir, None).unwrap();
    assert!(
        restored == b.store.export().unwrap(),
        "latest restore differs"
    );
    assert!(
        restored == a.store.export().unwrap(),
        "the follower differs"
    );

    // Every TXID the replica still reaches restores to what the writer had.
    let mut checked = 0;
    for (&txid, want) in &states {
        if moul_ltx::restore::plan(&files, Some(Txid(txid))).is_err() {
            continue;
        }
        let got = h.restore(&dir, Some(Txid(txid))).unwrap();
        assert!(&got == want, "TXID {txid} of {newest} differs");
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} TXIDs were still restorable");
}
