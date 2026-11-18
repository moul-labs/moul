//! Crash safety: no failure at any step corrupts the database or loses a
//! commit that was reported as done.
//!
//! The workload runs on a memory disk that fails at step N and every step
//! after it. The disk then "loses power" in one of several ways, the volume
//! is reopened, and the result must be a valid database equal to the state
//! after some whole number of transactions: at least every transaction whose
//! COMMIT returned success, and at most one more (the one in flight, whose
//! log record may have reached storage although its flush reported failure).
//! The same is then done to recovery itself.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{connect, integrity};
use moul_vfs::{CrashMode, MemoryStorage, Volume};
use rusqlite::{Connection, params};

type Rows = (Vec<(i64, Vec<u8>)>, Vec<(i64, String)>);

const TXS: usize = 8;

fn run_tx(c: &Connection, i: usize) -> rusqlite::Result<()> {
    match i {
        0 => c.execute_batch(
            "BEGIN;
             CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB);
             CREATE TABLE writer.secrets (k INTEGER PRIMARY KEY, s TEXT);
             COMMIT;",
        ),
        1 => {
            c.execute_batch("BEGIN")?;
            for k in 0..60 {
                c.execute(
                    "INSERT INTO kv VALUES (?1, ?2)",
                    params![k, vec![k as u8; 700]],
                )?;
            }
            c.execute_batch("COMMIT")
        }
        2 => c.execute_batch("UPDATE kv SET v = CAST(printf('%0900d', k) AS BLOB) WHERE k % 2 = 0"),
        3 => {
            // Bigger than the cache, so pages are written before COMMIT.
            c.execute_batch("PRAGMA cache_size = 6; BEGIN")?;
            for k in 100..220 {
                c.execute(
                    "INSERT INTO kv VALUES (?1, ?2)",
                    params![k, vec![3u8; 2500]],
                )?;
            }
            c.execute_batch("COMMIT; PRAGMA cache_size = -2000;")
        }
        4 => {
            c.execute_batch("PRAGMA cache_size = 6; BEGIN; DELETE FROM kv WHERE k < 150;")?;
            c.execute_batch("ROLLBACK; PRAGMA cache_size = -2000;")?;
            c.execute_batch("UPDATE kv SET v = x'01' WHERE k = 1")
        }
        5 => c.execute_batch("DELETE FROM kv WHERE k >= 120"),
        6 => c.execute_batch("VACUUM"),
        7 => c.execute_batch(
            "BEGIN;
             INSERT INTO secrets VALUES (1, 'hash-1');
             UPDATE kv SET v = x'07' WHERE k = 7;
             COMMIT;",
        ),
        _ => unreachable!(),
    }
}

fn rows(c: &Connection) -> rusqlite::Result<Rows> {
    let has_kv: i64 = c.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name = 'kv'",
        [],
        |r| r.get(0),
    )?;
    if has_kv == 0 {
        return Ok((vec![], vec![]));
    }
    let mut kv = c.prepare("SELECT k, v FROM kv ORDER BY k")?;
    let kv = kv
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut s = c.prepare("SELECT k, s FROM secrets ORDER BY k")?;
    let s = s
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((kv, s))
}

/// The state after each prefix of the workload: `states[k]` is after `k`
/// transactions, recorded from a clean run on the same baseline.
fn expected_states(baseline: &MemoryStorage) -> Vec<Rows> {
    let storage = baseline.fork();
    let volume = Volume::open(storage).unwrap();
    let conn = connect(&volume).unwrap();
    let mut states = vec![rows(&conn).unwrap()];
    for i in 0..TXS {
        run_tx(&conn, i).unwrap();
        states.push(rows(&conn).unwrap());
    }
    states
}

/// Opens the crashed storage and checks it holds one of `states[lo..=hi]`.
fn check(storage: MemoryStorage, states: &[Rows], lo: usize, hi: usize, what: &str) {
    let volume = Volume::open(storage).unwrap_or_else(|e| panic!("{what}: reopen failed: {e}"));
    let conn = connect(&volume).unwrap_or_else(|e| panic!("{what}: connect failed: {e}"));
    assert_eq!(integrity(&conn), "ok", "{what}");
    let got = rows(&conn).unwrap_or_else(|e| panic!("{what}: read failed: {e}"));
    let hi = hi.min(states.len() - 1);
    assert!(
        (lo..=hi).any(|k| states[k] == got),
        "{what}: state matches none of transactions {lo}..={hi} \
         (got {} rows, {} secrets)",
        got.0.len(),
        got.1.len()
    );
}

/// Runs the workload with step `fail` failing. Returns how many transactions
/// reported success.
fn run_failing(storage: &MemoryStorage, fail: u64) -> usize {
    let volume = Volume::open(storage.clone()).unwrap();
    let conn = connect(&volume).unwrap();
    storage.fail_at(Some(storage.steps() + fail));
    let mut ok = 0;
    for i in 0..TXS {
        if run_tx(&conn, i).is_err() {
            break;
        }
        ok += 1;
    }
    let _ = conn.close();
    drop(volume);
    ok
}

/// Every crash point in release builds (CI runs this suite with `--release`),
/// every few in debug builds so that a plain `cargo test` stays quick.
/// `MOUL_CRASH_STRIDE` overrides it.
fn stride(release: u64, debug: u64) -> usize {
    if let Some(n) = std::env::var("MOUL_CRASH_STRIDE")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return n;
    }
    (if cfg!(debug_assertions) {
        debug
    } else {
        release
    }) as usize
}

fn crash_modes() -> Vec<CrashMode> {
    let mut modes = vec![CrashMode::DropUnflushed, CrashMode::KeepUnflushed];
    modes.extend((0..3).map(CrashMode::Torn));
    modes
}

#[test]
fn every_crash_point_leaves_a_whole_number_of_transactions() {
    let baseline = MemoryStorage::new();
    let states = expected_states(&baseline);
    assert!(states.windows(2).filter(|w| w[0] != w[1]).count() >= 6);

    // How many steps the workload takes when nothing fails.
    let clean = baseline.fork();
    let ok = run_failing(&clean, u64::MAX / 2);
    assert_eq!(ok, TXS);
    let total = clean.steps();
    assert!(
        total > 50,
        "the workload should take many steps, took {total}"
    );

    let mut runs = 0;
    for fail in (0..total).step_by(stride(1, 5)) {
        for mode in crash_modes() {
            let storage = baseline.fork();
            let ok = run_failing(&storage, fail);
            storage.crash(mode);
            check(
                storage,
                &states,
                ok,
                ok + 1,
                &format!("fail at {fail}, {mode:?}"),
            );
            runs += 1;
        }
    }
    eprintln!("{runs} crash runs over {total} steps");
}

#[test]
fn a_crash_during_recovery_is_recovered_too() {
    let baseline = MemoryStorage::new();
    let states = expected_states(&baseline);

    // Leave work for recovery: crash with unflushed copies kept partly, at
    // points where log records are durable but not yet applied.
    let clean = baseline.fork();
    run_failing(&clean, u64::MAX / 2);
    let total = clean.steps();

    let mut runs = 0;
    for fail in (0..total).step_by(stride(3, 25)) {
        let crashed = baseline.fork();
        let ok = run_failing(&crashed, fail);
        crashed.crash(CrashMode::Torn(fail));

        // Fail recovery at each of its steps, crash again, and reopen.
        for rfail in 0.. {
            let storage = crashed.fork();
            storage.fail_at(Some(rfail));
            let opened = Volume::open(storage.clone());
            let finished = opened.is_ok();
            drop(opened);
            if finished && storage.steps() <= rfail {
                break; // recovery never reached step `rfail`
            }
            storage.crash(CrashMode::Torn(rfail ^ fail));
            check(
                storage,
                &states,
                ok,
                ok + 1,
                &format!("fail at {fail}, recovery fail at {rfail}"),
            );
            runs += 1;
        }
    }
    eprintln!("{runs} recovery crash runs");
}
