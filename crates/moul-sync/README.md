# moul-sync

Litestream-compatible LTX sync for Moul: push, pull, follow, hand-off, compaction and retention.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** early development. The API changes without notice until the first
release.

## What it does

One device writes and pushes; every other device follows; the writer role
moves only by an explicit hand-off. The replica is the one Litestream 0.5
writes, so `litestream restore` rebuilds it, and Moul follows replicas
Litestream wrote.

| Piece | API |
| --- | --- |
| Where a replica lives | [`Replica`]: `get`, `put` with a condition, `list`, `delete` |
| An S3-compatible bucket (R2, S3, MinIO) | [`S3Replica`], signed with SigV4, over `ureq` natively and `fetch` in the browser |
| A folder, laid out as Litestream's file replica | `DirReplica` (native) |
| Memory, with lost requests, lost answers and a full disk to order | [`MemoryReplica`] |
| A device's sync: connect, push, pull, take over | [`Sync`] |
| What sync needs from the local database | [`Local`]; [`StoreLocal`] for a [`moul_store::Store`] |
| Compaction, snapshots and retention | [`maintain`] |
| Rebuild a database, at any TXID still held | [`restore::restore_db`] |

```text
ltx/0/0000000000000001-0000000000000001.ltx   one per TXID
ltx/1/…, ltx/2/…                              compacted ranges
ltx/9/0000000000000001-00000000000000a4.ltx   snapshots
moul/head.json                                newest TXID, for cheap polling
moul/writer.json                              which device writes
```

## The rules

- **A TXID is written once.** The writer uploads each level-0 file with
  `If-None-Match: *`. S3 and R2 check that atomically, so two devices can
  never both write a TXID. A refusal means another device wrote it, unless
  the object holds exactly these bytes: an earlier upload whose answer was
  lost.
- **The writer record moves by compare-and-swap.** `moul/writer.json` is
  created with `If-None-Match: *` and replaced with `If-Match`. Each new
  writer adds one to the generation.
- **A new writer fences.** Right after taking over, it uploads a file that
  rewrites page 1 unchanged as the next TXID. Whatever the previous writer
  believes, its next upload finds the TXID taken; if the previous writer's
  last file landed first, the new writer takes it and fences after it.
- **Head is a hint, guarded too.** The writer replaces `moul/head.json` with
  `If-Match`, so a writer that lost the role cannot move it back. Followers
  poll it every few seconds, then fetch files by name, or from a listing when
  files were compacted.
- **Nothing is discarded silently.** A device whose unpushed changes are not
  in the replica keeps its database through [`Local::save_conflict`] before
  taking the replica's state. So does a device that joins a replica while
  holding data of its own, or that wrote while disconnected.
- **Followers stay byte for byte.** A follower applies the writer's pages
  verbatim ([`moul_vfs::Capture::apply`]), so its `main.db` is the writer's
  file, and is never written by anything else.

## Compaction and retention

The writer compacts when idle; any writer, or a script, can, because every
decision comes from the replica's listing. Ages are measured by the replica's
clock, never the device's, so a phone with a wrong clock neither prunes early
nor compacts on every pass. Before deleting anything it checks that the
newest state still restores without it.

| Setting ([`Retention`]) | Default |
| --- | --- |
| Level 1 | every 5 minutes |
| Level 2 | every hour |
| Snapshot | every 24 hours |
| Compacted level-0 and level-1 files kept | 24 hours; a follower asleep longer starts again from a snapshot |
| History | 7 days, plus the snapshot before the window as its base |

## Tests

| Suite | Shows |
| --- | --- |
| `tests/scenarios.rs` | Devices converge byte for byte: a race to create, hand-off both ways, an offline writer that lost the role, an old writer's last upload before the fence, lost requests and answers, a full replica, a restart between upload and head, a follower asleep past retention, a crash at every storage step while following, a Litestream replica, a skewed clock |
| `tests/s3.rs` | The S3 transport against a real store: conditional writes, eight devices racing for one TXID, wrong credentials, and devices syncing through it |
| `tools/litestream-compat/tests/sync.rs` | `litestream restore` rebuilds a replica sync wrote, with a hand-off and every level, at every TXID still held |

The S3 tests run when `MOUL_S3_ENDPOINT` is set; CI runs them against MinIO:

```sh
MINIO_ROOT_USER=moul-test MINIO_ROOT_PASSWORD=moul-test-secret \
  minio server /tmp/minio --address 127.0.0.1:9000 &
mkdir -p /tmp/minio/moul-test
MOUL_S3_ENDPOINT=http://127.0.0.1:9000 cargo test -p moul-sync --test s3
```

## Not yet

- The writer-only store is not replicated. It holds nothing until the engine
  port (Phase 4); then it gets its own capture under `moul/writer-only/`, and
  a device fetches it when it takes over.
- Futures are not `Send`: sync runs on one thread, in a browser worker or
  under `block_on` in the CLI. The hub (Phase 6) will need `Send` ones.

## License

MIT
