# The server: `moul serve`

The same engine that runs in a browser tab runs as a server: one database
folder, served over HTTP to everyone the rules let in. Use it to share an
app, to run the jobs and send the mail a tab cannot, to give an AI agent the
database through MCP, or in place of a Go engine server.

```sh
moul serve ./notes --listen 0.0.0.0:8090 --public-url https://notes.example.com
```

On first start it makes the folder and an admin key, which it prints once.
The SDK talks to it with the same requests it sends to a tab:

```ts
const moul = createClient({ url: 'https://notes.example.com' });
```

## The folder

```text
notes/main.db     the shared store: a SQLite file other tools can read
notes/writer.db   secrets and logs: password hashes, keys, settings, the request log
notes/device.db   this server's own state
notes/files/      uploads, named by SHA-256
notes/ltx/        commits not yet pushed to the replica (ltx/writer-only/ for writer.db)
```

One process uses a folder at a time. `--from <file>` fills a new folder from
a database file: a Moul export, or a Go engine database, which is converted.

## Settings

| Flag | Environment | Default | |
| --- | --- | --- | --- |
| `--listen` | `MOUL_PORT` | `127.0.0.1:8090` | A port, or address and port. `MOUL_PORT` listens on every address, as the Go engine did. |
| `--public-url` | `MOUL_PUBLIC_URL` | `http://<listen>` | The address people use: links in mail, OAuth redirects |
| `--admin-key` | `MOUL_ADMIN_KEY` | made on first start | Sent as `X-Admin-Key` (or a bearer token) for the owner's requests |
| | `MOUL_JWT_SECRET` | made on first start | The key that signs tokens. Give a Go engine's, and its tokens stay valid. |
| `--cors` | `MOUL_CORS_ORIGINS` | any origin | Origins allowed to call from a browser, comma-separated |
| | `MOUL_TRUSTED_PROXIES` | `loopback` | Proxies whose `X-Forwarded-For` is believed: addresses, ranges, `private`, `none` |
| | `MOUL_CLIENT_IP_HEADER` | | A header a trusted proxy sets to the client's address |
| | `MOUL_TOKEN_TTL`, `MOUL_TOKEN_REFRESH_GRACE`, `MOUL_FILE_URL_TTL` | `24h`, `7d`, `1h` | Lifetimes |
| `--replica` | | | Push every commit to a replica: a folder, `s3://…` or `moul+https://hub/<database>` |
| `--read-only` | | | Serve a folder `moul pull` keeps, refusing writes |

Everything else is in the settings (`GET` and `PATCH /api/settings`, owner
only), under the Go engine's names, so a converted Go database keeps them:
mail (`email_*`), OAuth providers (`oauth_*`), rate limits
(`rate_limiting_*`), the request log (`request_log_*`), root user addresses
(`root_user_*`) and replication (`litestream_*`). Secret settings read back as
`********`; sending the mask back leaves them as they were.

## What it serves

Everything a tab serves (collections, records with rules, sign-in, files,
the change log, the schema file; see the [SDK](sdk.md)), and:

| | |
| --- | --- |
| Live events | `GET /api/moul/:name/subscribe` and `/api/moul/subscribe`, server-sent events in the Go engine's format. `POST /api/realtime/ticket` gives a single-use ticket for clients that cannot send headers. |
| Root users | `/api/setup`, `/api/admin/login`, `/api/admin/account`, `/api/admin/password`: the administrators who sign in to Studio and approve MCP clients |
| Mail | Sign-in codes and reset links go out through Resend, Mailgun, SendGrid, Amazon SES or Cloudflare (`email_provider`). With mail off they stay in the inbox (`GET /api/inbox`) and the server prints them. |
| OAuth sign-in | GitHub, Google and Apple (`oauth_<provider>_*` settings). `auth-methods` lists them; the provider sends people back to `/api/moul/:name/oauth2/:provider/callback`. |
| Webhooks | `/api/moul/:name/webhooks`: a call for each create, update or delete, signed with `X-Moul-Signature` (HMAC-SHA256 of the body), retried with backoff until delivered |
| Jobs | `/api/workers`: `SendEmail` and `Webhook` run here; a job for any other worker waits for your own program to claim it |
| Feature flags | `/api/feature-flags`, with actor, group and percentage gates, evaluated at `POST /api/feature-flags/:key/eval` |
| MCP | `/api/mcp` for AI agents; see below |
| Logs | `/api/requests` (the request log), `/api/visits`, `/api/system/metrics` |
| Docs | `/openapi.json`, `/docs`, `/llms.txt` |

Mail, webhook calls and jobs are rows the write leaves in the writer-only
store, in the same transaction, so each is owed exactly when its change is
kept and is sent only by the writer. A call that fails is tried again later;
one that cannot be delivered is kept, marked failed, for a week.

Webhooks and the `Webhook` job never call private, local or link-local
addresses, such as the server's own network or a cloud metadata service,
do not follow redirects, and ignore proxy settings in the environment.
`--allow-private-calls` lifts that for tests and closed networks.

Every path that checks a password or hands out codes and tokens (sign-in,
the consent page, the device page, token and registration endpoints) counts
against the `*:auth` rate limit, ten every three seconds by default, per
address, or per /64 for IPv6. Changing a root user's password ends the
refresh tokens of the MCP clients they approved.

## MCP for AI agents

On a server, point the agent at `https://<server>/api/mcp`. It signs in
through the server's OAuth endpoints: the agent registers itself, and a root
user approves it on the consent page. Or send the admin key as `X-Admin-Key`.

For a database on your own machine, run the tools over stdio:

```json
{ "mcpServers": { "notes": { "command": "moul", "args": ["mcp", "/path/to/notes"] } } }
```

A folder `moul pull` keeps gives the agent a read-only copy of the real data:
`moul mcp ./notes --read-only`.

The tools keep the Go engine's names (`moul_list_records`,
`moul_create_record`, `moul_set_feature_flag`, …) and add `moul_get_schema`,
`moul_apply_schema`, `moul_list_changes` and `moul_query`, one read-only
`SELECT` over the app's data that cannot reach credentials or keys. Changes go
through the engine like Studio's, checked and recorded in the change log.

## Replication and backups

`--replica` pushes every commit as it lands, as Litestream does, and compacts
the replica. It pushes both stores a server writes:

```text
<replica>/ltx/…, moul/head.json, moul/writer.json    main.db, as Litestream lays it out
<replica>/moul/writer-only/ltx/…, moul/…             writer.db, in the same layout
```

The shared store is byte for byte the replica Litestream writes, so
`litestream restore` rebuilds it and ignores the rest. The writer-only store
(root users, password hashes, signing keys, settings, queued mail, webhook
calls and jobs, the request log) is a replica of its own beside it: Litestream
pointed at `<replica>/moul/writer-only` restores it too. Like the shared
store it is not encrypted, so whoever can read the replica can read its
password hashes and keys: anyone with the bucket's credentials, or on a hub,
any device that reaches the database.

`moul restore` rebuilds either store, or both into a folder that `moul serve`
opens as the same server, root users and all:

```sh
moul restore s3://backups/notes -o notes.db                 # the shared store
moul restore s3://backups/notes -o writer.db --writer-only  # the writer-only store
moul restore s3://backups/notes -o ./notes-restored/        # both, as main.db and writer.db
moul serve ./notes-restored
```

Each store has its own TXIDs, so `--txid` picks a moment in one store; a
folder gets the newest state of both. The two are pushed one after the other,
so after a crash the newest writer-only store can be a second or so behind or
ahead of the shared one. Serve a restored folder with a new replica, or with
none: with its old one it follows that replica's writer, as below.

Only the server that writes pushes the writer-only store. Its sync only ever
pushes: it never takes another device's writer-only store in place of its
own, and stops with an error in the log if the replica's is someone else's.
A server whose replica another device takes over becomes a follower and
refuses writes until it takes the role back; meanwhile its own `writer.db`
stays local and is not pushed.

## Where it departs from the Go engine

Each of these is listed with its exchange in the parity suites
(`crates/moul-core/tests/go_parity.rs`, `crates/moul-server/tests/go_parity.rs`).

- **Secrets are written, never read back**: settings and webhook secrets.
- **OAuth**: the sign-in `state` is signed and checked; an account is linked
  by email only when the provider vouches for the address and the account
  has no password (its owner resets the password by mail instead), and an
  address the provider does not vouch for is not kept; an MCP client must
  register, and codes go only to a redirect address it registered; PKCE is
  S256; refresh tokens are used once; the admin key is not a password on the
  consent page; a device code's token is handed out once.
- **Webhooks** fire after a change, never before it, and are retried; a new
  webhook is enabled unless it says otherwise.
- **Jobs** read back with their JSON decoded; a job whose runner stopped goes
  back to `available`.
- **A second feature flag with a key in use** is refused.
- **Not yet**: TLS certificates (run behind a proxy that ends TLS), counting
  visits, encrypted fields, passkeys, image thumbnails, the self-updater, and
  Go's admin console (use Studio).
