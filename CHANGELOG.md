# Changelog

## Unreleased

The engine runs as a server, and devices can sync through a hub instead of a bucket.

- **`moul serve <folder>`**, the native server (`moul-server`): the engine's API over HTTP with
  CORS, rate limits and a request log; live events as server-sent events, with single-use
  tickets; mail through Resend, Mailgun, SendGrid, Amazon SES or Cloudflare; sign-in with GitHub,
  Google and Apple; webhooks, signed and retried; jobs (`SendEmail`, `Webhook`, or claimed by your
  own program); feature flags; root users and settings; replication to a folder, a bucket or a hub
  as it commits; OpenAPI at `/openapi.json`. It opens a Go engine database with `--from`, keeps
  its settings, and its tokens with `MOUL_JWT_SECRET`.
- **MCP** at `/api/mcp`, with an OAuth authorization server so agents can sign in, and `moul mcp
  <folder>` over stdio. The Go engine's tools, plus the schema file, the change log, and a
  read-only query that cannot reach credentials.
- **`moul hub`**, sync for a builder's devices (`moul-hub`): sign-in by email code, a token per
  device, databases, join tickets for QR codes, push events, and compaction on the hub. The hosted
  beta, sync.moul.dev, is `moul hub` behind an invitation.
- **Devices sync through a hub** (`moul-sync`'s `HubReplica`): the SDK's `HubClient` and hub sync
  settings, join links that carry a one-time ticket, a worker that syncs as soon as the hub says
  there is something new, and a "Moul Hub" choice on Studio's Sync page. `moul login <hub>`, then
  `moul pull moul+https://<hub>/<database>`.
- **The engine** serves settings, root users, feature flags, jobs, webhooks and OAuth sign-in in
  every host, and writes mail, webhook calls and jobs as rows a server sends once.
- The parity suites now cover the server's routes; where Moul departs from Go (secrets never read
  back, checked OAuth state, registered MCP clients only, retried webhooks, …) each difference is
  a written decision.

## 0.1.0

The first public release. A backend runs in a browser tab: collections, records, rules, sign-in,
files, realtime and live queries, kept as one SQLite file in the browser and synced, if you want,
to a bucket you own as a Litestream replica.

- **`@moul-dev/client`**, the SDK. The engine starts in a worker in each tab, one of which holds
  the database for all of them, or the client talks to a Moul server over HTTP with the same requests. Records, sign-in with
  passwords and email codes (codes land in a local inbox), files, record subscriptions, and live
  queries that refresh whenever a commit touches what they read, from any tab or device. The
  database file, sync with a bucket, and join links for other devices. A headless engine
  (`@moul-dev/client/headless`) runs the same engine in Node for tests. `llms.txt`, `AGENTS.md`
  and a skill ship in the package.
- **`@moul-dev/studio`**, the admin console as a `<moul-studio>` element: collections, fields and
  rules with a rule tester, records, the local inbox, the database file and change log, sync with a
  QR code for other devices, and the schema file and TypeScript types. It warns when nothing is
  backed up and offers the file on every screen.
- **The Playground** at play.moul.dev: Studio on a database in your browser, started empty, from a
  sample app, a `.db` file or a spreadsheet. It works offline once loaded.
- **`moul check`** validates a schema file or a database's definitions: every definition, every
  rule compiled against its collection, relations, and warnings for rules that let anyone write.
  `--json` for agents and CI.
- **The engine** (`moul-core` on `moul-store`, `moul-rules`, `moul-sync`, `moul-ltx`, `moul-vfs`)
  answers the Go engine's API for the operations it supports, converts a Go engine database on
  first open, logs every write as a named request, and reports what each read depended on
  (`Moul-Reads`).
- **Sync** writes the replica Litestream writes: `litestream restore` and `moul restore` rebuild it.

Not yet: the native server, MCP, OAuth providers, mail, jobs and Moul Hub.
