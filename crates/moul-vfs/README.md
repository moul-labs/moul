# moul-vfs

SQLite file layer for Moul that records the pages each commit changes, with back ends for browser storage (OPFS), disk and memory.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** early development. The API changes without notice until the first
release.

## What it does

A [`Volume`] is a folder of SQLite database files plus `commit.log`, served to
SQLite as a VFS. SQLite's pages never go straight to a database file:

1. While SQLite writes a transaction, its pages stay in memory.
2. When SQLite releases its write locks, the volume appends every changed page
   of every file to `commit.log` and flushes. **That flush is the commit.**
3. The commit goes to an observer (Phase 2 turns it into an LTX file), then the
   pages are copied into the database files and the log is cut back.
4. On open, any commit in the log that was not copied in yet is copied again.

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

## Crash safety

`tests/crash.rs` runs a workload (cache spills, a rollback, `VACUUM`, a
transaction across two files) on `MemoryStorage`, fails it at every storage
step, then crashes the disk: unflushed writes are dropped, kept, or kept in
part with the last one torn. After reopening, the database must pass
`integrity_check` and hold exactly the transactions that reported success,
plus at most the one in flight. The same is done to recovery itself.

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
