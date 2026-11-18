# Moul

A backend that runs entirely in a browser tab: a database, access rules, auth,
files and realtime, stored as one SQLite file and synced as a
Litestream-compatible LTX replica. The same engine later runs on Moul Hub, where
apps are deployed as data.

**Status:** early development. The crates on crates.io are placeholders that
hold the names; none has a usable API yet.

## Crates

| Crate | Job | Builds for |
| --- | --- | --- |
| `moul` | Umbrella crate | Browser and native |
| `moul-ltx` | Read, write and compact LTX v3 files | Browser and native |
| `moul-vfs` | SQLite file layer that records the pages each commit changes | Browser and native |
| `moul-store` | SQLite access, the three stores, system tables, migrations | Browser and native |
| `moul-rules` | Parse the rule language, compile it to SQL, evaluate it on a record | Browser and native |
| `moul-core` | Collections, records, auth, files, realtime and the change log | Browser and native |
| `moul-sync` | Push, pull, follow, hand-off, compaction and retention | Browser and native |
| `moul-web` | Browser host: worker entry point, message protocol, tab coordination | Browser |
| `moul-server` | Native host for one database: HTTP API, realtime, mail, jobs, MCP | Native |
| `moul-hub` | Native host for many databases | Native |
| `moul-cli` | The `moul` binary | Native |

## Building

```sh
cargo build --workspace
cargo build --workspace --target wasm32-unknown-unknown
```

On macOS, the browser build needs an `llvm-ar` that understands WebAssembly
objects; Apple's `ar` silently produces an empty SQLite archive.
`rust-toolchain.toml` installs the `llvm-tools` component and
`scripts/llvm-ar` finds it (or a Homebrew LLVM).

## Testing

```sh
cargo test --workspace
```

The storage crash-safety suite samples crash points in debug builds; run every
one with:

```sh
cargo test -p moul-vfs --release --test crash -- --nocapture
```

Browser tests run the workspace's `wasm-bindgen-test` targets in Chromium,
Firefox and WebKit through Playwright. They need `wasm-bindgen-test-runner` at
the `wasm-bindgen` version in `Cargo.lock`:

```sh
cargo install wasm-bindgen-cli --version 0.2.130 --locked
cd browser-tests && npm ci && npx playwright install chromium firefox webkit
node run.mjs                       # all three engines
node run.mjs --browsers webkit     # one engine
MOUL_BROWSER_TEST_CONSOLE=1 node run.mjs   # also print the browser console
```

## Fixtures from the Go engine

The Go engine ([moul-dev/moul-dev](https://github.com/moul-dev/moul-dev)) is the
reference for the port. `fixtures/go` holds HTTP exchanges, rule evaluations and
the database states they ran against, recorded from its test suite. To refresh
them from a clean checkout:

```sh
scripts/export-go-fixtures.sh ../../moul-dev/moul-dev
```

See [`fixtures/go/README.md`](fixtures/go/README.md) for the format.

## Publishing the placeholders

`scripts/reserve-names.sh` publishes every crate at the workspace version and
skips any that is already on crates.io. crates.io allows five new crates at
once and then one every ten minutes, so the script waits when it is rate
limited; a full run takes about an hour.

## License

MIT
