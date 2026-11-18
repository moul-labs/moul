# Litestream compatibility suite

Proves that replicas work in both directions, page by page.

| Test | Shows |
| --- | --- |
| `tests/format.rs` | The Go LTX library (v0.5.1) reads every file Moul writes and Moul reads every file it writes, at every page size; the two compactors agree; page checksums agree; neither writes the lock page, including in a snapshot past 1 GiB |
| `tests/replica.rs` | `litestream restore` rebuilds Moul's level-0 files, compactions and snapshots, at any TXID; Litestream's compactor compacts a Moul replica; Moul restores and follows replicas that Litestream 0.5.13 wrote while replicating a live database |

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
