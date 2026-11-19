# Working on an app that uses Moul

This file is for AI coding agents. The app's backend is Moul: the engine runs inside the web page
(`@moul-dev/client`), and the data is one SQLite file in the browser. There is no server to call
and no server code to write.

## The definition is a file

- Collections, fields and rules live in **`moul.schema.json`** in the repository. Change the app's
  data model there, not with ad hoc requests. The client applies it when the app opens
  (`createClient({ db, schema })`); it creates and changes collections and never deletes one.
- After editing it, run **`moul check --json`** and fix every error. Treat warnings about empty
  write rules as real: an empty rule lets anyone, including guests, do that.
- Regenerate types with **`moul typegen moul.schema.json -o src/moul-types.ts`** and pass
  `MoulSchema` to `createClient<MoulSchema>()`. Let the compiler find field mistakes.
- Field names are camelCase (`dueDate`, not `due_date`). Collection names are letters, digits and
  `_`. `id`, `createdAt` and `updatedAt` exist on every record already.

## Reading and writing

- Show data with **live queries** (`collection(name).live(...)`, `liveGet(id)`), not one-off reads
  plus manual refreshes: they update themselves after every relevant commit, from any tab or
  device. Do not add polling.
- Write with `create`, `update` (partial), `delete`. Writes return the stored record.
- Filters and rules use the same language: `owner = @request.auth.id && done = false`. Text goes in
  quotes. Relation paths (`project.owner`) work in filters and list rules.
- Handle `MoulError` (`status`, `message`, `data`): 400 invalid input, 401 sign-in needed, 403 a
  rule refused it, 404 not found or not visible, 409 the device follows another writer
  (read-only).

## Sign-in

- Auth collections (usually `users`) have password and email-code sign-in:
  `moul.auth('users').signInWithPassword(...)`, `requestCode(email)` + `signInWithCode(email, code)`.
- In a browser tab no mail is sent. Codes land in the local inbox; in tests read them with
  `owner.admin.inbox()`.
- Rules decide what a signed-in person may do. Owner-only pages (admin tools) should use
  `moul.asOwner()` or Studio, never by weakening rules.

## Tests

- Test data rules and flows with **`@moul-dev/client/headless`** in Node: no browser, a fresh
  in-memory database per test, a fixed clock (`now`) and seed (`seed`) for repeatable runs.
- Make one owner client for setup and a separate `new MoulClient(owner.transport, { authStorage: null })`
  as the app's view, so rules apply.

## Do not

- Do not write a server, an API layer or SQL: the client is the API.
- Do not store secrets (API keys of other services) in records: anyone with the page can read the
  database file.
- Do not import `@moul-dev/client/headless` in browser code: it is for tests and tools.
- Do not call `database.exec` (raw SQL) for app data: it bypasses rules and the change log.
- Do not put a Moul Hub token in a link, a record or `localStorage`: `moul.sync.configure` keeps it
  in the engine. Join links for a hub carry a one-time ticket (`moul.sync.joinTicket()`).

More: `node_modules/@moul-dev/client/llms.txt`, and the docs at
https://github.com/moul-labs/moul/tree/main/docs
