---
name: moul
description: Build and change web apps whose backend is Moul (@moul-dev/client): a database, rules, sign-in, files and live queries running inside the browser page, defined by moul.schema.json. Use when the project depends on @moul-dev/client or @moul-dev/studio, has a moul.schema.json, or the user asks for an app with data that works without a server.
---

# Moul

Moul runs the app's backend inside the web page. The data model is `moul.schema.json`; the code
talks to the engine through `@moul-dev/client`. There is no server.

## Workflow

1. **Change the model in `moul.schema.json`**: collections, fields (camelCase names), rules.
   Read `node_modules/@moul-dev/client/llms.txt` for the format and the rule language.
2. **Validate**: `moul check --json`. Fix all errors; take warnings about empty write rules
   seriously (empty = anyone, guests included).
3. **Types**: `moul typegen moul.schema.json -o src/moul-types.ts`, then
   `createClient<MoulSchema>({ db, schema })`.
4. **UI reads with live queries** (`collection(x).live(...)`, `liveGet(id)`): they refresh on
   every relevant commit. No polling, no manual cache invalidation.
5. **Test with the headless engine** (`@moul-dev/client/headless`) in Node: owner client for setup,
   a separate non-owner `MoulClient` on the same transport to exercise the rules. Sign-in codes are
   in `owner.admin.inbox()`.

## Rules cheat sheet

| Want | Rule |
| --- | --- |
| Anyone | `""` (empty); on an auth collection's list, view, update and delete rules, `true` (empty there means own account only) |
| Signed in | `@request.auth.id != ''` |
| Own records | `owner = @request.auth.id` |
| Create only as yourself | `@request.auth.id != '' && @request.body.owner = @request.auth.id` |
| Field set only by the owner | `@request.body.role:isset = false` (create) / `@request.body.role:changed = false` (update) |
| Public once published | `status = 'published' \|\| owner = @request.auth.id` |
| Through membership | `@collection.members.user = @request.auth.id && @collection.members.team = team` |

## Facts to keep straight

- On auth collections, an empty list, view, update or delete rule is stored as
  `id = @request.auth.id`. Use `true` for a public user directory.
- Rules have no `!`: use `!=`, `!~`, `?!=`, compare a bool field or modifier with `false`
  (`done = false`), and rewrite a compound condition (`!(a = 1 && b = 2)` → `a != 1 || b != 2`).
- List rules cannot use `@request.body` (a list has no body).
- Unset fields come back `null`. Relation fields hold ids; `expand` adds related records.
- A tab cannot send mail: email codes and reset links go to the local inbox (Studio → Inbox).
- In a tab the person at the keyboard owns the data; rules protect it once the app is served by
  Moul Hub. Never put other services' secrets in records.
- `moul.database.export()` is the whole data model and records as one SQLite file.
- The admin console is `<moul-studio>` from `@moul-dev/studio` (set `.client = moul`).
