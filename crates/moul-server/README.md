# moul-server

Native host for one Moul database: HTTP API, realtime, mail, jobs and MCP.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1. The API may change before 1.0. Most people run it as
`moul serve`; see [the server guide](https://github.com/moul-labs/moul/blob/main/docs/server.md).

## How it is built

The engine (`moul-core`) is synchronous and holds one SQLite connection, so
it lives on a thread of its own ([`db`]): every request, subscription and
batch of mail is a task queued to that thread and served in order, which is
the order that settles conflicts. Between tasks the thread runs its chores
([`chores`]): pushing the replica, ending live streams whose token expired,
housekeeping. Moul Hub runs one such thread for each open app.

Around it, an axum server ([`http`]) works out the client's address, answers
CORS, refuses credentials in query strings, applies rate limits ([`limits`])
and writes the request log ([`logs`]), then hands the request to the server's
own routes or the engine.

| Module | Job |
| --- | --- |
| [`server`] | Start and stop a server; open the database with its chores |
| [`open`] | Open a database folder, with its admin key; fill a new one from a file |
| [`realtime`] | Server-sent events and single-use tickets |
| [`effects`] | Send the mail, make the webhook calls and run the jobs writes leave as rows |
| [`mail`], [`webhooks`], [`net`] | Mail providers; signed webhook calls; an HTTPS client that refuses private addresses |
| [`oauth`] | Sign-in with GitHub, Google and Apple: the code exchange and the profile |
| [`auth_server`] | An OAuth authorization server for MCP clients, and the device flow |
| [`mcp`] | MCP tools, over Streamable HTTP and stdio |
| [`replica`] | Name a replica: a folder, a bucket or a hub database |
| [`docs`] | `/`, `/api/health`, `/openapi.json`, `/docs`, `/llms.txt` |

```rust,no_run
# async fn run() -> Result<(), String> {
let running = moul_server::start(moul_server::ServerConfig {
    dir: "notes".into(),
    ..Default::default()
})
.await?;
println!("listening on {}", running.addr);
running.stop().await;
# Ok(())
# }
```

## Tests

| Suite | Shows |
| --- | --- |
| `tests/server.rs` | A real server over loopback HTTP: live events and tickets, CORS and challenges, the request log, rate limits, signed webhooks and private addresses refused, the OAuth flow into MCP, the device flow, replication that restores both stores (a server on the restored files signs its root user in), a read-only server that pushes neither, and a folder filled from a file |
| `tests/go_parity.rs` | The Go engine's recorded traffic replayed through the whole server; decisions listed in `DECIDED` |

## License

MIT
