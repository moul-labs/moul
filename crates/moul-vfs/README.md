# moul-vfs

SQLite file layer for Moul that records the pages each commit changes, with back ends for browser storage (OPFS), disk and memory.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1, the first public release. The API may change before 1.0.

## What it does

A [`Volume`] is a folder of SQLite database files plus `commit.log`, served to
SQLite as a VFS. SQLite's pages never go straight to a database file:

1. While SQLite writes a transaction, its pages stay in memory.
2. When SQLite releases its write locks, the volume appends every changed page
   of every file to `commit.log` and flushes. **That flush is the commit.**
3. The commit goes to an observer, then the pages are copied into the
   database files and the log is cut back.
4. On open, any commit in the log that was not copied in yet is copied again,
   and handed to the observer again.

So a transaction that spans `main.db` and an attached `writer.db` commits
atomically, and the database files only ever hold whole commits. SQLite runs
with its journal in memory, because the log already does the journal's job.

| Back end | Where | Use |
| --- | --- | --- |
| `OpfsStorage` | Origin private file system, through sync access handles | The browser, in a dedicated worker |
| `DiskStorage` | A folder on disk | The CLI, the hub, tests |
| `MemoryStorage` | Memory, with a crash simulator | Tests, private windows |

Rules for the connection: one connection per volume (attach the other stores
to it), `journal_mode=MEMORY`, `locking_mode=NORMAL`, and a fixed page size.
WAL is refused.

## Commit capture

[`Capture`] is the observer that builds the replica. Every commit that changes
`main.db` becomes one level-0 LTX file for the next TXID, appended to
`ltx.pack` in its own storage (the `ltx/` folder), and `capture.state` records
the last TXID and the commit it came from:

```rust,ignore
let capture = Capture::open(ltx_storage, "main.db")?;
let volume = Volume::open_observed(storage, capture.clone())?;
capture.snapshot_existing(&volume)?; // a database from before capture
```

- The files sit in one pack rather than a file each because the browser back
  end can only serve files it opened in advance. Sync reads them with
  `Capture::read` and drops them with `Capture::release` once the replica
  holds them.
- The file is flushed before the state, so the state never counts a file that
  is not durable. A crash between the two leaves the commit in the log, and
  reopening hands it over again; the stale record is overwritten.
- If capture fails, the volume takes no more commits until it is reopened,
  and the commit is handed over again then. The replica is never silently
  behind.
- TXID 1 holds every page, as LTX requires. So does the file after any commit
  capture did not see, which the page-1 change counter shows.
- Commits to the other stores write nothing here. The writer-only store
  (`writer.db`) gets a capture of its own, in a storage of its own, and
  [`Fanout`] hands each commit to both:

```rust,ignore
let shared = Capture::open(ltx_storage, "main.db")?;
let writer_only = Capture::open(writer_only_ltx_storage, "writer.db")?;
let volume = Volume::open_observed(
    storage,
    Fanout::new().with(shared.clone()).with(writer_only.clone()),
)?;
```

  A commit that spans both stores becomes one file in each. If either
  capture fails, the volume stops as it does for one, and the next open
  hands the commit to both again; the one that had it already ignores it.

A device that follows another writer uses three more calls:

| Call | Does |
| --- | --- |
| `Capture::apply` | Writes a replica file's pages into the volume as one commit, exactly as given (`Volume::apply_pages`), and records the writer's TXID instead of writing a file. The expected TXID is recorded durably first, so a crash cannot turn the writer's commit into a local file with the wrong number. |
| `Capture::fence` | Writes the next TXID as a file that rewrites page 1 unchanged. A device that takes over writing uploads it at once. |
| `Capture::rebase` | Starts the numbering again at TXID 1 with the whole file, for a new replica. |

A file written in WAL mode (as Litestream's are) says so in its header. The
volume shows SQLite such a file as a rollback-journal file without changing
the stored bytes, so a follower can read a Litestream replica and still match
it byte for byte.

## Crash safety

`tests/crash.rs` runs a workload (cache spills, a rollback, `VACUUM`, a
transaction across two files) on `MemoryStorage`, fails it at every storage
step, then crashes the disk: unflushed writes are dropped, kept, or kept in
part with the last one torn. After reopening, the database must pass
`integrity_check` and hold exactly the transactions that reported success,
plus at most the one in flight. The same is done to recovery itself. Both run
again with both stores captured, and then each store's level-0 files must
also restore to exactly the file that was recovered.

```sh
cargo test -p moul-vfs --release --test crash -- --nocapture
```

## Stepping back

Commits carry local numbers (`CommitSeq`), not replica TXIDs, and a commit sits
in its own layer until it is copied into the file. A device that follows a hub
(Phase 9) adds an unconfirmed layer that is only copied in once the hub
confirms, so stepping back is dropping a layer.

## License

MIT
