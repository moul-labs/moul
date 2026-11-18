# moul-ltx

Read, write and compact LTX v3 files, the replica format Litestream writes.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** early development. The API changes without notice until the first
release.

## What it does

| Piece | API |
| --- | --- |
| Write a file page by page, streaming | [`Encoder`], [`encode`] |
| Read and verify a file | [`Ltx::decode`], then [`Ltx::iter_pages`] or [`Ltx::read_page`] |
| Merge consecutive files into one | [`compact`] |
| Choose the files to restore from | [`restore::plan`] |
| Rebuild a database, or follow a replica | [`restore::restore`], [`restore::apply`] |

The crate does no I/O and has no dependencies, so it builds for the browser
as well as natively. LZ4 and CRC-64 are written here.

## Format

Files match `github.com/superfly/ltx` v0.5.1, which Litestream 0.5 uses:
a 100-byte header, one LZ4 frame per page in page order, a page index and a
16-byte trailer, with a CRC-64/ISO file checksum over the uncompressed
content. Files written here set the no-checksum flag, as Litestream does.

A file whose first TXID is 1 is a snapshot and holds every page but SQLite's
lock page. Compaction keeps the newest copy of each page. Unlike the Go
compactor, a file in the middle that shrinks the database drops the older
pages past its end, and pages that come back without being written are zeros.

## License

MIT
