# Fixtures from the Go engine

These files record what the Go engine ([moul-dev/moul-dev](https://github.com/moul-dev/moul-dev))
does in its own test suite. The Rust port replays them to show it answers the same
way (the parity suite, Phase 4). Do not edit them by hand; re-export instead.

`MANIFEST.json` names the Go commit, the Go version and the counts.

## How they are made

```sh
scripts/export-go-fixtures.sh [path to a clean moul-dev checkout]
```

`tools/go-fixtures` runs `go test -json ./...` in the Go checkout with a build
overlay (`go test -overlay`). The overlay wraps three things in recorders; the Go
checkout itself is never changed:

| Wrapped | Recorded as |
| --- | --- |
| The echo router built by `handlers.NewRouterWithOptions` | `http/` |
| `rules.EvaluateRule` | `rules/evaluate.jsonl` |
| `rules.BuildFilterSQLWithOptions` | `rules/filter_sql.jsonl` |
| `rules.BuildSortSQLWithOptions` | `rules/sort_sql.jsonl` |

If the Go engine renames or reshapes one of these, the exporter stops with an
error that names the file to look at.

## `http/<package>/<Test>.json`

One file per top-level Go test that sent requests through a router.

- `outcome` is the Go test's result. Replay only `pass` fixtures.
- `sessions` has one entry per router the test built. Each starts from a fresh
  engine, and `db_before` names the database state taken just before its first
  request. Setup a test does straight into the database after that request is not
  captured.
- `exchanges` are in the order the router finished them. Each has the matched
  `route` pattern, the `request` and the `response`.
- A body is `{"json": …}`, `{"text": …}` or `{"base64": …}`. Bodies over 256 KiB
  are `{"$omitted": true, "len": …, "sha256": …}`; a response that streamed more
  than that has `body_truncated`.

Only tests that go through a router produce HTTP fixtures. Tests that call a
handler function directly with `e.NewContext` do not.

## `rules/<kind>.jsonl`

One call per line, sorted by `id`, which is a hash of input and output, so
identical calls appear once. `sources` lists the `package.Test` that made the call,
or `package.(server)` when the call came from inside a request.

- `evaluate`: input `rule`, `auth`, `record`, `context` and, for rules that query
  the database (`@collection`, `get(`, `exists(`), the state `db`; output
  `allowed` and `error`.
- `filter_sql`: input `filter`, `moul` (the collection), `auth`, `context`,
  `param_prefix`, `untrusted`, and `loaded`, every relation target the call looked
  up; output `where`, `params` and `error`.
- `sort_sql`: input `sort`, `moul`, `untrusted` and `loaded`; output `order_by`
  and `error`.

## `db/<id>.json`

A database state: `schema` holds every `sqlite_master` entry with SQL, and
`tables` holds each table's `columns` and `rows`. Blobs are `{"$bytes": base64}`.
A table over 1,000 rows is cut and marked `truncated`. States are stored once and
referenced by id.

## Known limits

- Ids, timestamps and tokens differ on every run, so a re-export rewrites most
  files. A replay has to match these by shape, not by value.
- Rule calls made while serving a request are not tied to the HTTP session that
  made them.

`tools/moul-fixtures` loads these files with types, and its tests check that every
file parses and every referenced database state exists.

## Rule edge cases

`crates/moul-rules/tests/data` holds more rule calls, ones the Go test suite never
makes. `scripts/export-rules-edge-cases.sh` records them: `tools/rules-edge-cases`
adds a recorder to the Go checkout's `internal/rules` with the same kind of
overlay and feeds it the inputs in `tools/rules-edge-cases/cases.py`. See
`crates/moul-rules/README.md`.
