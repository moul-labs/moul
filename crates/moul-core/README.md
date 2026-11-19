# moul-core

The Moul engine: collections, records, auth, files, realtime and the change log. Takes a request and returns a response.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1, the first public release. The API may change before 1.0.

## How it runs

`Engine::open(store, host, blobs, config)` takes the three stores from
`moul-store`, a `Host` (clock and randomness), a `Blobs` store for uploaded
bytes and a `Config`. `Engine::handle(Request) -> Response` serves one
request. Nothing in the crate does I/O of its own, so the same engine runs in
a browser worker, in the native binary and, later, on the hub.

A request has the shape of an HTTP request because the Go engine's HTTP API
is the specification and because the SDK sends the same requests to a worker
or to a hub. `Request::from_http` parses JSON, multipart and form bodies. The
host marks the database owner's requests with `Caller::Owner` (Studio in the
builder's tab); otherwise a bearer token or the configured admin key decides.

| Host | Use |
| --- | --- |
| `SystemHost` | Native programs: system clock, OS randomness |
| `SeededHost` | Tests: a fixed clock and seeded randomness, so runs replay exactly |

| Blobs | Use |
| --- | --- |
| `StoreBlobs` | Bytes in the device store, written in the upload's transaction (the browser) |
| `DiskBlobs` | A folder of files named by SHA-256 (native) |
| `MemoryBlobs` | Tests and private windows |

## Routes

The Go engine's paths under `/api`, for the operations the first browser
release needs, plus four of Moul's own.

| Path | Methods | Who |
| --- | --- | --- |
| `/api/moul`, `/api/moul/:name` | collections: list, create, get, change, delete | owner |
| `/api/moul/:name/records[/:id]` | records: list (filter, sort, search, expand, pages, `after` cursor), view, create, update, delete | the collection's rules |
| `/api/moul/:name/auth-with-password`, `otp/request`, `auth-with-otp`, `refresh`, `logout`, `request-password-reset`, `confirm-password-reset`, `auth-methods` | sign-in | anyone |
| `/api/moul/:name/email-templates`, `export`, `import` | templates, CSV and JSON | owner |
| `/api/upload`, `/api/files[/*]`, `/storage/*` | uploads and serving | members upload, rules decide reads |
| `/api/rules/test` | try a rule | owner |
| `/api/changes?after=` | the change log | owner |
| `/api/inbox?to=` | mail kept locally: codes and reset links | owner |
| `/api/schema` | the schema file: `GET` exports, `POST` applies (`?prune=true` deletes what it does not list) | owner |
| `/api/types` | TypeScript types for the schema | owner |

## Effects: mail, webhook calls and jobs

A write records what it owes in the writer-only store, in its own
transaction: mail in `_inbox`, a webhook call per matching webhook in
`_outbox`, jobs in `_workers`. A tab keeps them (Studio shows the inbox). A
native server takes them in leased batches (`Engine::due_mail`,
`due_webhooks`, `claim_jobs`), does the network work and reports back
(`mail_done`, `webhook_done`, `job_done`); failures are retried with backoff.
Only the writer holds that store, so a follower never sends anything.

OAuth sign-in is split the same way: the engine makes the provider's address
and a signed `state` whose PKCE verifier it can recompute
(`Engine::verify_oauth_state`); the server trades the code for a profile and
hands it to `Engine::sign_in_external`.

## Requests, the change log and budgets

Every write that changes the shared store is a named request. Its id is the
sending device and a counter: the `Moul-Request-Id` header or `Request::id`,
or the engine's own device and counter when none is sent. The writer applies
each in one transaction and adds a row to `_changes` in the same transaction:
the request id, the actor, the operation, the collection and record ids, the
changed field ids, the other records it changed (cascaded deletes, cleared
references), and the outcome. A refused request is logged with its status
and reason. A request sent again with the same id, by the same actor, for
the same operation and target, is answered from the log
(`Moul-Replayed: true`) and applied once; an id used for anything else
answers 409. Ids made by this device's counter cannot be sent by anyone but
the owner, and the counter skips any id already in the log.

Responses to writes carry `Moul-Request-Id` and `Moul-Change` (the log
position). `Engine::catch_up` reads the log after the last position it
announced and turns it into live events, for a follower whose pages arrived
underneath SQLite. `Engine::import_database` replaces the shared store with a
file (an export, or a Go engine database, converted) and migrates it. A copy
is migrated in memory first, so a file that cannot be migrated (one from a
newer engine) is refused and the data stays.

Each request runs under a `Budget`: wall-clock time and SQLite steps (via the
progress handler), rows read, and response bytes. A request may lower its
budget with `Moul-Budget: time=500, rows=1000, bytes=65536, ksteps=100`. A
spent budget answers 400 with `"budget": "time" | "steps" | "rows" | "bytes"`,
and nothing the request did is kept: a statement the budget stopped fails
the request even where the code around it would read the failure as "no
row", so a starved rule lookup can never let a caller through.

## What a read depends on

A successful `GET` answers with `Moul-Reads`: the ids of the collections it
looked up, comma-separated. That is the collection itself, the targets of
relations it filtered, sorted or expanded through, collections its rules
named, and the signed-in caller's own auth collection (it feeds
`@request.auth`). A rule that calls `exists(` or `get(` may read any table, so
the read depends on everything: `Moul-Reads: *`. A live query refreshes when a
change-log row touches one of its collections, when a definition changes, or
always for `*`.

## Rule SQL stays in the shared store

Filters and list rules compile to SQL, and a rule's `exists(` condition is
SQL its author wrote. It runs on the engine's connection, where the
writer-only store (credentials, signing keys) and the device store (sync
keys) are attached. While such SQL is prepared, an SQLite authorizer lets it
read only the shared store's tables and run only `SELECT`s: no other schema,
no writes, no pragmas, no attaching. A rule that reaches further fails
closed.

## Realtime

`Engine::subscribe(Subscription)` registers a live subscription for a
collection (or `*`), optionally one record, as the subscriber its credentials
name. After each commit, `Engine::take_deliveries` returns the events each
subscriber may see under the subscribe rule, else the view rule, else the
list rule (as in the Go engine), redacted for them and with file URLs signed
as in responses. An update that hides a record from a subscriber arrives as
a delete of the last version they could see. A subscription whose token was
revoked or has expired, or whose user was deleted, ends; the host finds it
in `Engine::take_closed` and closes the stream. On a follower, which learns
of changes from the log, a delete, or an update the subscriber can no
longer see, arrives as a delete carrying only the id.

The host passes the client's network address in `Request::remote`; limits
on guessing email codes count per address (per /64 for IPv6), never per a
header the client chose.

## Where this engine departs from the Go engine

The parity suite (`tests/go_parity.rs`) replays the HTTP traffic recorded from
the Go engine's tests. These differences are decisions:

- **Field ids.** Fields carry an `id`; renaming a field renames its column and
  keeps the data (Go dropped the column and added an empty one). Renaming a
  collection updates relation fields that point at it.
- **Secrets live in the writer-only store.** Password hashes, codes and reset
  tokens are in `writer._auth`, never in the shared file. A Go database is
  converted on first open, and the space the moved credentials used is wiped.
  Reset tokens are stored hashed. Exports never include credentials.
- **Shapes.** An empty field list is `[]`, not `null`. Creating a collection
  whose name is taken answers 400, not 500. A 429 from email-code sign-in
  carries `Retry-After` as a header.
- **Writes on a follower** answer 409.
- **Order and cascades.** A list sorted by anything but `id` ends with `id`
  as a tie-break, so pages and `after` cursors never skip a row (Go ordered
  ties by row position). A delete that would cascade into a record a
  `RESTRICT` relation protects is refused as a whole; Go deleted the parent
  and logged the failed cascade.
- **Tokens.** A token has exactly one accepted spelling, so a revoked token
  cannot come back re-encoded. Logging out with a token this engine did not
  sign answers as Go does but stores nothing.
- **Errors.** A 500 never carries SQLite's text, which can quote rules.
- **Rules are compiled when saved.** Creating or changing collections
  (`POST /api/moul`, `PATCH /api/moul/{name}`, `POST /api/schema`) is
  refused with a 400 naming the rule (`"rule": "listRule"`) when a rule of a
  collection the request saves does not compile against the collections as
  they are once it is made: the list rule as a list compiles it, and its
  `@collection` names checked; the others as `moul check` checks them. Go
  stored any rule text, and every list then failed with a 500. No recorded
  Go test saves such a rule, so the parity suite skips nothing for it. A
  database the Go engine made opens with its rules as they are; a broken one
  is refused when its collection is next saved. A change to one collection
  that breaks another's rule (a rename that `@collection` names) is not
  caught: `moul check` reports it.
- **Password hashes** with a bcrypt cost above 16 are refused rather than
  checked (Go accepts up to 31): one check at cost 20 would hold the
  engine's only thread for about a minute.
- **Cleared references are announced.** When a delete clears a `SET_NULL`
  reference, the record that held it gets a live update event; Go sent none.
- **Request ids** may be sent only by signed-in callers and the owner: every
  guest is the same actor, so a guest could claim another guest's id.
- **Nesting.** A rule, filter or sort nested more than 32 levels deep is
  refused with a 400 (`moul-rules`' `MAX_DEPTH`), and a rule over
  expr-lang's 10,000-node limit fails as it did in Go: deep input must not
  exhaust the stack of a browser worker or a server thread.
- **Settings and webhook secrets** are written, never read back: they show
  as `********`, and sending that back keeps the secret. Invalid
  `rate_limiting_rules` are refused (Go kept the old rules and answered 200).
- **Webhooks** fire after a change only (`:before` is refused when saved),
  are delivered from rows with retries, start enabled, and keep `enabled`
  when a change does not mention it.
- **Feature flags:** a second flag with a key in use is refused (Go updated
  the first and answered with an id it never stored).
- **Jobs** read back with JSON and numbers decoded; `state` and
  `scheduled_at` are checked; discarded jobs are kept a week.
- **OAuth sign-in** checks a signed `state`, and links an existing account by
  email only when the provider says the email is verified and the account has
  no password; an unverified address is not kept on a new account.
- **Not served yet:** analytics events, encrypted (`cloak`) fields, image
  thumbnails, passkeys, and worker collections. The server adds live events,
  MCP, the OAuth authorization server and the request log. Go's in-process
  hooks for custom code have no equivalent: a deployed app carries no code.

## License

MIT
