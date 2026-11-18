//! Sync scenarios: devices converge and never fork silently.
//!
//! Each test runs devices against one in-memory replica that can lose
//! requests, lose answers and fill up. `tests/s3.rs` runs the core of these
//! against a real S3-compatible store.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{Clock, Device, assert_converged};
use moul_ltx::Txid;
use moul_sync::layout::{HEAD_KEY, WRITER_KEY, ltx_key};
use moul_sync::{ErrorKind, Fault, MemoryReplica, Replica, Retention, Role, block_on};
use moul_vfs::CrashMode;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;

fn device(name: &str, replica: &MemoryReplica, clock: &Clock) -> Device {
    device_with(name, replica, clock, Retention::default())
}

fn device_with(name: &str, replica: &MemoryReplica, clock: &Clock, r: Retention) -> Device {
    let replica = replica.clone();
    Device::new(name, move || Box::new(replica.clone()), clock, r)
}

fn setup() -> (MemoryReplica, Clock) {
    let clock = Clock::new();
    let replica = MemoryReplica::new();
    replica.set_now(clock.now());
    (replica, clock)
}

#[test]
fn a_writer_pushes_and_a_follower_follows() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('before sync');");
    a.connect().unwrap();
    assert_eq!(a.role(), Role::Writer);
    assert_eq!(a.status().pending, 0);
    assert!(replica.object(WRITER_KEY).is_some());
    assert!(replica.object(HEAD_KEY).is_some());

    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    assert_eq!(b.role(), Role::Follower);
    assert_eq!(b.rows(), ["before sync"]);
    assert!(
        b.conflict_copies().is_empty(),
        "an empty device keeps nothing"
    );
    assert_converged(&replica, &[&a, &b]);

    for i in 0..20 {
        a.exec(&format!("INSERT INTO t VALUES ('row {i}')"));
    }
    assert_eq!(a.status().pending, 20);
    a.sync().unwrap();
    assert_eq!(a.status().pending, 0);
    b.sync().unwrap();
    assert_eq!(b.rows().len(), 21);
    assert_eq!(b.status().position, a.status().position);
    assert_converged(&replica, &[&a, &b]);

    // A schema change reaches the follower's open connection too.
    a.exec("ALTER TABLE t ADD COLUMN n INTEGER DEFAULT 7; CREATE INDEX t_v ON t (v);");
    a.sync().unwrap();
    b.sync().unwrap();
    let n: i64 = b
        .store()
        .conn()
        .query_row("SELECT n FROM t LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 7);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn a_device_with_data_keeps_a_copy_when_it_joins() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('laptop');");
    a.connect().unwrap();

    let mut b = device("phone", &replica, &clock);
    b.exec("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('phone only');");
    b.connect().unwrap();
    assert_eq!(b.rows(), ["laptop"]);
    let copies = b.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let kept = rusqlite::Connection::open(&copies[0]).unwrap();
    let body: String = kept
        .query_row("SELECT body FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "phone only");
    assert_eq!(b.status().conflicts.len(), 1);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn two_devices_race_to_create_the_replica() {
    let (replica, clock) = setup();
    let mut a = device("a", &replica, &clock);
    let mut b = device("b", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('a');");
    b.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('b');");
    // B checked first and saw no writer, but A's create lands before B's:
    // B's conditional create is refused, so it joins A's replica instead.
    a.connect().unwrap();
    b.connect().unwrap();
    assert_eq!((a.role(), b.role()), (Role::Writer, Role::Follower));
    assert_eq!(b.rows(), ["a"]);
    assert_eq!(b.conflict_copies().len(), 1);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn hand_off_moves_the_writer_and_the_old_writer_follows() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();

    b.take_over().unwrap();
    assert_eq!(b.role(), Role::Writer);
    let writer = b.status().writer.unwrap();
    assert_eq!(
        (writer.device.as_str(), writer.generation),
        (b.status().device.as_str(), 2)
    );
    b.exec("INSERT INTO t VALUES ('from phone')");
    b.sync().unwrap();

    // A had nothing unpushed: it just becomes a follower.
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    assert!(a.conflict_copies().is_empty());
    assert_eq!(a.rows(), ["one", "from phone"]);
    assert_converged(&replica, &[&a, &b]);

    // And it can take the role back.
    a.take_over().unwrap();
    a.exec("INSERT INTO t VALUES ('laptop again')");
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!((a.role(), b.role()), (Role::Writer, Role::Follower));
    assert_eq!(b.rows(), ["one", "from phone", "laptop again"]);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn an_offline_writer_that_lost_the_role_keeps_a_conflict_copy() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();

    // The laptop goes offline and keeps writing; the phone takes over.
    a.exec("INSERT INTO t VALUES ('written offline 1')");
    a.exec("INSERT INTO t VALUES ('written offline 2')");
    b.take_over().unwrap();
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();

    // Back online, the laptop's upload hits the fence.
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    assert_eq!(a.rows(), ["shared", "phone"]);
    let copies = a.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let kept = rusqlite::Connection::open(&copies[0]).unwrap();
    let rows: Vec<String> = kept
        .prepare("SELECT v FROM t ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, ["shared", "written offline 1", "written offline 2"]);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn an_old_writers_last_upload_before_the_fence_is_kept() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();

    // The laptop's file lands, but it dies before updating head, so the
    // phone believes it is caught up when it takes over.
    a.exec("INSERT INTO t VALUES ('late')");
    let pos = a.status().position;
    let bytes = a.capture.read(Txid(pos)).unwrap();
    replica.insert(&ltx_key(0, Txid(pos), Txid(pos)), &bytes);

    b.take_over().unwrap();
    assert_eq!(
        b.rows(),
        ["one", "late"],
        "the fence went after the late file"
    );
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    assert!(
        a.conflict_copies().is_empty(),
        "nothing was lost, so nothing is kept"
    );
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn lost_requests_and_lost_answers_are_retried() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();

    for (i, faults) in [
        vec![Fault::Lost],
        vec![Fault::AnswerLost],
        vec![Fault::Lost, Fault::AnswerLost],
        // Head read fine, file written but its answer lost.
        vec![Fault::AnswerLost, Fault::AnswerLost],
    ]
    .into_iter()
    .enumerate()
    {
        a.exec(&format!("INSERT INTO t VALUES ('try {i}')"));
        replica.fail_next(&faults);
        let mut tries = 0;
        while let Err(e) = a.sync() {
            assert_eq!(e.kind(), ErrorKind::Network, "{e}");
            tries += 1;
            assert!(tries < 5);
        }
        assert_eq!(a.status().pending, 0);
        replica.fail_next(&[Fault::AnswerLost]);
        while b.sync().is_err() {}
        assert_eq!(b.rows().len(), i + 1);
    }
    assert_eq!(
        a.role(),
        Role::Writer,
        "its own earlier upload is not a conflict"
    );
    assert!(a.conflict_copies().is_empty());
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn a_full_replica_holds_changes_back_until_there_is_room() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    replica.set_full(true);
    for i in 0..5 {
        a.exec(&format!("INSERT INTO t VALUES ('{i}')"));
        let err = a.sync().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Network);
        assert!(a.status().last_error.unwrap().contains("full"));
    }
    assert_eq!(a.status().pending, 5);
    // The app restarts while the replica is still full.
    a.restart();
    assert!(a.sync().is_err());
    assert_eq!(a.role(), Role::Writer, "a writer keeps writing offline");
    a.exec("INSERT INTO t VALUES ('after restart')");
    replica.set_full(false);
    a.sync().unwrap();
    assert_eq!(a.status().pending, 0);
    assert_eq!(a.status().last_error, None);
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    assert_eq!(b.rows().len(), 6);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn a_writer_restarts_between_upload_and_head() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    a.exec("INSERT INTO t VALUES ('x'); INSERT INTO t VALUES ('y');");
    // Two files land, then the app quits before writing head.
    let head_before = replica.object(HEAD_KEY).unwrap();
    {
        // Upload the files by hand, as a sync that died before head would.
        let p = a.status().position;
        for t in p - 1..=p {
            let bytes = a.capture.read(Txid(t)).unwrap();
            replica.insert(&ltx_key(0, Txid(t), Txid(t)), &bytes);
        }
    }
    assert_eq!(replica.object(HEAD_KEY).unwrap(), head_before);
    a.restart();
    a.connect().unwrap();
    assert_eq!(a.role(), Role::Writer);
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.rows(), ["x", "y"]);
    assert!(a.conflict_copies().is_empty());
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn a_follower_that_sleeps_past_retention_catches_up_from_a_snapshot() {
    let (replica, clock) = setup();
    let r = Retention {
        level1_every: 5 * MINUTE,
        level2_every: HOUR,
        snapshot_every: 2 * HOUR,
        compacted_kept: 3 * HOUR,
        history: 24 * HOUR,
    };
    let mut a = device_with("laptop", &replica, &clock, r.clone());
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    let mut b = device_with("phone", &replica, &clock, r.clone());
    b.connect().unwrap();
    let mut c = device_with("tablet", &replica, &clock, r);
    c.connect().unwrap();

    // Two days of writing, one row every ten minutes.
    for i in 0..288 {
        clock.advance(10 * MINUTE);
        replica.set_now(clock.now());
        a.exec(&format!("INSERT INTO t VALUES ('row {i}')"));
        a.sync().unwrap();
        if i == 140 {
            c.sync().unwrap(); // the tablet looks in now and then
        }
    }
    let keys = replica.keys();
    let level = |l: u8| {
        keys.iter()
            .filter(|k| k.starts_with(&format!("ltx/{l}/")))
            .count()
    };
    assert!(level(1) > 0 && level(2) > 0 && level(9) > 0, "{keys:?}");
    let first_l0 = keys.iter().find(|k| k.starts_with("ltx/0/")).unwrap();
    assert!(
        !first_l0.ends_with("0000000000000002-0000000000000002.ltx"),
        "old level-0 files are pruned: {first_l0}"
    );
    let report = a.sync.last_report().cloned();
    assert!(report.is_some());

    // The phone slept the whole time: TXID 3 is long gone.
    b.sync().unwrap();
    c.sync().unwrap();
    assert_eq!(b.rows().len(), 288);
    assert!(b.conflict_copies().is_empty());
    assert_converged(&replica, &[&a, &b, &c]);

    // Every moment in the history window still restores.
    let files = block_on(moul_sync::layout::list_files(&replica)).unwrap();
    let newest = files.iter().map(|f| f.max_txid).max().unwrap();
    let (_, at_newest) = block_on(moul_sync::restore::restore_db(&replica, Some(newest))).unwrap();
    assert!(at_newest == a.main_db());
}

#[test]
fn a_follower_survives_a_crash_at_any_point_while_applying() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    for i in 0..6 {
        a.exec(&format!(
            "INSERT INTO t VALUES ('{}')",
            "x".repeat(3000 + i)
        ));
    }
    a.exec("CREATE TABLE big (b BLOB); INSERT INTO big VALUES (zeroblob(50000));");
    a.sync().unwrap();
    let start_storage = b.storage.steps();
    let start_ltx = b.ltx.steps();
    let (base_storage, base_ltx) = (b.storage.fork(), b.ltx.fork());
    // Count the steps one clean catch-up takes.
    b.sync().unwrap();
    let total = (b.storage.steps() - start_storage) + (b.ltx.steps() - start_ltx);
    assert!(total > 10, "{total}");
    eprintln!("crash points in one catch-up: {total}");
    let modes = [
        CrashMode::DropUnflushed,
        CrashMode::KeepUnflushed,
        CrashMode::Torn(7),
    ];
    let stride = if cfg!(debug_assertions) { 3 } else { 1 };
    for step in (0..total).step_by(stride) {
        for mode in modes {
            let mut d = device("phone", &replica, &clock);
            d.storage = base_storage.fork();
            d.ltx = base_ltx.fork();
            d.restart();
            // Fail one of the two storages at this step of the catch-up.
            let (s0, l0) = (d.storage.steps(), d.ltx.steps());
            if step % 2 == 0 {
                d.storage.fail_at(Some(s0 + step / 2));
            } else {
                d.ltx.fail_at(Some(l0 + step / 2));
            }
            let _ = d.sync();
            d.crash(mode);
            d.sync()
                .unwrap_or_else(|e| panic!("step {step} {mode:?}: {e}"));
            assert_eq!(d.rows().len(), 6, "step {step} {mode:?}");
            assert!(
                d.main_db() == a.main_db(),
                "step {step} {mode:?}: the follower's file differs"
            );
        }
    }
}

#[test]
fn a_replica_that_litestream_wrote_can_be_followed() {
    let (replica, clock) = setup();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/litestream");
    for level in ["0", "1", "9"] {
        for entry in std::fs::read_dir(root.join("replica/ltx").join(level)).unwrap() {
            let entry = entry.unwrap();
            let key = format!("ltx/{level}/{}", entry.file_name().to_string_lossy());
            replica.insert(&key, &std::fs::read(entry.path()).unwrap());
        }
    }
    let expected = std::fs::read(root.join("expected.db")).unwrap();
    let mut b = device("reader", &replica, &clock);
    b.connect().unwrap();
    assert_eq!(b.role(), Role::Follower);
    assert!(b.main_db() == expected);
    // Litestream keeps writing; the reader follows by listing.
    b.sync().unwrap();
    assert!(b.main_db() == expected);
}

#[test]
fn followers_cannot_be_tricked_into_a_stale_head() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    b.take_over().unwrap();
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();
    let head = replica.object(HEAD_KEY).unwrap();
    // The laptop still believes it writes and has nothing new: its idle
    // check reads the head and steps down without touching it.
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    assert_eq!(replica.object(HEAD_KEY).unwrap(), head);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn the_device_state_survives_a_restart() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    let id = a.status().device;
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    a.restart();
    b.restart();
    a.exec("INSERT INTO t VALUES ('after restart')");
    a.connect().unwrap();
    b.connect().unwrap();
    assert_eq!(a.status().device, id);
    assert_eq!((a.role(), b.role()), (Role::Writer, Role::Follower));
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.rows(), ["after restart"]);
    assert!(a.conflict_copies().is_empty() && b.conflict_copies().is_empty());
    assert_converged(&replica, &[&a, &b]);
    let state = b.load_state().unwrap();
    assert_eq!(state.role, Role::Follower);
    let _ = replica.describe();
}

#[test]
fn a_follower_that_wrote_while_disconnected_keeps_a_copy() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    {
        let mut local = moul_sync::StoreLocal {
            store: b.store.as_mut().unwrap(),
            capture: b.capture.clone(),
            conflicts: Some(b.conflicts.clone()),
        };
        b.sync.disconnect(&mut local).unwrap();
    }
    assert_eq!(b.role(), Role::None);
    b.exec("INSERT INTO t VALUES ('phone, disconnected')");
    a.exec("INSERT INTO t VALUES ('laptop')");
    a.sync().unwrap();
    b.restart();
    b.connect().unwrap();
    assert_eq!(b.role(), Role::Follower);
    assert_eq!(b.rows(), ["shared", "laptop"]);
    assert_eq!(b.conflict_copies().len(), 1);
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn a_device_clock_far_ahead_does_not_prune_early() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT);");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    // The laptop's clock runs ten days fast; the replica's does not.
    clock.advance(10 * 24 * HOUR);
    for i in 0..5 {
        a.exec(&format!("INSERT INTO t VALUES ('{i}')"));
        a.sync().unwrap();
    }
    assert!(
        replica
            .keys()
            .iter()
            .any(|k| k.ends_with("0000000000000002-0000000000000002.ltx")),
        "level-0 files written minutes ago by the replica's clock are kept"
    );
    b.sync().unwrap();
    assert_converged(&replica, &[&a, &b]);
}
