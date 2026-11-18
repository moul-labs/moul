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
Whether or not the import lands, the stores are opened again. If that
fails (a volume a failed commit left unusable), the store stays closed and
answers errors, never panics, until `Store::ensure_open` succeeds; nothing
else, not even `Store::write`, opens it again behind the owner's back.
`Store::reopen_around` behaves the same way. Every new connection starts
bare, so settings the owner needs on each one, such as an authorizer, go
in `Store::on_connect`, which runs on every new connection; if it fails,
the store stays closed.

## The app definition

`schema` holds collections, fields and rules in the Go engine's JSON shape,
plus a stable `id` on every field. `Collection::validate` applies the Go
engine's checks and messages; `Collection::match_field_ids` keeps ids across
an update (by id, then by name). `catalog` saves definitions in `_moul` and
keeps each collection's table in line: renames by id (chains and swaps
included), added and dropped columns, a rebuild when a column's type
changes, and unique indexes. `catalog::save` never writes a webhook secret
to `_moul`: it moves it to `writer._webhook_secrets`, keyed by the
webhook's id. A webhook without an id loses its secret.

## System tables and migrations

`Store::migrate` creates or upgrades the system tables of all three stores in
one transaction; each store records its format in `PRAGMA user_version`, and
`_meta` records the oldest runtime that may open the shared store. A newer
format is refused. A follower calls `Store::migrate_local`, which leaves the
shared store to the writer.

| Store | Tables |
| --- | --- |
| Shared | `_moul`, `_moul_version`, `_meta`, `_changes`, `_files`, `_file_refs`, and one table per collection |
| Writer-only | `_auth`, `_keys`, `_settings`, `_inbox`, `_revoked_tokens`, `_rootUsers`, `_webhook_secrets`, `_worker_args` |
| Device | `_device`, `_queue`, `_blobs` |

## Converting a Go engine database

A Go database keeps everything in one file. On first `migrate` it is
converted in place:

- Fields get ids.
- Credential columns of auth tables move to `writer._auth`, reset tokens
  hashed as this engine looks them up. Indexes that use one of those
  columns are dropped, matched by whole column name, so an index on
  `otpCodeHint` stays. A view, trigger, `CHECK` constraint or generated
  column that uses one stops the conversion with an error that says what
  keeps the column.
- `_settings`, `_rootUsers`, `_revoked_tokens`, `_requests`, `_visits`,
  `_certmagic` and `_workers` (its queued mail holds one-time codes and
  reset links) move to the writer-only store.
- Before `_workers` existed, the Go engine queued its mail in a worker
  collection. The arguments of `SendEmail` jobs in worker collections
  that carry a rendered `body` move to `writer._worker_args`, leaving
  `{}`. Every other job keeps its arguments.
- Webhook secrets move to `writer._webhook_secrets`.
- `_realtime_tickets` is dropped.

The conversion runs with `secure_delete` and ends with `VACUUM`, so no
moved secret stays in the shared file's free pages; if the `VACUUM` fails,
`_meta` says so and the next `migrate` runs it again. The Go engine cannot
open the result. `tests/convert.rs` converts every database state recorded
from the Go engine's tests.

## License

MIT
