# moul

A backend that runs in a browser tab: one SQLite file, access rules, auth, files and realtime, synced as a Litestream replica.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1, the first public release. The API may change before 1.0.

This crate gathers the engine's crates under one name:

| Module | Crate | Job |
| --- | --- | --- |
| `moul::core` | `moul-core` | The engine: takes a request, returns a response |
| `moul::store` | `moul-store` | The three stores, system tables, migrations |
| `moul::rules` | `moul-rules` | The rule language |
| `moul::sync` | `moul-sync` | Push, pull, follow and hand-off over a replica |
| `moul::ltx` | `moul-ltx` | LTX files, as Litestream writes them |
| `moul::vfs` | `moul-vfs` | The SQLite file layer that captures commits |
| `moul::web` | `moul-web` | The browser host (WebAssembly only) |

Most people use Moul from JavaScript, through
[`@moul-dev/client`](https://www.npmjs.com/package/@moul-dev/client), or from
the `moul` command (`cargo install moul-cli`).

## License

MIT
