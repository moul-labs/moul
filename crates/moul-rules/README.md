# moul-rules

Parser, SQL compiler and record evaluator for Moul's access-rule language.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1, the first public release. The API may change before 1.0.

## What it does

Rules look like PocketBase's: `owner = @request.auth.id && status != 'draft'`.
This crate reads them exactly as the Go engine does.

| Piece | API |
| --- | --- |
| Does a rule allow this record for this caller | [`evaluate`] with an [`EvalContext`] |
| A filter or list rule as a SQL `WHERE` | [`filter_sql`] with [`FilterOptions`] |
| A sort string as `ORDER BY` terms | [`sort_sql`] with [`SortOptions`] |
| Columns a rule pins to the caller | [`owner_fields`] |
| Whether a column may be filtered, or is secret | [`is_valid_field`], [`is_secret_field`] |
| Syntax errors, before a rule is saved | [`check`] |
| Fields a record rule reads that its collection lacks (`moul check`) | [`record_rule_names`] |
| The expression a rule stands for | [`translate`] |
| How deeply a rule or filter may nest | [`MAX_DEPTH`] |

```rust
use moul_rules::{EvalContext, Map, evaluate};
use serde_json::json;

let record: Map = serde_json::from_value(json!({"owner": "u1", "status": "live"})).unwrap();
let auth: Map = serde_json::from_value(json!({"id": "u1"})).unwrap();
let ctx = EvalContext { auth: Some(&auth), record: &record, request: None, db: None, now_ms: 0 };
assert_eq!(evaluate("owner = @request.auth.id && status != 'draft'", &ctx), Ok(true));
```

The evaluator needs the database only for `@collection.*`, `get(`, `exists(`
and `strftime(`; [`Lookup`] is what it asks of it: a collection by name, and
the first row of a `SELECT` with numbered placeholders. The SQL compiler
writes named placeholders (`:p1`, `:f_p1`) for rusqlite. The caller passes
the time (`now_ms`, for `@now` and the other date macros) and the cloak blind
index, so the crate has no clock, no I/O and builds for the browser.

## How a rule is evaluated

The Go engine translated a rule into an [expr-lang](https://expr-lang.org)
expression and ran it. So does this crate: the translation is ported as is,
and the expression is read by a port of the part of expr-lang (v1.17) a
translated rule can reach: its lexer, parser, type checker, constant folding
and run-time rules. A type mismatch the checker can see (`status = 1` on a
text field) is a compile error, one it cannot is a run error, and the
messages, with their `(line:column)` and caret, are expr-lang's.

## Parity with the Go engine

Every rule call recorded from the Go test suite (`fixtures/go/rules`) is
replayed by `tests/go_parity.rs`: 258 of 258 `evaluate`, 64 of 64
`filter_sql` and 40 of 40 `sort_sql` cases match, error messages included.
`tests/go_edge_cases.rs` replays 457 more calls answered by the Go engine
itself (type errors, folding, odd spacing, database functions, relation and
cloak SQL, numbers out of range). The Go package's own tests are ported in
`tests/go_unit.rs`.

The edge cases are listed in `tools/rules-edge-cases/cases.py`. After adding
one, or when the Go engine changes, answer them again:

```sh
scripts/export-rules-edge-cases.sh [path to a clean moul-dev checkout]
```

This rewrites the three files in `tests/data` with answers from the engine's
own `internal/rules`, built with a `go test -overlay` that leaves the checkout
untouched. It needs `go` and `python3`. The recording differs from a running
engine in two ways. Cloak's keyed hash becomes `hash(<text>)`. The database
cases run on plain modernc SQLite, built by the file's `setup` and then by the
engine's `db.EnsureSystemTables`.

As in the Go engine, a rule of more than expr-lang's 10,000 nodes is
refused, with its message and position. One limit is new: a filter or rule
nested more than [`MAX_DEPTH`] deep is refused too. The Go engine's stacks
grow, so it had none, but here a stack overflow would abort the process or
kill the browser worker. A long flat chain such as `a = 1 || a = 2 || ...`
does not nest, so it runs up to the node limit. `tests/limits.rs` runs the
deepest and longest input on a 256 KiB stack.

Not reproduced, because a translated rule cannot reach them or they would
need a regex engine: expr-lang's `matches`, `..`, `*`, `/`, `%`, `**`,
arrays, maps, closures, `let`, `if` and pipes. They are refused with an
error.

## License

MIT
