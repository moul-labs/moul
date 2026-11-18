# moul-store

SQLite access for Moul: the shared, writer-only and device stores, system tables and migrations.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** early development. The API changes without notice until the first
release.

## Three stores

| Store | File | Holds | Goes to |
| --- | --- | --- | --- |
| Shared | `main.db` | Records, the app definition, the change log | The replica and every device |
| Writer-only | `writer.db` | Secrets and the request log | The writer only |
| Device | `device.db` | Unconfirmed requests, drafts, sync position | Nowhere |

`Store::open` opens `main.db` and attaches the other two to the same
connection, so one transaction can write a record and its secret, and the
volume commits both or neither. Each file is marked with its own
`application_id` (`MOUL`, `MOUW`, `MOUD`) and uses 4,096-byte pages.

`Store::export` returns `main.db` as a plain SQLite file. `Store::import`
checks a file in a scratch volume (integrity check, page size, not a
writer-only or device store) and then replaces `main.db` in one commit.

System tables, migrations and converting a Go engine database arrive with
the engine port in Phase 4.

## License

MIT
