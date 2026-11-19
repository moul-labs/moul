# moul-cli

The moul command-line tool: pull, restore, check, serve, hub and deploy.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1. `serve`, `mcp`, `hub`, `login`, `restore`, `pull`, `info`,
`schema`, `typegen` and `check` work; `deploy` arrives with app hosting on
Moul Hub.

```sh
cargo install moul-cli    # installs the `moul` command
```

## Commands

```sh
moul serve [<folder>] [--listen <addr>] [--public-url <url>] [--replica <replica>] [--from <file>]
moul mcp <folder> [--read-only]
moul hub [<folder>] [--listen <addr>] [--store <store>] [--signup open|invite|closed]
moul login <hub>
moul logout <hub>
moul restore <replica> -o notes.db [--txid <n>] [--writer-only] [--force]
moul restore <replica> -o ./notes/ [--force]
moul pull <replica> ./notes [--watch] [--every <seconds>]
moul info <replica>
moul schema <database> [-o moul.schema.json]
moul typegen <schema file | database> [-o moul.d.ts]
moul check [<schema file | database>] [--json] [--strict]
```

- **`serve`** serves a database folder over HTTP: the API, live events,
  mail, OAuth sign-in, webhooks, jobs and MCP. See
  [the server guide](../../docs/server.md).
- **`mcp`** serves a database folder to an AI agent as MCP tools over stdio.
- **`hub`** runs a sync hub for builders' devices; see
  [Moul Hub](../../docs/hub.md).
- **`login`** signs this computer in to a hub with an email code and keeps
  the token in `~/.config/moul/hubs.json`, readable only by you; `logout`
  revokes it. `pull`, `restore`, `info` and `serve --replica` then reach
  `moul+https://<hub>/<database>`.
- **`restore`** rebuilds the database a replica holds into a plain SQLite
  file and checks its integrity, as `litestream restore` does. `--txid`
  restores an earlier state still in the replica's history. `--writer-only`
  restores the writer-only store a server pushes beside it (root users,
  password hashes, keys, settings, the outbox) instead. Given a folder (one
  that exists, or a path ending in `/`), it restores both, as `main.db` and
  `writer.db`, for `moul serve` to open; it refuses a folder that holds
  anything else, such as a server's commit log.
- **`pull`** keeps a local copy in a folder, following the replica's writer.
  `./notes/main.db` is byte for byte the writer's file and any SQLite tool can
  read it while `moul` is not running. `--watch` keeps following, every 3
  seconds or every `--every` seconds (0.2 to 86400). A folder that
  held other data keeps it in `./notes/conflicts/` before taking the
  replica's.
- **`info`** shows who writes, how far the replica reaches, its files by
  level, and which TXIDs it can restore.
- **`schema`** writes the app definition (collections, fields with their
  ids, rules) as a schema file, for git, review and agents.
- **`typegen`** writes TypeScript types for the records of every collection.
- **`check`** validates an app definition so an agent or CI can reject a
  broken one before it is applied. See below.

A database is a SQLite file (`main.db`, a Moul export or a Go engine
database) or a folder holding `main.db`, such as one `moul pull` keeps.
Databases are only read, never changed.

## `moul check`

`moul check` reads a schema file, `./moul.schema.json` when no path is
given, or a database. A schema file is checked in stages, each only once the
ones before it found no error:

1. The file is JSON, its `format` is one this engine reads, and
   `collections` is an array.
2. Each collection is a valid definition, no two share a name or id, and
   every relation field targets a collection in the file.
3. The whole file applies to an empty in-memory engine, exactly as
   `POST /api/schema` would apply it.
4. On the collections that engine then holds, every rule compiles against
   its collection. A list rule is compiled to SQL as a list does; the
   other rules (view, create, update, delete, subscribe) are read against
   one record, so each field they name must exist and they cannot follow
   a relation (`team.owner`). `@request.auth.*` and the rest of the request
   are not fields and are not flagged. Neither is `@request.body.*` in the
   rules read against one record, except `@request.body.<field>:changed`,
   which names a field of the record and is flagged when the collection has
   no such field; in a list rule any `@request.body.*` is an error, because
   a list has no body.

Each stage runs only when the ones before it found no error, so fixing a
relation error can bring rule errors to light. A database is checked as it
stands: each collection as its next change would check it, then its
relation fields, then stage 4. Collections are numbered in the order `moul
schema` writes them, by name.

Warnings, which do not fail the check unless `--strict` is given, flag
rules that let anyone, guests included, write: an empty create, update or
delete rule on any collection. On an auth collection, an empty list, view,
update or delete rule is stored as `id = @request.auth.id` when the file is
applied, so those are never open; a database made by the Go engine may
still hold empty ones, and an empty list or view rule there is flagged
too. To keep a rule open on purpose and still pass `--strict`, write the
rule `true`.

```text
$ moul check
error: collections[1] (tasks).fields[2] (team): relation target "teams" is not a collection
1 error in moul.schema.json
```

With the relation fixed, the next run reaches the rules:

```text
$ moul check
error: collections[1] (tasks).rules.listRule: invalid field "ownr"
warning: collections[1] (tasks).rules.deleteRule: anyone can delete records of tasks, guests included: set deleteRule
1 error, 1 warning in moul.schema.json
```

It exits 0 when there is no error (and, with `--strict`, no warning), 1 when
there is, and 2 when nothing could be checked: a bad argument, a file that
cannot be read, or a SQLite file that is not a Moul database. Exit 2 prints
`moul: <reason>` on stderr and nothing on stdout, even with `--json`.

With `--json`, stdout holds one JSON object whose shape stays stable:

```json
{
  "ok": false,
  "file": "moul.schema.json",
  "collections": 2,
  "errors": [
    { "location": "collections[1] (tasks).rules.listRule", "message": "invalid field \"ownr\"" }
  ],
  "warnings": []
}
```

- `ok` is true exactly when `moul check` exits 0.
- `file` is the path as given.
- `collections` is how many collections the file lists.
- `errors` and `warnings` are lists of `{location, message}`. A location is
  `collections[<i>] (<name>)`, then `.fields[<j>] (<name>)` or
  `.rules.<listRule|viewRule|createRule|updateRule|deleteRule|subscribeRule>`;
  or `format`, `collections`, `file`, or `line <l>, column <c>` for a file
  that is not JSON.

A replica is a folder (a path or `file:///path`, laid out as Litestream's file
replica) or an S3-compatible bucket:

```text
s3://<bucket>/<prefix>?endpoint=https://<account>.r2.cloudflarestorage.com
s3://<bucket>/<prefix>?region=eu-west-1
s3://<bucket>/<prefix>?endpoint=http://localhost:9000
```

Credentials come from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
`AWS_SESSION_TOKEN`; the endpoint and region may come from `AWS_ENDPOINT_URL`
and `AWS_REGION`.

## License

MIT
