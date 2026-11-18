# Changelog

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
