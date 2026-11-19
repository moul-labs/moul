# Litestream compatibility suite

Proves that replicas work in both directions, page by page.

| Test | Shows |
| --- | --- |
| `tests/format.rs` | The Go LTX library (v0.5.1) reads every file Moul writes and Moul reads every file it writes, at every page size; the two compactors agree; page checksums agree; neither writes the lock page, including in a snapshot past 1 GiB |
| `tests/replica.rs` | `litestream restore` rebuilds Moul's level-0 files, compactions and snapshots, at any TXID; Litestream's compactor compacts a Moul replica; Moul restores and follows replicas that Litestream 0.5.13 wrote while replicating a live database |
| `tests/sync.rs` | `litestream restore` rebuilds what Moul's sync wrote, with a hand-off and every level, at every TXID still held; a replica that also carries a server's writer-only store under `moul/writer-only/` restores its shared store the same, and Litestream restores the writer-only store from there too |

`main.go` is a small harness over Litestream's own library, so the tests drive
the real restore planner, compactor and file replica client. The Rust tests
build it with the local Go toolchain on first use.

```sh
cargo test -p litestream-compat
```

| Variable | Effect |
| --- | --- |
| `MOUL_REQUIRE_LITESTREAM=1` | Fail, rather than skip, when Go is missing (CI's compatibility job) |
| `MOUL_SKIP_LITESTREAM=1` | Skip the tests that need Go (CI's other jobs) |

Scratch files go to `target/litestream-compat/tmp`. The nightly
`litestream-latest` workflow runs the suite against the newest Litestream and
LTX releases.

`fixtures/litestream` is a small replica Litestream wrote, which `moul-ltx`
restores in its own tests without Go. Regenerate it with:

```sh
cargo test -p litestream-compat --test replica -- --ignored regenerate_fixture
```
