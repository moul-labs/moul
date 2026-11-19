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
    let base_writer_ltx = b.writer_ltx.fork();
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
            d.writer_ltx = base_writer_ltx.fork();
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

/// Rows of `t` in a conflict copy.
fn copy_rows(path: &std::path::Path) -> Vec<String> {
    let kept = rusqlite::Connection::open(path).unwrap();
    kept.prepare("SELECT v FROM t ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn a_writer_that_wrote_while_disconnected_pushes_on_reconnect() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    a.disconnect().unwrap();
    assert_eq!(a.role(), Role::None);
    // Its level-0 files are what connecting again pushes: a host keeps
    // them while the device is disconnected.
    a.exec("INSERT INTO t VALUES ('laptop, disconnected 1')");
    a.exec("INSERT INTO t VALUES ('laptop, disconnected 2')");
    assert!(a.capture.first_held().is_some());
    a.restart();
    a.connect().unwrap();
    assert_eq!(a.role(), Role::Writer);
    assert_eq!(a.status().pending, 0, "{:?}", a.status());
    b.sync().unwrap();
    assert_eq!(
        b.rows(),
        ["shared", "laptop, disconnected 1", "laptop, disconnected 2"]
    );
    assert!(a.conflict_copies().is_empty() && b.conflict_copies().is_empty());
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn a_writer_that_wrote_while_disconnected_keeps_a_copy_if_another_took_over() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    a.disconnect().unwrap();
    b.take_over().unwrap();
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();
    a.exec("INSERT INTO t VALUES ('laptop, disconnected')");
    a.restart();
    a.connect().unwrap();
    assert_eq!((a.role(), b.role()), (Role::Follower, Role::Writer));
    assert_eq!(a.rows(), ["shared", "phone"]);
    let copies = a.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copy_rows(&copies[0]), ["shared", "laptop, disconnected"]);
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

/// A replica that something else compacts, as a hub does.
struct Remote(MemoryReplica);

impl Replica for Remote {
    fn describe(&self) -> String {
        self.0.describe()
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<Option<Object>>> {
        self.0.get(key)
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, moul_sync::Result<Put>> {
        self.0.put(key, bytes, condition)
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, moul_sync::Result<Vec<ObjectInfo>>> {
        self.0.list(prefix)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<()>> {
        self.0.delete(key)
    }
    fn maintained_remotely(&self) -> bool {
        true
    }
}

#[test]
fn a_writer_leaves_a_remotely_maintained_replica_to_its_maintainer() {
    let r = Retention {
        level1_every: 5 * MINUTE,
        level2_every: 10 * MINUTE,
        snapshot_every: 20 * MINUTE,
        compacted_kept: 30 * MINUTE,
        history: HOUR,
    };
    let run = |remote: bool| {
        let (replica, clock) = setup();
        let shared = replica.clone();
        let mut a = Device::new(
            "laptop",
            move || -> Box<dyn Replica> {
                if remote {
                    // Boxed twice, as hosts pass a boxed replica to `Sync::new`.
                    Box::new(Box::new(Remote(shared.clone())) as Box<dyn Replica>)
                } else {
                    Box::new(shared.clone())
                }
            },
            &clock,
            r.clone(),
        );
        assert_eq!(a.sync.replica().maintained_remotely(), remote);
        a.exec("CREATE TABLE t (v TEXT);");
        a.connect().unwrap();
        let mut b = device_with("phone", &replica, &clock, r.clone());
        b.connect().unwrap();
        for i in 0..30 {
            clock.advance(5 * MINUTE);
            replica.set_now(clock.now());
            a.exec(&format!("INSERT INTO t VALUES ('row {i}')"));
            a.sync().unwrap();
        }
        b.sync().unwrap();
        assert_eq!(b.rows().len(), 30);
        assert_converged(&replica, &[&a, &b]);
        (replica.keys(), a.sync.last_report().is_some())
    };

    let (keys, reported) = run(false);
    assert!(reported);
    assert!(keys.iter().any(|k| k.starts_with("ltx/9/")), "{keys:?}");

    let (keys, reported) = run(true);
    assert!(!reported, "no maintenance pass ran");
    let ltx: Vec<_> = keys.iter().filter(|k| k.starts_with("ltx/")).collect();
    assert!(ltx.iter().all(|k| k.starts_with("ltx/0/")), "{keys:?}");
    assert_eq!(ltx.len(), 31, "every level-0 file is still there: {keys:?}");
}

// ---- The writer-only store ------------------------------------------------

/// The writer-only store's newest state, restored from its part of the
/// replica.
fn writer_only_restored(d: &Device) -> Vec<u8> {
    block_on(moul_sync::restore::restore_db(
        &d.writer_only_replica(),
        None,
    ))
    .unwrap()
    .1
}

fn writer_only_head(replica: &MemoryReplica) -> moul_sync::layout::Head {
    serde_json::from_slice(&replica.object("moul/writer-only/moul/head.json").unwrap()).unwrap()
}

#[test]
fn the_writer_only_store_is_pushed_beside_the_shared_one() {
    let r = Retention {
        level1_every: 5 * MINUTE,
        level2_every: 10 * MINUTE,
        snapshot_every: 20 * MINUTE,
        compacted_kept: 30 * MINUTE,
        history: HOUR,
    };
    let (replica, clock) = setup();
    let mut a = device_with("server", &replica, &clock, r.clone());
    a.exec("CREATE TABLE t (v TEXT); CREATE TABLE writer.secrets (k TEXT, v TEXT);");
    a.connect().unwrap();
    a.push_writer_only().unwrap();
    assert_eq!(a.writer_sync.role(), Role::Writer);
    let mut b = device_with("phone", &replica, &clock, r.clone());
    b.connect().unwrap();
    let phone_secrets = b.writer_db();
    for i in 0..30 {
        clock.advance(5 * MINUTE);
        replica.set_now(clock.now());
        a.exec(&format!("INSERT INTO t VALUES ('row {i}')"));
        // One transaction across both stores, one in the writer-only store.
        a.exec(&format!(
            "BEGIN; INSERT INTO t VALUES ('both {i}');
             INSERT INTO secrets VALUES ('user {i}', 'hash'); COMMIT;"
        ));
        a.exec(&format!(
            "INSERT INTO secrets VALUES ('log {i}', '{}')",
            "x".repeat(2000)
        ));
        a.sync().unwrap();
        a.push_writer_only().unwrap();
    }
    b.sync().unwrap();
    assert_eq!(b.rows().len(), 60);
    // The shared store converges and restores as before, untouched by the
    // writer-only store beside it.
    assert_converged(&replica, &[&a, &b]);
    assert!(writer_only_restored(&a) == a.writer_db());
    assert!(
        b.writer_db() == phone_secrets,
        "a follower never gets the writer-only store"
    );
    assert_eq!(
        writer_only_head(&replica).txid,
        a.writer_capture.position().txid.0,
        "the writer-only store keeps its own numbering"
    );
    assert_eq!(a.writer_sync.status(&a.writer_capture).pending, 0);
    let keys = replica.keys();
    for key in [
        "moul/writer-only/moul/head.json",
        "moul/writer-only/moul/writer.json",
    ] {
        assert!(keys.iter().any(|k| k == key), "{key}: {keys:?}");
    }
    // Compacted and pruned under its own prefix.
    let snapshots = keys
        .iter()
        .filter(|k| k.starts_with("moul/writer-only/ltx/9/"))
        .count();
    assert!(snapshots > 0, "{keys:?}");
    let level0 = keys
        .iter()
        .filter(|k| k.starts_with("moul/writer-only/ltx/0/"))
        .count();
    assert!(
        (level0 as u64) < a.writer_capture.position().txid.0,
        "old level-0 files were pruned: {keys:?}"
    );
}

#[test]
fn a_push_only_sync_leaves_another_devices_writer_only_store_alone() {
    let (replica, clock) = setup();
    let mut a = device("server", &replica, &clock);
    a.exec("CREATE TABLE writer.secrets (v TEXT); INSERT INTO secrets VALUES ('a');");
    a.push_writer_only().unwrap();
    let mut b = device("another server", &replica, &clock);
    b.exec("CREATE TABLE writer.secrets (v TEXT); INSERT INTO secrets VALUES ('b');");
    let mine = b.writer_db();
    let refused = |b: &mut Device| {
        let e = b.push_writer_only().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Conflict, "{e}");
        assert!(e.to_string().contains("only pushes its own"), "{e}");
        assert!(b.writer_db() == mine, "its own store is kept as it is");
        assert!(b.conflict_copies().is_empty());
    };
    refused(&mut b);
    // Nor does it take files that no writer record names.
    replica.remove("moul/writer-only/moul/writer.json");
    refused(&mut b);
    assert!(writer_only_restored(&a) == a.writer_db());
}

#[test]
fn a_device_that_takes_over_writing_fetches_the_writer_only_store() {
    let (replica, clock) = setup();
    let mut a = device("server", &replica, &clock);
    a.exec(
        "CREATE TABLE t (v TEXT); CREATE TABLE writer.secrets (v TEXT);
         INSERT INTO secrets VALUES ('root password hash');",
    );
    a.connect().unwrap();
    a.push_writer_only().unwrap();
    let mut b = device("laptop", &replica, &clock);
    b.connect().unwrap();
    b.take_over().unwrap();
    b.take_over_writer_only().unwrap();
    assert!(
        b.writer_db() == a.writer_db(),
        "the new writer has the secrets"
    );
    // The old writer's late change to its writer-only store is refused, not
    // pushed over the new writer's.
    a.exec("INSERT INTO secrets VALUES ('late')");
    let e = a.push_writer_only().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Conflict, "{e}");
    // The new writer pushes on with the push-only sync, from where it took over.
    b.exec("INSERT INTO secrets VALUES ('new')");
    b.push_writer_only().unwrap();
    assert!(writer_only_restored(&b) == b.writer_db());
    let rows: Vec<String> = {
        let conn = b.store().conn();
        let mut stmt = conn
            .prepare("SELECT v FROM secrets ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(rows, ["root password hash", "new"]);
    assert!(a.conflict_copies().is_empty() && b.conflict_copies().is_empty());
}

#[test]
fn the_writer_only_store_is_pushed_after_a_crash() {
    for mode in [
        CrashMode::DropUnflushed,
        CrashMode::KeepUnflushed,
        CrashMode::Torn(3),
    ] {
        let (replica, clock) = setup();
        let mut a = device("server", &replica, &clock);
        a.exec("CREATE TABLE t (v TEXT); CREATE TABLE writer.secrets (v TEXT);");
        a.connect().unwrap();
        a.push_writer_only().unwrap();
        for i in 0..5 {
            a.exec(&format!(
                "BEGIN; INSERT INTO t VALUES ('{i}');
                 INSERT INTO secrets VALUES ('{}'); COMMIT;",
                "s".repeat(1500 + i)
            ));
        }
        a.sync().unwrap();
        // The power goes before the writer-only store is pushed.
        a.crash(mode);
        a.integrity();
        a.push_writer_only()
            .unwrap_or_else(|e| panic!("{mode:?}: {e}"));
        assert!(
            writer_only_restored(&a) == a.writer_db(),
            "{mode:?}: the replica's writer-only store differs"
        );
        assert!(a.conflict_copies().is_empty(), "{mode:?}");
        let n: i64 = a
            .store()
            .conn()
            .query_row("SELECT count(*) FROM secrets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 5, "{mode:?}");
    }
}

// ---- Requests that arrive while sync replaces the database ----------------

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use moul_store::Store;
use moul_sync::{
    BoxFuture, Condition, ConflictCopy, DeviceState, Local, MemoryReplica as Memory, Object,
    ObjectInfo, Options, Put, Sync, store_apply, store_has_data, store_load_state,
    store_save_state,
};
use moul_vfs::{ApplyMode, Capture, MemoryStorage, Position, Volume};

/// A device whose host answers requests while sync runs, as the browser
/// worker does: a request may arrive whenever sync waits, on the replica or
/// on keeping a conflict copy. Like the worker, the host refuses writes
/// while sync has made the device read-only.
struct Racer {
    store: RefCell<Store>,
    capture: Capture,
    conflicts: PathBuf,
    read_only: Cell<bool>,
    /// Where the next request arrives: a replica call (`"get"` or `"list"`)
    /// on a key that starts with the prefix, or `("saved", "")` right after
    /// a conflict copy is kept.
    at: RefCell<Option<(&'static str, &'static str)>>,
    /// The next read of a key that starts with this fails.
    fail: RefCell<Option<&'static str>>,
    /// The next write of this key is lost.
    lose: RefCell<Option<String>>,
    acked: RefCell<Vec<String>>,
    refused: RefCell<Vec<String>>,
}

impl Racer {
    fn exec(&self, sql: &str) {
        self.store.borrow().conn().execute_batch(sql).unwrap();
    }

    fn rows(&self) -> Vec<String> {
        let store = self.store.borrow();
        let mut stmt = store
            .conn()
            .prepare("SELECT v FROM t ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// A request that writes arrives now, if one is due at `call` on `key`.
    fn arrive(&self, call: &str, key: &str) {
        let due = matches!(*self.at.borrow(), Some((c, p)) if c == call && key.starts_with(p));
        if !due {
            return;
        }
        *self.at.borrow_mut() = None;
        let v = format!("written during {call} {key}");
        if self.read_only.get() {
            self.refused.borrow_mut().push(v);
        } else {
            // A storage reset may have left no table yet.
            self.store
                .borrow()
                .conn()
                .execute_batch("CREATE TABLE IF NOT EXISTS t (v TEXT)")
                .unwrap();
            self.store
                .borrow()
                .conn()
                .execute("INSERT INTO t VALUES (?1)", [&v])
                .unwrap();
            self.acked.borrow_mut().push(v);
        }
    }
}

struct RacerLocal(Rc<Racer>);

impl Local for RacerLocal {
    fn capture(&self) -> Capture {
        self.0.capture.clone()
    }
    fn apply(&mut self, ltx: &[u8], mode: ApplyMode) -> moul_sync::Result<Position> {
        store_apply(&mut self.0.store.borrow_mut(), &self.0.capture, ltx, mode)
    }
    fn fence(&mut self) -> moul_sync::Result<Txid> {
        Ok(self.0.capture.fence(self.0.store.borrow().volume())?)
    }
    fn rebase(&mut self) -> moul_sync::Result<Option<Txid>> {
        Ok(self.0.capture.rebase(self.0.store.borrow().volume())?)
    }
    fn export(&mut self) -> moul_sync::Result<Vec<u8>> {
        Ok(self.0.store.borrow().export()?)
    }
    fn has_data(&mut self) -> moul_sync::Result<bool> {
        store_has_data(&self.0.store.borrow())
    }
    fn load_state(&mut self) -> moul_sync::Result<Option<DeviceState>> {
        store_load_state(&self.0.store.borrow())
    }
    fn save_state(&mut self, state: &DeviceState) -> moul_sync::Result<()> {
        store_save_state(&self.0.store.borrow(), state)
    }
    fn save_conflict(&mut self, copy: ConflictCopy) -> BoxFuture<'_, moul_sync::Result<String>> {
        Box::pin(async move {
            std::fs::create_dir_all(&self.0.conflicts).unwrap();
            let path = self.0.conflicts.join(&copy.name);
            std::fs::write(&path, &copy.bytes).unwrap();
            // Keeping a copy waits on storage: a request may come in.
            self.0.arrive("saved", "");
            Ok(path.display().to_string())
        })
    }
    fn role_changed(&mut self, role: Role) {
        self.0.read_only.set(role == Role::Follower);
    }
}

/// The shared replica, as the racing device reaches it.
struct RacerReplica(Memory, Rc<Racer>);

impl Replica for RacerReplica {
    fn describe(&self) -> String {
        self.0.describe()
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<Option<Object>>> {
        Box::pin(async move {
            self.1.arrive("get", key);
            let fail = self.1.fail.borrow().is_some_and(|p| key.starts_with(p));
            if fail {
                *self.1.fail.borrow_mut() = None;
                return Err(moul_sync::Error::new(ErrorKind::Network, "connection lost"));
            }
            self.0.get(key).await
        })
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, moul_sync::Result<Put>> {
        if self.1.lose.borrow().as_deref() == Some(key) {
            *self.1.lose.borrow_mut() = None;
            return Box::pin(async {
                Err(moul_sync::Error::new(ErrorKind::Network, "connection lost"))
            });
        }
        self.0.put(key, bytes, condition)
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, moul_sync::Result<Vec<ObjectInfo>>> {
        Box::pin(async move {
            self.1.arrive("list", prefix);
            self.0.list(prefix).await
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<()>> {
        self.0.delete(key)
    }
}

/// A racing device and its sync.
struct Racing {
    host: Rc<Racer>,
    sync: Sync,
}

impl Racing {
    fn new(name: &str, replica: &MemoryReplica, clock: &Clock) -> Racing {
        let ltx = MemoryStorage::new();
        let capture = Capture::open(ltx, "main.db").unwrap();
        let volume = Volume::open_observed(MemoryStorage::new(), capture.clone()).unwrap();
        let host = Rc::new(Racer {
            store: RefCell::new(Store::open_follower(volume).unwrap()),
            capture,
            conflicts: common::scratch(&format!("racing-{name}-{}", moul_sync::new_id())),
            read_only: Cell::new(false),
            at: RefCell::new(None),
            fail: RefCell::new(None),
            lose: RefCell::new(None),
            acked: RefCell::new(Vec::new()),
            refused: RefCell::new(Vec::new()),
        });
        let sync = Racing::new_sync(name, replica, &host, clock);
        Racing { host, sync }
    }

    fn new_sync(name: &str, replica: &MemoryReplica, host: &Rc<Racer>, clock: &Clock) -> Sync {
        let clock = clock.clone();
        Sync::new(
            RacerReplica(replica.clone(), host.clone()),
            Options {
                label: name.to_owned(),
                poll_every: 0,
                maintain_every: 0,
                clock: Box::new(move || clock.now()),
                ..Options::default()
            },
        )
    }

    /// The app restarts: the host opens writable, and sync loads the role
    /// the device had, as the browser worker does, before connecting.
    fn restart(&mut self, name: &str, replica: &MemoryReplica, clock: &Clock) {
        self.host.read_only.set(false);
        self.sync = Racing::new_sync(name, replica, &self.host, clock);
        self.sync.load(&mut RacerLocal(self.host.clone())).unwrap();
    }

    fn connect(&mut self) -> moul_sync::Result<()> {
        block_on(self.sync.connect(&mut RacerLocal(self.host.clone())))
    }

    fn sync(&mut self) -> moul_sync::Result<()> {
        block_on(self.sync.sync(&mut RacerLocal(self.host.clone())))
    }

    fn take_over(&mut self) -> moul_sync::Result<()> {
        block_on(self.sync.take_over(&mut RacerLocal(self.host.clone())))
    }

    /// The next request arrives at `call` on a key starting with `prefix`.
    fn arm(&self, call: &'static str, prefix: &'static str) {
        *self.host.at.borrow_mut() = Some((call, prefix));
    }

    /// Asserts that a request arrived, and that every write the host
    /// acknowledged is in the database or in a conflict copy.
    fn assert_nothing_lost(&self) {
        let (acked, refused) = (self.host.acked.borrow(), self.host.refused.borrow());
        assert!(
            !acked.is_empty() || !refused.is_empty(),
            "no request arrived"
        );
        assert!(self.host.at.borrow().is_none(), "the request never came");
        let mut seen = self.host.rows();
        let mut copies: Vec<PathBuf> = std::fs::read_dir(&self.host.conflicts)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        copies.sort();
        for c in &copies {
            seen.extend(copy_rows(c));
        }
        for v in acked.iter() {
            assert!(
                seen.contains(v),
                "{v:?} was acknowledged, then lost: {seen:?}"
            );
        }
    }

    fn assert_same_as(&self, other: &Device) {
        let ok: String = self
            .host
            .store
            .borrow()
            .conn()
            .query_row("PRAGMA main.integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        assert!(
            self.host.store.borrow().export().unwrap() == other.main_db(),
            "the racing device differs from {}",
            other.name
        );
    }
}

/// A request that writes while a device joins a replica, after its copy is
/// kept or while the replica's state is downloaded, is refused: it would be
/// in neither the copy nor the state that replaces the database.
#[test]
fn a_write_while_joining_is_refused_not_lost() {
    for (call, prefix) in [("saved", ""), ("list", "ltx/"), ("get", "ltx/")] {
        let (replica, clock) = setup();
        let mut a = device("laptop", &replica, &clock);
        a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('laptop');");
        a.connect().unwrap();
        a.exec("INSERT INTO t VALUES ('laptop 2')");
        a.sync().unwrap();

        let mut b = Racing::new("phone", &replica, &clock);
        b.host
            .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('phone only');");
        b.arm(call, prefix);
        b.connect().unwrap();
        assert_eq!(b.sync.role(), Role::Follower);
        b.assert_nothing_lost();
        assert_eq!(b.host.refused.borrow().len(), 1, "{call} {prefix}");
        assert_eq!(b.host.rows(), ["laptop", "laptop 2"]);
        b.assert_same_as(&a);
    }
}

/// A writer that lost the role refuses writes from the moment it learns
/// so: one that arrived after it read its position would be counted as
/// pushed, and the new writer's next file applied on top of it.
#[test]
fn a_write_while_losing_the_role_is_refused_not_mixed_in() {
    let (replica, clock) = setup();
    let mut a = Racing::new("laptop", &replica, &clock);
    a.host
        .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    assert_eq!(a.sync.role(), Role::Writer);
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();

    // The laptop's file lands, but its answer is lost; the phone fences
    // after it, and writes.
    a.host.exec("INSERT INTO t VALUES ('late')");
    let pos = a.host.capture.position().txid;
    replica.insert(&ltx_key(0, pos, pos), &a.host.capture.read(pos).unwrap());
    b.take_over().unwrap();
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();

    // The laptop checks its landed file against the replica; a request
    // arrives meanwhile.
    a.arm("get", "ltx/0/");
    a.sync().unwrap();
    assert_eq!(a.sync.role(), Role::Follower);
    a.assert_nothing_lost();
    a.assert_same_as(&b);
    assert_eq!(a.host.rows(), ["one", "late", "phone"]);
    assert_eq!(a.host.refused.borrow().len(), 1);

    // The role comes back by taking over, and the laptop writes again.
    a.take_over().unwrap();
    assert!(!a.host.read_only.get());
    a.host.exec("INSERT INTO t VALUES ('laptop again')");
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.rows(), ["one", "late", "phone", "laptop again"]);
    a.assert_same_as(&b);
}

/// A writer that lost the role with changes the replica lacks keeps them as
/// a conflict copy; a write that arrives while the copy is kept is refused,
/// not lost when the replica's state replaces the database.
#[test]
fn a_write_while_keeping_a_copy_is_refused_not_lost() {
    let (replica, clock) = setup();
    let mut a = Racing::new("laptop", &replica, &clock);
    a.host
        .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    a.host.exec("INSERT INTO t VALUES ('written offline')");
    b.take_over().unwrap();
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();

    a.arm("saved", "");
    a.sync().unwrap();
    assert_eq!(a.sync.role(), Role::Follower);
    a.assert_nothing_lost();
    assert_eq!(a.host.refused.borrow().len(), 1);
    assert_eq!(a.host.rows(), ["shared", "phone"]);
    a.assert_same_as(&b);
}

/// When losing the role fails half way, the device stays read-only: the
/// next round settles the role again, and taking over still works.
#[test]
fn a_device_stays_read_only_when_losing_the_role_fails() {
    let (replica, clock) = setup();
    let mut a = Racing::new("laptop", &replica, &clock);
    a.host
        .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared');");
    a.connect().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    a.host.exec("INSERT INTO t VALUES ('written offline')");
    b.take_over().unwrap();
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();

    // Head says another device writes; reading the writer record fails.
    *a.host.fail.borrow_mut() = Some(WRITER_KEY);
    assert!(a.sync().is_err());
    assert!(a.host.read_only.get(), "read-only once it knows");
    assert_eq!(a.sync.role(), Role::Writer, "not settled yet");
    // Taking over first settles the role (the copy is kept), then takes it.
    a.take_over().unwrap();
    assert_eq!(a.sync.role(), Role::Writer);
    assert!(!a.host.read_only.get());
    let copies = std::fs::read_dir(&a.host.conflicts).unwrap().count();
    assert_eq!(copies, 1);
    a.host.exec("INSERT INTO t VALUES ('laptop again')");
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.rows(), ["shared", "phone", "laptop again"]);
    a.assert_same_as(&b);
}

/// A writer whose storage was reset, its device state kept, resumes from
/// the replica it wrote. It writes until it has read head, so a request
/// that arrives meanwhile is kept with what it wrote after the reset; from
/// then until that is decided it is read-only. What it wrote is kept as a
/// copy, not lost when the replica's state replaces the database.
#[test]
fn a_writer_whose_storage_was_reset_keeps_what_it_wrote_since() {
    for (call, prefix) in [("get", HEAD_KEY), ("saved", "")] {
        let (replica, clock) = setup();
        let mut a = Racing::new("laptop", &replica, &clock);
        a.host
            .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
        a.connect().unwrap();
        for i in 0..4 {
            a.host.exec(&format!("INSERT INTO t VALUES ('row {i}')"));
        }
        a.sync().unwrap();
        let state = store_load_state(&a.host.store.borrow()).unwrap().unwrap();
        assert_eq!(state.role, Role::Writer);

        // The laptop's shared store and level-0 files are gone, its device
        // state is not. It writes before it connects, as a writer may
        // offline.
        let mut b = Racing::new("laptop", &replica, &clock);
        store_save_state(&b.host.store.borrow(), &state).unwrap();
        b.host
            .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('after the reset');");
        b.arm(call, prefix);
        b.connect().unwrap();
        assert_eq!(b.sync.role(), Role::Writer);
        assert!(!b.host.read_only.get());
        b.assert_nothing_lost();
        let copies: Vec<PathBuf> = std::fs::read_dir(&b.host.conflicts)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert_eq!(copies.len(), 1, "{copies:?}");
        if call == "get" {
            assert!(b.host.refused.borrow().is_empty());
            assert_eq!(
                copy_rows(&copies[0]),
                ["after the reset", "written during get moul/head.json"]
            );
        } else {
            assert_eq!(b.host.refused.borrow().len(), 1);
            assert_eq!(copy_rows(&copies[0]), ["after the reset"]);
        }
        assert_eq!(b.host.rows(), ["one", "row 0", "row 1", "row 2", "row 3"]);

        b.host.exec("INSERT INTO t VALUES ('laptop again')");
        b.sync().unwrap();
        let mut c = device("phone", &replica, &clock);
        c.connect().unwrap();
        assert_eq!(c.rows().last().map(String::as_str), Some("laptop again"));
        b.assert_same_as(&c);
    }
}

/// A replica that loses the next put to one key.
struct LosePut(MemoryReplica, Rc<RefCell<Option<String>>>);

impl Replica for LosePut {
    fn describe(&self) -> String {
        self.0.describe()
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<Option<Object>>> {
        self.0.get(key)
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, moul_sync::Result<Put>> {
        if self.1.borrow().as_deref() == Some(key) {
            *self.1.borrow_mut() = None;
            return Box::pin(async {
                Err(moul_sync::Error::new(ErrorKind::Network, "connection lost"))
            });
        }
        self.0.put(key, bytes, condition)
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, moul_sync::Result<Vec<ObjectInfo>>> {
        self.0.list(prefix)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<()>> {
        self.0.delete(key)
    }
}

/// The head the replica holds.
fn head(replica: &MemoryReplica) -> moul_sync::layout::Head {
    serde_json::from_slice(&replica.object(HEAD_KEY).unwrap()).unwrap()
}

/// A laptop that writes and a phone whose take-over failed once the writer
/// record named it: the laptop's file landed without head, and the phone's
/// fence after it was lost.
fn half_taken_over() -> (MemoryReplica, Device, Device) {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    let lose = Rc::new(RefCell::new(None));
    let (r, l) = (replica.clone(), lose.clone());
    let mut b = Device::new(
        "phone",
        move || Box::new(LosePut(r.clone(), l.clone())),
        &clock,
        Retention::default(),
    );
    b.connect().unwrap();
    a.exec("INSERT INTO t VALUES ('late')");
    let pos = a.status().position;
    let late = a.capture.read(Txid(pos)).unwrap();
    replica.insert(&ltx_key(0, Txid(pos), Txid(pos)), &late);
    *lose.borrow_mut() = Some(ltx_key(0, Txid(pos + 1), Txid(pos + 1)));
    let e = b.take_over().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Network, "{e}");
    assert_eq!(b.sync.status(&b.capture).writer.unwrap().generation, 2);
    (replica, a, b)
}

/// A take-over that fails once the writer record names the device is
/// finished by the next round before the device writes: it takes what the
/// previous writer pushed meanwhile, fences after it and writes head of
/// its generation, so the previous writer stops at its next upload.
#[test]
fn a_take_over_that_stopped_half_way_is_finished_before_writing() {
    let (replica, mut a, mut b) = half_taken_over();

    // The laptop has seen no head of a new generation: it still writes.
    a.exec("INSERT INTO t VALUES ('laptop, still writing')");
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Writer);

    // The phone's next round finishes taking over.
    b.sync().unwrap();
    assert_eq!(b.role(), Role::Writer);
    let h = head(&replica);
    assert_eq!((h.writer, h.generation), (b.status().device, 2));
    assert!(b.conflict_copies().is_empty(), "its own fence is no change");
    assert_eq!(b.rows(), ["one", "late", "laptop, still writing"]);
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();
    assert_eq!(b.status().pending, 0);
    assert_eq!(head(&replica).txid, b.status().position);

    // The laptop's next upload hits the fence: it keeps a copy and follows.
    a.exec("INSERT INTO t VALUES ('laptop, too late')");
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    let copies = a.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(
        copy_rows(&copies[0]),
        ["one", "late", "laptop, still writing", "laptop, too late"]
    );
    assert_eq!(a.rows(), ["one", "late", "laptop, still writing", "phone"]);
    assert_converged(&replica, &[&a, &b]);
}

/// After a restart the session's fence is forgotten. A host that writes
/// although loading the device state told it the device follows (the test
/// device does not refuse writes) has what it wrote kept as a copy when
/// the fence is refused: the previous writer wrote that TXID first.
#[test]
fn a_take_over_finished_after_a_restart_keeps_what_was_written_meanwhile() {
    let (replica, mut a, mut b) = half_taken_over();
    a.exec("INSERT INTO t VALUES ('laptop, still writing')");
    a.sync().unwrap();
    b.restart();
    b.exec("INSERT INTO t VALUES ('phone, before connecting')");
    b.sync().unwrap();
    assert_eq!(b.role(), Role::Writer);
    assert_eq!(head(&replica).writer, b.status().device);
    let copies = b.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(
        copy_rows(&copies[0]),
        ["one", "late", "phone, before connecting"]
    );
    assert_eq!(b.rows(), ["one", "late", "laptop, still writing"]);
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    assert!(a.conflict_copies().is_empty());
    assert_converged(&replica, &[&a, &b]);
}

/// A device whose next put to one key is lost, and that key's cell.
fn losing(
    name: &str,
    replica: &MemoryReplica,
    clock: &Clock,
) -> (Device, Rc<RefCell<Option<String>>>) {
    let lose = Rc::new(RefCell::new(None));
    let (r, l) = (replica.clone(), lose.clone());
    let d = Device::new(
        name,
        move || Box::new(LosePut(r.clone(), l.clone())),
        clock,
        Retention::default(),
    );
    (d, lose)
}

/// A writer whose storage was reset, its device state kept, and that wrote
/// as many TXIDs before connecting as the replica holds is not taken for
/// current: the files it holds are not the replica's. It keeps them as a
/// copy and takes the replica's state, instead of pushing on top of it.
#[test]
fn a_reset_writer_that_wrote_up_to_head_keeps_a_copy_instead_of_pushing() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    a.exec("INSERT INTO t VALUES ('two')");
    a.sync().unwrap();
    let state = a.load_state().unwrap();

    let mut b = device("laptop", &replica, &clock);
    store_save_state(b.store(), &state).unwrap();
    b.exec("CREATE TABLE t (v TEXT)");
    b.exec("INSERT INTO t VALUES ('reset 1')");
    b.exec("INSERT INTO t VALUES ('reset 2')");
    assert!(b.status().position > head(&replica).txid);
    b.connect().unwrap();
    assert_eq!(b.role(), Role::Writer);
    assert_eq!(b.rows(), ["one", "two"]);
    let copies = b.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copy_rows(&copies[0]), ["reset 1", "reset 2"]);

    b.exec("INSERT INTO t VALUES ('laptop again')");
    b.sync().unwrap();
    let mut c = device("phone", &replica, &clock);
    c.connect().unwrap();
    assert_eq!(c.rows(), ["one", "two", "laptop again"]);
    assert_converged(&replica, &[&b, &c]);
}

/// A writer whose files landed but whose head write was lost, and whose
/// storage was then reset, takes the replica's state with those files, and
/// pushes on from the last of them.
#[test]
fn a_reset_writer_takes_the_files_it_pushed_past_head() {
    let (replica, clock) = setup();
    let (mut a, lose) = losing("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    for i in 0..3 {
        a.exec(&format!("INSERT INTO t VALUES ('row {i}')"));
    }
    a.sync().unwrap();
    a.exec("INSERT INTO t VALUES ('late 1'); INSERT INTO t VALUES ('late 2');");
    *lose.borrow_mut() = Some(HEAD_KEY.to_owned());
    assert_eq!(a.sync().unwrap_err().kind(), ErrorKind::Network);
    assert_eq!(head(&replica).txid + 2, a.status().position);
    let state = a.load_state().unwrap();

    let mut b = device("laptop", &replica, &clock);
    store_save_state(b.store(), &state).unwrap();
    assert!(b.status().position < head(&replica).txid);
    b.connect().unwrap();
    assert_eq!(b.role(), Role::Writer);
    let rows = ["one", "row 0", "row 1", "row 2", "late 1", "late 2"];
    assert_eq!(b.rows(), rows);
    assert_eq!(head(&replica).txid, b.status().position);
    b.exec("INSERT INTO t VALUES ('after the reset')");
    b.sync().unwrap();
    assert_eq!(b.status().pending, 0);
    assert!(b.conflict_copies().is_empty());
    let mut c = device("phone", &replica, &clock);
    c.connect().unwrap();
    assert_eq!(c.rows().len(), rows.len() + 1);
    assert_eq!(c.rows().last().map(String::as_str), Some("after the reset"));
    assert_converged(&replica, &[&b, &c]);
}

/// A writer that restarts while the replica is out of reach keeps writing:
/// it is made read-only only once it has read head, so a round that fails
/// before that leaves it writable.
#[test]
fn a_writer_that_restarts_offline_keeps_writing() {
    let (replica, clock) = setup();
    let mut a = Racing::new("laptop", &replica, &clock);
    a.host
        .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    a.restart("laptop", &replica, &clock);
    assert!(!a.host.read_only.get());

    // Head cannot be read, then nothing can.
    *a.host.fail.borrow_mut() = Some(HEAD_KEY);
    assert!(a.sync().is_err());
    assert!(!a.host.read_only.get(), "read-only once head failed");
    *a.host.fail.borrow_mut() = Some(WRITER_KEY);
    assert!(a.sync().is_err());
    assert!(!a.host.read_only.get(), "read-only while offline");
    a.host.exec("INSERT INTO t VALUES ('offline')");

    a.sync().unwrap();
    assert_eq!(a.sync.role(), Role::Writer);
    assert!(!a.host.read_only.get());
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    assert_eq!(b.rows(), ["one", "offline"]);
    a.assert_same_as(&b);
}

/// A take-over cut short once the writer record named the device is
/// finished after a restart without a copy when nothing conflicts: the
/// replica holds nothing past what the device holds as its own, so what it
/// wrote since is pushed. Its fence landed and head was lost, so the host
/// writes; or the fence was lost, and the host writes although told the
/// device follows (the test device does not refuse writes).
#[test]
fn a_take_over_cut_short_pushes_what_was_written_after_a_restart() {
    for fence_lost in [false, true] {
        let (replica, clock) = setup();
        let mut a = device("laptop", &replica, &clock);
        a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
        a.connect().unwrap();
        let (mut b, lose) = losing("phone", &replica, &clock);
        b.connect().unwrap();
        let next = b.status().position + 1;
        *lose.borrow_mut() = Some(if fence_lost {
            ltx_key(0, Txid(next), Txid(next))
        } else {
            HEAD_KEY.to_owned()
        });
        assert_eq!(b.take_over().unwrap_err().kind(), ErrorKind::Network);

        b.restart();
        b.exec("INSERT INTO t VALUES ('phone, after the restart')");
        b.sync().unwrap();
        assert_eq!(b.role(), Role::Writer);
        let copies = b.conflict_copies();
        assert!(copies.is_empty(), "fence lost {fence_lost}: {copies:?}");
        assert_eq!(b.rows(), ["one", "phone, after the restart"]);
        let h = head(&replica);
        assert_eq!((h.writer, h.txid), (b.status().device, b.status().position));

        a.sync().unwrap();
        assert_eq!(a.role(), Role::Follower);
        assert!(a.conflict_copies().is_empty());
        assert_eq!(a.rows(), ["one", "phone, after the restart"]);
        assert_converged(&replica, &[&a, &b]);
    }
}

/// A take-over cut short before its fence landed stays read-only across a
/// restart, until the next round has fenced. That round keeps no copy:
/// the device holds nothing of its own but the fence, whether the fence
/// goes up now or the previous writer wrote that TXID first.
#[test]
fn a_take_over_cut_short_before_the_fence_stays_read_only_across_a_restart() {
    for rival in [false, true] {
        let (replica, clock) = setup();
        let mut a = device("laptop", &replica, &clock);
        a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
        a.connect().unwrap();
        let mut b = Racing::new("phone", &replica, &clock);
        b.connect().unwrap();
        let next = b.host.capture.position().txid.0 + 1;
        *b.host.lose.borrow_mut() = Some(ltx_key(0, Txid(next), Txid(next)));
        assert_eq!(b.take_over().unwrap_err().kind(), ErrorKind::Network);
        if rival {
            a.exec("INSERT INTO t VALUES ('laptop, still writing')");
            a.sync().unwrap();
            assert_eq!(a.role(), Role::Writer);
        }

        b.restart("phone", &replica, &clock);
        assert!(b.host.read_only.get(), "rival {rival}: writable unfenced");
        b.sync().unwrap();
        assert_eq!(b.sync.role(), Role::Writer);
        assert!(!b.host.read_only.get());
        assert_eq!(
            std::fs::read_dir(&b.host.conflicts).unwrap().count(),
            0,
            "rival {rival}: a copy of the fence alone"
        );
        let mut expected = vec!["one"];
        if rival {
            expected.push("laptop, still writing");
        }
        assert_eq!(b.host.rows(), expected);
        b.host.exec("INSERT INTO t VALUES ('phone')");
        b.sync().unwrap();

        // The laptop's next upload hits the fence.
        a.exec("INSERT INTO t VALUES ('laptop, too late')");
        a.sync().unwrap();
        assert_eq!(a.role(), Role::Follower);
        assert_eq!(a.conflict_copies().len(), 1);
        expected.push("phone");
        assert_eq!(a.rows(), expected);
        b.assert_same_as(&a);
    }
}

/// A writer whose head write failed after its file landed writes head on
/// its next round, idle or not, before it releases the file: a restart
/// then resumes from head, and a head behind a released file would leave
/// the device unable to push.
#[test]
fn a_writer_whose_head_write_failed_writes_head_before_releasing() {
    let (replica, clock) = setup();
    let (mut a, lose) = losing("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    a.exec("INSERT INTO t VALUES ('two')");
    *lose.borrow_mut() = Some(HEAD_KEY.to_owned());
    assert_eq!(a.sync().unwrap_err().kind(), ErrorKind::Network);
    assert_eq!(head(&replica).txid + 1, a.status().position);

    // An idle round: nothing to upload, and maintenance is due.
    a.sync().unwrap();
    assert_eq!(head(&replica).txid, a.status().position);
    assert_eq!(a.capture.first_held(), None, "released once head is in");

    a.restart();
    a.sync().unwrap();
    a.exec("INSERT INTO t VALUES ('three')");
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Writer);
    assert_eq!(a.status().pending, 0);
    assert_eq!(head(&replica).txid, a.status().position);
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    assert_eq!(b.rows(), ["one", "two", "three"]);
    assert_converged(&replica, &[&a, &b]);
}

/// The device that created the replica lost its first head write, then its
/// storage was reset with its device state kept, and it wrote before it
/// connected. Its first TXID is not the replica's: it keeps a copy, takes
/// the replica's state and still writes, across a restart too.
#[test]
fn a_reset_creator_whose_first_head_was_lost_keeps_writing() {
    let (replica, clock) = setup();
    let (mut a, lose) = losing("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    *lose.borrow_mut() = Some(HEAD_KEY.to_owned());
    assert_eq!(a.connect().unwrap_err().kind(), ErrorKind::Network);
    assert!(replica.object(HEAD_KEY).is_none());
    let state = a.load_state().unwrap();

    let mut b = device("laptop", &replica, &clock);
    store_save_state(b.store(), &state).unwrap();
    b.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('reset 1');");
    b.connect().unwrap();
    assert_eq!(b.role(), Role::Writer);
    assert_eq!(b.rows(), ["one"]);
    let copies = b.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copy_rows(&copies[0]), ["reset 1"]);

    b.restart();
    b.sync().unwrap();
    assert_eq!(b.role(), Role::Writer);
    b.exec("INSERT INTO t VALUES ('laptop again')");
    b.sync().unwrap();
    assert_eq!(b.status().pending, 0);
    assert_eq!(head(&replica).txid, b.status().position);
    let mut c = device("phone", &replica, &clock);
    c.connect().unwrap();
    assert_eq!(c.rows(), ["one", "laptop again"]);
    assert_converged(&replica, &[&b, &c]);
}

/// What runs right before a put.
type Hook = Box<dyn FnOnce()>;

/// A replica whose next read of the writer record answers with an old
/// record, and which runs a hook right before the next put of one key: a
/// request that lands while another device is part way through.
#[derive(Clone)]
struct Hooked {
    inner: MemoryReplica,
    stale_writer: Rc<RefCell<Option<Vec<u8>>>>,
    hook: Rc<RefCell<Option<(String, Hook)>>>,
}

impl Hooked {
    fn new(replica: &MemoryReplica) -> Hooked {
        Hooked {
            inner: replica.clone(),
            stale_writer: Rc::new(RefCell::new(None)),
            hook: Rc::new(RefCell::new(None)),
        }
    }

    fn device(&self, name: &str, clock: &Clock) -> Device {
        let r = self.clone();
        Device::new(
            name,
            move || Box::new(r.clone()),
            clock,
            Retention::default(),
        )
    }
}

impl Replica for Hooked {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<Option<Object>>> {
        if key == WRITER_KEY
            && let Some(bytes) = self.stale_writer.borrow_mut().take()
        {
            return Box::pin(async move {
                Ok(Some(Object {
                    bytes,
                    etag: "\"stale\"".into(),
                }))
            });
        }
        self.inner.get(key)
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, moul_sync::Result<Put>> {
        let due = self.hook.borrow().as_ref().is_some_and(|(k, _)| k == key);
        if due && let Some((_, hook)) = self.hook.borrow_mut().take() {
            hook();
        }
        self.inner.put(key, bytes, condition)
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, moul_sync::Result<Vec<ObjectInfo>>> {
        self.inner.list(prefix)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<()>> {
        self.inner.delete(key)
    }
}

/// A writer whose storage was reset resumes from a writer record read just
/// before another device took over. The new writer's fence is in, its head
/// not yet, so the replica's state the reset writer takes holds the fence.
/// It finds the record has moved on and follows, instead of writing head
/// of the old generation and pushing on top of the fence.
#[test]
fn a_reset_writer_that_takes_a_new_writers_fence_follows() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    a.exec("INSERT INTO t VALUES ('two')");
    a.sync().unwrap();
    let state = a.load_state().unwrap();
    let old_record = replica.object(WRITER_KEY).unwrap();
    drop(a);

    let reset = Hooked::new(&replica);
    *reset.stale_writer.borrow_mut() = Some(old_record);
    let a = Rc::new(RefCell::new(reset.device("laptop", &clock)));
    store_save_state(a.borrow().store(), &state).unwrap();

    let phone = Hooked::new(&replica);
    let mut b = phone.device("phone", &clock);
    b.connect().unwrap();
    let a2 = a.clone();
    *phone.hook.borrow_mut() = Some((
        HEAD_KEY.to_owned(),
        Box::new(move || {
            let mut a = a2.borrow_mut();
            a.connect().unwrap();
            if a.role() == Role::Writer {
                a.exec("INSERT INTO t VALUES ('laptop, after the reset')");
                a.sync().unwrap();
            }
        }),
    ));
    b.take_over().unwrap();
    assert_eq!(a.borrow().role(), Role::Follower);
    assert_eq!(b.role(), Role::Writer);
    let h = head(&replica);
    assert_eq!((h.writer, h.generation), (b.status().device, 2));

    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();
    assert_eq!(b.role(), Role::Writer);
    assert!(b.conflict_copies().is_empty());
    a.borrow_mut().sync().unwrap();
    assert_eq!(a.borrow().role(), Role::Follower);
    assert!(a.borrow().conflict_copies().is_empty());
    b.restart();
    b.exec("INSERT INTO t VALUES ('phone, after a restart')");
    b.sync().unwrap();
    assert_eq!(b.role(), Role::Writer);
    assert_eq!(b.status().pending, 0);
    a.borrow_mut().sync().unwrap();
    assert_eq!(
        a.borrow().rows(),
        ["one", "two", "phone", "phone, after a restart"]
    );
    assert_converged(&replica, &[&a.borrow(), &b]);
}

/// A take-over whose head write is lost right after the replica was
/// created: the previous writer's next upload hits the fence, and the new
/// writer finishes taking over on its next round, before and after a
/// restart.
#[test]
fn a_take_over_of_a_new_replica_whose_head_was_lost_is_finished() {
    for restart in [false, true] {
        let (replica, clock) = setup();
        let mut a = device("laptop", &replica, &clock);
        a.connect().unwrap();
        assert_eq!(a.role(), Role::Writer);
        let (mut b, lose) = losing("phone", &replica, &clock);
        b.connect().unwrap();
        *lose.borrow_mut() = Some(HEAD_KEY.to_owned());
        assert_eq!(b.take_over().unwrap_err().kind(), ErrorKind::Network);

        a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('laptop');");
        a.sync().unwrap();
        assert_eq!(a.role(), Role::Follower);
        assert_eq!(a.conflict_copies().len(), 1);
        if restart {
            b.restart();
        }
        b.sync().unwrap();
        assert_eq!(b.role(), Role::Writer);
        b.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('phone');");
        b.sync().unwrap();
        assert_eq!(b.status().pending, 0);
        a.sync().unwrap();
        assert_eq!(a.rows(), ["phone"]);
        assert_converged(&replica, &[&a, &b]);
    }
}

/// A writer whose head lags behind files it pushed and released (as one
/// whose head write failed and that released them anyway did) resumes
/// from head after a restart. The files it no longer holds are the
/// replica's, byte for byte in what they add up to, so it pushes on from
/// the last of them instead of failing every round.
#[test]
fn a_writer_whose_head_lags_its_released_files_pushes_on() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    a.sync().unwrap();
    let lagging = replica.object(HEAD_KEY).unwrap();
    a.exec("INSERT INTO t VALUES ('two')");
    a.exec("INSERT INTO t VALUES ('three')");
    a.sync().unwrap();
    assert_eq!(a.capture.first_held(), None);
    replica.insert(HEAD_KEY, &lagging);
    assert!(head(&replica).txid + 2 <= a.status().position);

    a.restart();
    a.exec("INSERT INTO t VALUES ('four')");
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Writer);
    assert!(a.conflict_copies().is_empty());
    assert_eq!(a.status().pending, 0);
    assert_eq!(head(&replica).txid, a.status().position);
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    assert_eq!(b.rows(), ["one", "two", "three", "four"]);
    assert_converged(&replica, &[&a, &b]);
}

/// The same, but the replica's file for a TXID the writer no longer holds
/// is not the one it pushed. The writer keeps its database as a copy and
/// takes the replica's state, then writes on from it.
#[test]
fn a_writer_whose_released_files_differ_from_the_replicas_keeps_a_copy() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    a.sync().unwrap();
    let mut b = device("phone", &replica, &clock);
    b.connect().unwrap();
    let lagging = replica.object(HEAD_KEY).unwrap();
    a.exec("INSERT INTO t VALUES ('two')");
    a.sync().unwrap();
    assert_eq!(a.capture.first_held(), None);
    let txid = Txid(a.status().position);
    let key = ltx_key(0, txid, txid);
    assert!(
        !replica.keys().iter().any(|k| !k.starts_with("ltx/0/")
            && moul_sync::layout::parse_ltx_key(k).is_some_and(|(_, _, max)| max >= txid)),
        "only level 0 holds {txid}"
    );
    // Another file for that TXID, from the same state.
    b.exec("INSERT INTO t VALUES ('other')");
    assert_eq!(b.status().position, txid.0);
    replica.insert(&key, &b.capture.read(txid).unwrap());
    replica.insert(HEAD_KEY, &lagging);
    drop(b);

    a.restart();
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Writer);
    assert_eq!(a.rows(), ["one", "other"]);
    let copies = a.conflict_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copy_rows(&copies[0]), ["one", "two"]);
    a.exec("INSERT INTO t VALUES ('laptop again')");
    a.sync().unwrap();
    assert_eq!(a.status().pending, 0);
    assert_eq!(head(&replica).txid, a.status().position);
    let mut c = device("phone", &replica, &clock);
    c.connect().unwrap();
    assert_eq!(c.rows(), ["one", "other", "laptop again"]);
    assert_converged(&replica, &[&a, &c]);
}

// ---- A take-over racing another --------------------------------------------

/// A replica with one-shot hooks before a get or a put of a key, and one
/// put of a key that is lost.
#[derive(Clone)]
struct Gate {
    inner: MemoryReplica,
    before_get: Rc<RefCell<Option<(String, Hook)>>>,
    before_put: Rc<RefCell<Option<(String, Hook)>>>,
    lose: Rc<RefCell<Option<String>>>,
}

impl Gate {
    fn new(replica: &MemoryReplica) -> Gate {
        Gate {
            inner: replica.clone(),
            before_get: Rc::new(RefCell::new(None)),
            before_put: Rc::new(RefCell::new(None)),
            lose: Rc::new(RefCell::new(None)),
        }
    }
    fn device(&self, name: &str, clock: &Clock) -> Device {
        let r = self.clone();
        Device::new(
            name,
            move || Box::new(r.clone()),
            clock,
            Retention::default(),
        )
    }
}

impl Replica for Gate {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<Option<Object>>> {
        let due = self
            .before_get
            .borrow()
            .as_ref()
            .is_some_and(|(k, _)| k == key);
        if due && let Some((_, hook)) = self.before_get.borrow_mut().take() {
            hook();
        }
        self.inner.get(key)
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, moul_sync::Result<Put>> {
        if self.lose.borrow().as_deref() == Some(key) {
            *self.lose.borrow_mut() = None;
            return Box::pin(async {
                Err(moul_sync::Error::new(ErrorKind::Network, "connection lost"))
            });
        }
        let due = self
            .before_put
            .borrow()
            .as_ref()
            .is_some_and(|(k, _)| k == key);
        if due && let Some((_, hook)) = self.before_put.borrow_mut().take() {
            hook();
        }
        self.inner.put(key, bytes, condition)
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, moul_sync::Result<Vec<ObjectInfo>>> {
        self.inner.list(prefix)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<()>> {
        self.inner.delete(key)
    }
}

fn writer_record(replica: &MemoryReplica) -> moul_sync::layout::WriterRecord {
    serde_json::from_slice(&replica.object(WRITER_KEY).unwrap()).unwrap()
}

/// Two devices only. The phone takes over; between its record and its
/// fence, the laptop (the old writer) connects, loses the role, and takes
/// writing back at once: record gen 3, fence, head. The phone's fence is
/// refused; its fence loop takes the laptop's fence in and fences on top.
#[test]
fn a_take_over_does_not_fence_on_top_of_a_newer_one() {
    let (replica, clock) = setup();
    let a = Rc::new(RefCell::new(device("laptop", &replica, &clock)));
    a.borrow()
        .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.borrow_mut().connect().unwrap();
    let gate = Gate::new(&replica);
    let mut b = gate.device("phone", &clock);
    b.connect().unwrap();
    let n = b.status().position;
    let a2 = a.clone();
    *gate.before_put.borrow_mut() = Some((
        ltx_key(0, Txid(n + 1), Txid(n + 1)),
        Box::new(move || {
            let mut a = a2.borrow_mut();
            a.connect().unwrap();
            assert_eq!(a.role(), Role::Follower);
            a.take_over().unwrap();
            assert_eq!(a.role(), Role::Writer);
        }),
    ));
    let r = b.take_over();
    eprintln!("phone take_over: {r:?}, role {:?}", b.role());
    let rec = writer_record(&replica);
    let h = head(&replica);
    eprintln!(
        "record gen {} {}, head gen {} txid {} by {}",
        rec.generation, rec.device, h.generation, h.txid, h.writer
    );
    eprintln!("files: {:?}", replica.keys());
    let mut a = a.borrow_mut();
    a.exec("INSERT INTO t VALUES ('laptop, writer of record')");
    let r = a.sync();
    eprintln!(
        "laptop sync: {r:?}, role {:?}, copies {:?}",
        a.role(),
        a.conflict_copies()
    );
    a.sync().unwrap();
    b.sync().unwrap();
    eprintln!(
        "after another round: laptop {:?}, phone {:?}, record names laptop: {}",
        a.role(),
        b.role(),
        rec.device == a.status().device
    );
    assert_eq!(rec.device, a.status().device);
    assert!(
        a.conflict_copies().is_empty(),
        "the writer of record's write was moved to a copy"
    );
    assert_eq!(a.role(), Role::Writer, "the writer of record follows");
}

/// Two devices only, through finish_take_over's own resync rather than the
/// fence loop. The phone's take-over stopped before its fence landed; the
/// laptop pushed past it. The phone restarts and finishes taking over;
/// while it compares its fence, the laptop connects, follows, and takes
/// writing back. The phone resyncs (taking in the laptop's fence) and
/// fences on top.
#[test]
fn finishing_a_take_over_does_not_fence_on_top_of_a_newer_one() {
    let (replica, clock) = setup();
    let a = Rc::new(RefCell::new(device("laptop", &replica, &clock)));
    a.borrow()
        .exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.borrow_mut().connect().unwrap();
    let gate = Gate::new(&replica);
    let mut b = gate.device("phone", &clock);
    b.connect().unwrap();
    let n = b.status().position;
    *gate.lose.borrow_mut() = Some(ltx_key(0, Txid(n + 1), Txid(n + 1)));
    assert_eq!(b.take_over().unwrap_err().kind(), ErrorKind::Network);
    a.borrow()
        .exec("INSERT INTO t VALUES ('laptop, still writing')");
    a.borrow_mut().sync().unwrap();
    assert_eq!(a.borrow().role(), Role::Writer);

    let a2 = a.clone();
    *gate.before_get.borrow_mut() = Some((
        ltx_key(0, Txid(n + 1), Txid(n + 1)),
        Box::new(move || {
            let mut a = a2.borrow_mut();
            a.connect().unwrap();
            assert_eq!(a.role(), Role::Follower);
            a.take_over().unwrap();
            assert_eq!(a.role(), Role::Writer);
        }),
    ));
    b.restart();
    let r = b.sync();
    eprintln!("phone sync: {r:?}, role {:?}", b.role());
    let rec = writer_record(&replica);
    let h = head(&replica);
    eprintln!(
        "record gen {} {}, head gen {} txid {} by {}",
        rec.generation, rec.device, h.generation, h.txid, h.writer
    );
    eprintln!("files: {:?}", replica.keys());
    let mut a = a.borrow_mut();
    a.exec("INSERT INTO t VALUES ('laptop, writer of record')");
    let r = a.sync();
    eprintln!(
        "laptop sync: {r:?}, role {:?}, copies {:?}",
        a.role(),
        a.conflict_copies()
    );
    assert_eq!(rec.device, a.status().device);
    assert!(
        a.conflict_copies().is_empty(),
        "the writer of record's write was moved to a copy"
    );
    assert_eq!(a.role(), Role::Writer, "the writer of record follows");
}

// ---- Random faults --------------------------------------------------------
//
// Two devices run random steps (writes, syncs, take-overs, restarts, storage
// resets, writes arriving mid-call) against a replica that loses requests or
// their answers at random. After every run: no acknowledged write is lost
// (it is in the replica's history or a conflict copy), the replica never
// holds a history from two writers, and the devices converge. A failing seed
// replays with MOUL_SYNC_FUZZ_SEED=<seed> (and MOUL_SYNC_FUZZ_RESETS=1)
// `cargo test -p moul-sync --test scenarios random_fault_seed -- --ignored --nocapture`.
// MOUL_SYNC_FUZZ_SEEDS and MOUL_SYNC_FUZZ_STEPS run more.

trait LabelOr {
    fn label_or(&self) -> String;
}
impl LabelOr for moul_sync::layout::Head {
    fn label_or(&self) -> String {
        self.writer.chars().take(4).collect()
    }
}

fn fuzz_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "replays one seed: set MOUL_SYNC_FUZZ_SEED"]
fn random_fault_seed() {
    let seed = fuzz_env("MOUL_SYNC_FUZZ_SEED", 0);
    let resets = std::env::var("MOUL_SYNC_FUZZ_RESETS").is_ok();
    fuzz_run(
        seed,
        fuzz_env("MOUL_SYNC_FUZZ_STEPS", 60) as u32,
        resets,
        true,
    )
    .unwrap();
}

fn writer_rec(replica: &MemoryReplica) -> moul_sync::layout::WriterRecord {
    serde_json::from_slice(&replica.object(WRITER_KEY).unwrap()).unwrap()
}

#[derive(Default)]
struct FaultCtl {
    /// Requests until the fault, and the fault.
    at: Cell<Option<(u32, moul_sync::Fault)>>,
    /// Fault the next request (method, key prefix).
    key: RefCell<Option<(&'static str, &'static str, moul_sync::Fault)>>,
    log: RefCell<Vec<String>>,
}

struct FuzzReplica(Memory, Rc<FaultCtl>, Rc<Racer>);

impl FuzzReplica {
    fn kfault(&self, m: &str, key: &str) -> Option<moul_sync::Fault> {
        let hit = matches!(*self.1.key.borrow(), Some((mm, p, _)) if mm == m && key.starts_with(p));
        if hit {
            let f = self.1.key.borrow_mut().take().unwrap().2;
            return Some(f);
        }
        self.fault()
    }
    fn fault(&self) -> Option<moul_sync::Fault> {
        match self.1.at.get() {
            Some((0, f)) => {
                self.1.at.set(None);
                Some(f)
            }
            Some((n, f)) => {
                self.1.at.set(Some((n - 1, f)));
                None
            }
            None => None,
        }
    }
}

fn lost() -> moul_sync::Error {
    moul_sync::Error::new(ErrorKind::Network, "fuzz: lost")
}

impl Replica for FuzzReplica {
    fn describe(&self) -> String {
        self.0.describe()
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<Option<Object>>> {
        Box::pin(async move {
            self.2.arrive("get", key);
            let f = self.kfault("get", key);
            if f == Some(moul_sync::Fault::Lost) {
                self.1.log.borrow_mut().push(format!("lost get {key}"));
                return Err(lost());
            }
            let r = self.0.get(key).await?;
            if f == Some(moul_sync::Fault::AnswerLost) {
                self.1
                    .log
                    .borrow_mut()
                    .push(format!("answer lost get {key}"));
                return Err(lost());
            }
            Ok(r)
        })
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, moul_sync::Result<Put>> {
        Box::pin(async move {
            let f = self.kfault("put", key);
            if f == Some(moul_sync::Fault::Lost) {
                self.1.log.borrow_mut().push(format!("lost put {key}"));
                return Err(lost());
            }
            let r = self.0.put(key, bytes, condition).await?;
            if f == Some(moul_sync::Fault::AnswerLost) {
                self.1
                    .log
                    .borrow_mut()
                    .push(format!("answer lost put {key} {r:?}"));
                return Err(lost());
            }
            Ok(r)
        })
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, moul_sync::Result<Vec<ObjectInfo>>> {
        Box::pin(async move {
            self.2.arrive("list", prefix);
            let f = self.kfault("list", prefix);
            if f.is_some() {
                self.1.log.borrow_mut().push(format!("lost list {prefix}"));
                return Err(lost());
            }
            self.0.list(prefix).await
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, moul_sync::Result<()>> {
        Box::pin(async move {
            let f = self.fault();
            if f == Some(moul_sync::Fault::Lost) {
                return Err(lost());
            }
            self.0.delete(key).await?;
            if f.is_some() {
                return Err(lost());
            }
            Ok(())
        })
    }
}

struct Fz {
    name: String,
    host: Rc<Racer>,
    sync: Sync,
    ctl: Rc<FaultCtl>,
    acked: Vec<String>,
}

fn fz_host(name: &str) -> Rc<Racer> {
    let ltx = MemoryStorage::new();
    let capture = Capture::open(ltx, "main.db").unwrap();
    let volume = Volume::open_observed(MemoryStorage::new(), capture.clone()).unwrap();
    Rc::new(Racer {
        store: RefCell::new(Store::open_follower(volume).unwrap()),
        capture,
        conflicts: common::scratch(&format!("fz-{name}-{}", moul_sync::new_id())),
        read_only: Cell::new(false),
        at: RefCell::new(None),
        fail: RefCell::new(None),
        lose: RefCell::new(None),
        acked: RefCell::new(Vec::new()),
        refused: RefCell::new(Vec::new()),
    })
}

impl Fz {
    fn new(name: &str, replica: &MemoryReplica, clock: &Clock) -> Fz {
        let host = fz_host(name);
        let ctl = Rc::new(FaultCtl::default());
        let sync = Fz::mk_sync(name, replica, &host, &ctl, clock);
        let mut f = Fz {
            name: name.into(),
            host,
            sync,
            ctl,
            acked: vec![],
        };
        f.sync.load(&mut RacerLocal(f.host.clone())).unwrap();
        f
    }
    fn mk_sync(
        name: &str,
        replica: &MemoryReplica,
        host: &Rc<Racer>,
        ctl: &Rc<FaultCtl>,
        clock: &Clock,
    ) -> Sync {
        let clock = clock.clone();
        Sync::new(
            FuzzReplica(replica.clone(), ctl.clone(), host.clone()),
            Options {
                label: name.to_owned(),
                poll_every: 0,
                maintain_every: 0,
                clock: Box::new(move || clock.now()),
                retention: if std::env::var("FZR").is_ok() {
                    Retention {
                        level1_every: 1,
                        level2_every: 2,
                        snapshot_every: 3,
                        compacted_kept: 1,
                        history: 5,
                    }
                } else {
                    Retention::default()
                },
                ..Options::default()
            },
        )
    }
    fn restart(&mut self, replica: &MemoryReplica, clock: &Clock) {
        self.host.read_only.set(false);
        self.sync = Fz::mk_sync(&self.name, replica, &self.host, &self.ctl, clock);
        self.sync.load(&mut RacerLocal(self.host.clone())).unwrap();
    }
    /// Storage reset with the device state kept.
    fn reset(&mut self, replica: &MemoryReplica, clock: &Clock) {
        let state = store_load_state(&self.host.store.borrow()).unwrap();
        let old = self.host.clone();
        let host = fz_host(&self.name);
        // keep the conflicts folder
        let host = Rc::new(Racer {
            store: RefCell::new(std::mem::replace(
                &mut *host.store.borrow_mut(),
                Store::open_follower(Volume::open(MemoryStorage::new()).unwrap()).unwrap(),
            )),
            capture: host.capture.clone(),
            conflicts: old.conflicts.clone(),
            read_only: Cell::new(false),
            at: RefCell::new(None),
            fail: RefCell::new(None),
            lose: RefCell::new(None),
            acked: RefCell::new(Vec::new()),
            refused: RefCell::new(Vec::new()),
        });
        if let Some(s) = state {
            store_save_state(&host.store.borrow(), &s).unwrap();
        }
        self.host = host;
        self.sync = Fz::mk_sync(&self.name, replica, &self.host, &self.ctl, clock);
        self.sync.load(&mut RacerLocal(self.host.clone())).unwrap();
    }
    fn write(&mut self, v: &str) -> bool {
        if self.host.read_only.get() {
            return false;
        }
        let st = self.host.store.borrow();
        st.conn()
            .execute_batch("CREATE TABLE IF NOT EXISTS t (v TEXT)")
            .unwrap();
        st.conn().execute("INSERT INTO t VALUES (?1)", [v]).unwrap();
        self.acked.push(v.to_owned());
        true
    }
    fn arm(&self, n: u32, f: moul_sync::Fault) {
        self.ctl.at.set(Some((n, f)));
    }
    fn disarm(&self) {
        self.ctl.at.set(None);
        *self.ctl.key.borrow_mut() = None;
        *self.host.at.borrow_mut() = None;
    }
    fn arm_key(&self, m: &'static str, p: &'static str, f: moul_sync::Fault) {
        *self.ctl.key.borrow_mut() = Some((m, p, f));
    }
    fn sync(&mut self) -> moul_sync::Result<()> {
        block_on(self.sync.sync(&mut RacerLocal(self.host.clone())))
    }
    fn take_over(&mut self) -> moul_sync::Result<()> {
        block_on(self.sync.take_over(&mut RacerLocal(self.host.clone())))
    }
    fn rows(&self) -> Vec<String> {
        let store = self.host.store.borrow();
        let conn = store.conn();
        let has: i64 = conn
            .query_row(
                "SELECT count(*) FROM main.sqlite_schema WHERE name='t'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if has == 0 {
            return vec![];
        }
        let mut stmt = conn.prepare("SELECT v FROM t ORDER BY rowid").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
    fn copies_rows(&self) -> Vec<String> {
        let mut out = vec![];
        if let Ok(d) = std::fs::read_dir(&self.host.conflicts) {
            for e in d.flatten() {
                let c = rusqlite::Connection::open(e.path()).unwrap();
                let has: i64 = c
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE name='t'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                if has == 0 {
                    continue;
                }
                let mut stmt = c.prepare("SELECT v FROM t").unwrap();
                out.extend(
                    stmt.query_map([], |r| r.get::<_, String>(0))
                        .unwrap()
                        .map(|r| r.unwrap()),
                );
            }
        }
        out
    }
    fn writable_writer(&self) -> bool {
        self.sync.role() == Role::Writer && !self.host.read_only.get()
    }
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn replica_rows(replica: &MemoryReplica) -> Vec<String> {
    let Ok((_, db)) = block_on(moul_sync::restore::restore_db(replica, None)) else {
        return vec![];
    };
    let dir = common::scratch(&format!("fzr-{}", moul_sync::new_id()));
    let p = dir.join("r.db");
    std::fs::write(&p, &db).unwrap();
    let c = rusqlite::Connection::open(&p).unwrap();
    let has: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='t'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    if has == 0 {
        return vec![];
    }
    let mut stmt = c.prepare("SELECT v FROM t").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn arm_any(d: &Fz, rng: &mut Lcg, fault: Option<moul_sync::Fault>, at: u32) -> String {
    let Some(f) = fault else {
        if rng.below(3) == 0 {
            let (c, p) = [
                ("get", "moul/head"),
                ("get", "moul/writer"),
                ("list", "ltx/"),
                ("get", "ltx/"),
                ("saved", ""),
            ][rng.below(5) as usize];
            *d.host.at.borrow_mut() = Some((c, p));
            return format!("arrive@{c}:{p}");
        }
        return String::new();
    };
    match rng.below(4) {
        0 => {
            d.arm_key("put", "moul/head", f);
            "key put head".into()
        }
        1 => {
            d.arm_key("put", "ltx/", f);
            "key put ltx".into()
        }
        2 => {
            d.arm_key("get", "moul/writer", f);
            "key get writer".into()
        }
        _ => {
            d.arm(at, f);
            format!("idx {at}")
        }
    }
}

// The two devices are addressed by index throughout: `d[j]` reads as "device j".
#[allow(clippy::needless_range_loop, clippy::useless_vec)]
fn fuzz_run(seed: u64, steps: u32, resets: bool, verbose: bool) -> Result<(), String> {
    let (replica, clock) = setup();
    let mut rng = Lcg(seed);
    let mut d = vec![
        Fz::new("laptop", &replica, &clock),
        Fz::new("phone", &replica, &clock),
    ];
    d[0].write("seed");
    for j in 0..2 {
        let h = d[j].host.clone();
        block_on(d[j].sync.connect(&mut RacerLocal(h))).unwrap();
    }
    let mut n = 0;
    let mut trace: Vec<String> = vec![];
    for _ in 0..steps {
        clock.advance(7);
        replica.set_now(clock.now());
        let i = rng.below(2) as usize;
        let fault = match rng.below(6) {
            0 => Some(moul_sync::Fault::Lost),
            1 => Some(moul_sync::Fault::AnswerLost),
            _ => None,
        };
        let at = rng.below(10) as u32;
        let act = rng.below(if resets { 21 } else { 20 });
        let both_before = d[0].writable_writer() && d[1].writable_writer();
        let what = match act {
            0..=7 => {
                n += 1;
                let v = format!("{}-{n}", d[i].name);
                let ok = d[i].write(&v);
                format!("{} write {v} ok={ok}", d[i].name)
            }
            8..=14 => {
                let kf = arm_any(&d[i], &mut rng, fault, at);
                let r = d[i].sync();
                let arrived = (
                    d[i].host.acked.borrow().len(),
                    d[i].host.refused.borrow().len(),
                );
                d[i].disarm();
                format!(
                    "{} sync fault={fault:?}@{at} {kf} -> {r:?} arrived {arrived:?}",
                    d[i].name
                )
            }
            15..=16 => {
                let _kf = arm_any(&d[i], &mut rng, fault, at);
                let r = d[i].take_over();
                d[i].disarm();
                format!("{} take_over fault={fault:?}@{at} -> {r:?}", d[i].name)
            }
            17..=18 => {
                d[i].restart(&replica, &clock);
                format!("{} restart", d[i].name)
            }
            19 => {
                clock.advance(rng.below(3 * 60 * 60_000) as i64);
                replica.set_now(clock.now());
                "advance".into()
            }
            _ => {
                // the reset loses what was not pushed: forget it.
                let mut kept = replica_rows(&replica);
                kept.extend(d[i].copies_rows());
                d[i].acked.retain(|v| kept.contains(v));
                d[i].reset(&replica, &clock);
                format!("{} RESET", d[i].name)
            }
        };
        let both = d[0].writable_writer() && d[1].writable_writer();
        let line = format!(
            "{what} | roles {:?}/{} {:?}/{} both={both}",
            d[0].sync.role(),
            d[0].host.read_only.get(),
            d[1].sync.role(),
            d[1].host.read_only.get()
        );
        let line = if verbose {
            let h = replica.object(HEAD_KEY).map(|_| {
                let h = head(&replica);
                (h.txid, h.generation, h.label_or())
            });
            let r = writer_rec(&replica);
            format!(
                "{line}\n     pos {}/{} pushed? rec gen {} {} since {} head {:?} ltx0 {:?}\n     L rows {:?} copies {:?}\n     P rows {:?} copies {:?} ctl {:?}",
                d[0].host.capture.position().txid.0,
                d[1].host.capture.position().txid.0,
                r.generation,
                r.label,
                r.since,
                h,
                replica
                    .keys()
                    .iter()
                    .filter(|k| k.starts_with("ltx/0/"))
                    .map(|k| k[6..22].trim_start_matches('0').to_string())
                    .collect::<Vec<_>>(),
                d[0].rows(),
                d[0].copies_rows(),
                d[1].rows(),
                d[1].copies_rows(),
                d[i].ctl.log.borrow().clone()
            )
        } else {
            line
        };
        d[i].ctl.log.borrow_mut().clear();
        if verbose {
            eprintln!("{line}");
        }
        trace.push(line);
        let _ = both_before;
    }
    // Quiesce.
    for _ in 0..6 {
        for j in 0..2 {
            clock.advance(3);
            let _ = d[j].sync();
        }
    }
    let writers: Vec<usize> = (0..2).filter(|&j| d[j].writable_writer()).collect();
    let rec = writer_rec(&replica);
    let state_desc = format!(
        "after quiesce: roles {:?}/{} {:?}/{}, record gen {} names {}, head {:?}",
        d[0].sync.role(),
        d[0].host.read_only.get(),
        d[1].sync.role(),
        d[1].host.read_only.get(),
        rec.generation,
        rec.label,
        replica.object(HEAD_KEY).map(|_| {
            let h = head(&replica);
            (h.txid, h.generation)
        })
    );
    if writers.len() != 1 {
        // A way out: take over on the device the record names, or device 0.
        let j = d
            .iter()
            .position(|f| f.sync.status(&f.host.capture).device == rec.device)
            .unwrap_or(0);
        clock.advance(3);
        let r = d[j].take_over();
        for _ in 0..3 {
            for k in 0..2 {
                clock.advance(3);
                let _ = d[k].sync();
            }
        }
        let writers2: Vec<usize> = (0..2).filter(|&j| d[j].writable_writer()).collect();
        if writers2.len() != 1 {
            return Err(format!(
                "seed {seed}: {} writers ({state_desc}); take_over -> {r:?}\n{}",
                writers.len(),
                trace.join("\n")
            ));
        }
    }
    let w = (0..2).find(|&j| d[j].writable_writer()).unwrap();
    // The writer can write and push; the other follows.
    d[w].write("final");
    for _ in 0..3 {
        for k in 0..2 {
            clock.advance(3);
            if let Err(e) = d[k].sync() {
                return Err(format!(
                    "seed {seed}: {} sync error {e} ({state_desc})\n{}",
                    d[k].name,
                    trace.join("\n")
                ));
            }
        }
    }
    let a = d[0].host.store.borrow().export().unwrap();
    let b = d[1].host.store.borrow().export().unwrap();
    if a != b {
        return Err(format!(
            "seed {seed}: not converged ({state_desc}) rows {:?} vs {:?}\n{}",
            d[0].rows(),
            d[1].rows(),
            trace.join("\n")
        ));
    }
    for j in 0..2 {
        let ok: String = d[j]
            .host
            .store
            .borrow()
            .conn()
            .query_row("PRAGMA main.integrity_check", [], |r| r.get(0))
            .unwrap();
        if ok != "ok" {
            return Err(format!("seed {seed}: integrity {ok}"));
        }
    }
    let final_rows = replica_rows(&replica);
    if final_rows != d[w].rows() {
        return Err(format!("seed {seed}: replica restores to other rows"));
    }
    for j in 0..2 {
        let mut seen = final_rows.clone();
        seen.extend(d[j].copies_rows());
        let mut acked = d[j].acked.clone();
        acked.extend(d[j].host.acked.borrow().iter().cloned());
        for v in &acked {
            if !seen.contains(v) {
                return Err(format!(
                    "seed {seed}: {} acked {v} LOST ({state_desc})\nfinal {:?}\n{}",
                    d[j].name,
                    final_rows,
                    trace.join("\n")
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn random_faults_lose_no_write() {
    let mut fails = vec![];
    let n = fuzz_env("MOUL_SYNC_FUZZ_SEEDS", 200);
    for seed in 0..n {
        if let Err(e) = fuzz_run(
            seed,
            fuzz_env("MOUL_SYNC_FUZZ_STEPS", 60) as u32,
            false,
            false,
        ) {
            fails.push(e);
        }
    }
    for f in fails.iter() {
        eprintln!("FAIL {}", f.lines().next().unwrap());
    }
    assert!(fails.is_empty(), "{} of {n} failed", fails.len());
}

#[test]
fn random_faults_and_storage_resets_lose_no_write() {
    let mut fails = vec![];
    let n = fuzz_env("MOUL_SYNC_FUZZ_SEEDS", 200);
    for seed in 1000..1000 + n {
        if let Err(e) = fuzz_run(
            seed,
            fuzz_env("MOUL_SYNC_FUZZ_STEPS", 60) as u32,
            true,
            false,
        ) {
            fails.push(e);
        }
    }
    for f in fails.iter() {
        eprintln!("FAIL {}", f.lines().next().unwrap());
    }
    assert!(fails.is_empty(), "{} of {n} failed", fails.len());
}

/// A take-over whose fence landed but whose head write was lost; then the
/// device's storage is reset (device state kept) and it writes exactly as
/// many TXIDs as the record's `since` before it connects again.
#[test]
fn a_reset_device_that_took_over_and_wrote_up_to_since_keeps_its_writes() {
    let (replica, clock) = setup();
    let mut a = device("laptop", &replica, &clock);
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    a.exec("INSERT INTO t VALUES ('two')");
    a.exec("INSERT INTO t VALUES ('three')");
    a.sync().unwrap();

    let (mut b, lose) = losing("phone", &replica, &clock);
    b.connect().unwrap();
    *lose.borrow_mut() = Some(HEAD_KEY.to_owned());
    assert_eq!(b.take_over().unwrap_err().kind(), ErrorKind::Network);
    let rec = writer_rec(&replica);
    let state = b.load_state().unwrap();
    assert!(!state.unfenced, "the fence is in");
    eprintln!(
        "record gen {} since {}; keys {:?}",
        rec.generation,
        rec.since,
        replica.keys()
    );

    // The phone's storage is reset; its device state is kept.
    let mut c = device("phone", &replica, &clock);
    store_save_state(c.store(), &state).unwrap();
    eprintln!("reset phone opens at {}", c.status().position);
    c.exec("CREATE TABLE t (v TEXT)");
    while c.status().position < rec.since {
        c.exec("INSERT INTO t VALUES ('phone, after the reset')");
    }
    assert_eq!(c.status().position, rec.since);
    eprintln!("reset phone rows before connecting: {:?}", c.rows());

    c.sync().unwrap();
    eprintln!(
        "role {:?}, rows {:?}, copies {:?}",
        c.role(),
        c.rows(),
        c.conflict_copies()
    );
    c.integrity();
    c.exec("INSERT INTO t VALUES ('phone, writing')");
    c.sync().unwrap();
    a.sync().unwrap();
    eprintln!("laptop role {:?}, rows {:?}", a.role(), a.rows());
    let mut seen = c.rows();
    for p in c.conflict_copies() {
        seen.extend(copy_rows(&p));
    }
    assert!(
        seen.iter().any(|v| v == "phone, after the reset"),
        "what the reset phone wrote is in neither its database nor a copy: {seen:?}"
    );
}
