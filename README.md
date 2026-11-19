# Moul

A backend that runs entirely in a browser tab: a database, access rules, auth,
files and realtime, stored as one SQLite file and synced as a
Litestream-compatible LTX replica. The same engine later runs on Moul Hub, where
apps are deployed as data.

**Status:** 0.1, the first public release. A browser tab runs the whole
engine: collections, records, rules, sign-in with a local inbox, files,
realtime and live queries, stored on OPFS and synced to an S3-compatible
bucket as a Litestream replica. `@moul-dev/client` is the SDK,
`@moul-dev/studio` the admin console as an element, and the
[Playground](https://play.moul.dev) is Studio on a database in your browser.
`moul check`, `schema`, `typegen`, `pull` and `restore` work on schema files,
database files and replicas.

The same engine runs as a server, `moul serve`: the API over HTTP, live events,
mail, sign-in with GitHub, Google and Apple, webhooks, jobs, feature flags,
replication, and MCP for AI agents (also `moul mcp` over stdio). `moul hub` is
sync without a bucket: devices sign in with an email code, hear of changes as
they land, and the hub compacts their replicas; a hosted beta runs at
sync.moul.dev. Hosting apps on the hub comes next.

Start with the [docs](docs/README.md).

## Packages

| Package | Job |
| --- | --- |
| [`@moul-dev/client`](packages/client) | The SDK: the engine in the page (or a server over HTTP), live queries, sign-in, files, sync; a headless engine for tests |
| [`@moul-dev/studio`](packages/studio) | The admin console as `<moul-studio>` |
| [`sites/playground`](sites/playground) | play.moul.dev |

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
| `moul-hub` | Native host for many databases; first, sync for a builder's devices | Native |
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

The TypeScript packages need Node 22 and the engine built for the browser:

```sh
npm ci
scripts/build-wasm.sh                  # packages/client/wasm
npm run build                          # the SDK, Studio and the Playground
npm run dev -w @moul-dev/studio        # Studio at localhost:5174
npm run dev -w moul-playground         # the Playground at localhost:5175
```

## Testing

```sh
cargo test --workspace
cargo test -p moul-core -p moul-sync --release --test budgets -- --ignored --nocapture   # time budgets
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

The TypeScript tests run on the headless engine, then end to end in the
three browsers:

```sh
npm test                               # SDK, Studio and Playground units
node browser-tests/e2e.mjs             # the SDK in a worker: tabs, live queries, leader hand-off
node browser-tests/playground.mjs      # the built Playground, offline, and sync between two devices
node browser-tests/studio.mjs          # the packaged <moul-studio>, as an app bundles it
node browser-tests/bench.mjs           # time budgets: opening a database, a create (Chromium's CPU / 4)
```

The sync suite's S3 tests run when `MOUL_S3_ENDPOINT` points at an
S3-compatible store; see [`crates/moul-sync`](crates/moul-sync/README.md).

The server and the hub are tested over real HTTP on loopback ports
(`cargo test -p moul-server -p moul-hub`), and the server's parity suite
replays the Go engine's recorded traffic through the whole server:

```sh
cargo test -p moul-server --test go_parity -- --nocapture
```

## Demo

`demo/` is Phase 3's SQL console over the storage and sync layers, kept as a
low-level test page:

```sh
scripts/build-demo.sh
python3 -m http.server 8787 --directory demo
```

See [`demo/README.md`](demo/README.md) for the bucket and its CORS rule.

## Fixtures from the Go engine

The Go engine ([moul-dev/moul-dev](https://github.com/moul-dev/moul-dev)) is the
reference for the port. `fixtures/go` holds HTTP exchanges, rule evaluations and
the database states they ran against, recorded from its test suite. To refresh
them from a clean checkout:

```sh
scripts/export-go-fixtures.sh ../../moul-dev/moul-dev
```

See [`fixtures/go/README.md`](fixtures/go/README.md) for the format.

The parity suite replays them: `crates/moul-rules/tests/go_parity.rs` for
rule evaluation and SQL, `crates/moul-core/tests/go_parity.rs` for the HTTP
API (run it with `--nocapture` for a table by route), and
`crates/moul-store/tests/convert.rs` converts every recorded database.

## Publishing the placeholders

`scripts/reserve-names.sh` publishes every crate at the workspace version and
skips any that is already on crates.io. crates.io allows five new crates at
once and then one every ten minutes, so the script waits when it is rate
limited; a full run takes about an hour.

## License

MIT
