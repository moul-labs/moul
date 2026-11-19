# Rules

Each collection has six rules. A rule is a condition in a small expression language; a request is
allowed when it holds for the caller and the record.

| Rule | Decides |
| --- | --- |
| `listRule` | Who may list records, and which ones they see (it becomes part of the query) |
| `viewRule` | Who may read one record |
| `createRule` | Who may create a record, and with what data |
| `updateRule` | Who may change a record |
| `deleteRule` | Who may delete a record |
| `subscribeRule` | Who hears about changes (the view rule when empty) |

An **empty rule lets everyone through**, guests included, with one exception: on an auth
collection, an empty list, view, update or delete rule is stored as `id = @request.auth.id`, so
people see and change only their own account. Write `true` to open one of those to everyone. The
owner (Studio, and clients made with `owner: true`) is never checked. Run [`moul check`](testing.md#moul-check) to catch rules that are
open by mistake.

## Invalid rules are refused

Saving a collection (`POST /api/moul`, `PATCH /api/moul/{name}`, `POST /api/schema`, or Studio's
Save) compiles each of its rules against the collections as they are once the save is made, and
refuses a rule that does not compile, a field that does not exist included: nothing is saved,
and the answer is a 400 naming the rule.

```json
{ "message": "Invalid listRule: invalid field \"ownr\"", "rule": "listRule" }
```

A schema file is checked once all of it is applied, so a rule may reach a collection the file
makes later; the message names the collection too. A change that breaks a rule which compiled,
such as removing a field it reads, is refused the same way. A database made by the Go engine,
which stored any rule text, opens with its rules as they are; a broken one is refused the next
time its collection is saved. `moul check` reports every broken rule at once.

## Common rules

```text
@request.auth.id != ''                                  signed in
owner = @request.auth.id                                the caller's own records
@request.auth.id != '' && @request.body.owner = @request.auth.id    create only as yourself
status = 'published' || owner = @request.auth.id        public once published
id = @request.auth.id                                   (auth collections) your own account
@request.body.role:isset = false                        a field the caller may not set
@request.body.role:changed = false                      a field the caller may not change
@collection.members.user = @request.auth.id && @collection.members.team = team
                                                        membership through another collection
```

## What a rule can use

- **The record's fields**, by name: `owner`, `status`, `title`. In list rules and filters, relation
  paths reach into the related record: `project.owner = @request.auth.id`. The other rules are
  checked against one record, which holds only the related record's id.
- **`@request.auth.*`**: the signed-in person (`id`, `email`, `username` and their record's
  fields). Empty for a guest.
- **`@request.body.*`**: the data a create or update sends (not in list rules: a list has no
  body). **`@request.query.*`** and
  **`@request.headers.*`**: the request's query and headers.
- **`@collection.<name>.<field>`**: rows of another collection, for membership and roles.
- **`@now`**, **`@todayStart`**: the time.
- **Modifiers**: `:isset` (the body has the field), `:changed` (the update changes it), `:length`,
  `:lower`, `:each` (every item of a list field).
- **Operators**: `=`, `!=`, `>`, `>=`, `<`, `<=`, `~` (contains), `!~`, and the "any item" forms
  `?=`, `?!=`, `?>`, `?~` … for list fields; `&&`, `||` and parentheses. There is no `!` to negate
  a condition: use the negated operator (`!=`, `!~`, `?!=`), compare a bool field or a modifier
  with `false` (`done = false`, `@request.body.role:isset = false`), and rewrite a compound
  condition with those (`!(a = 1 && b = 2)` is `a != 1 || b != 2`). Text goes in single or
  double quotes.
- **Functions**: `exists(table, condition, …)` and `get(…)` read other tables directly. A rule
  that uses them makes every live query on the collection refresh on every commit, because it may
  read any table. `strftime(…)` formats a date. A list rule becomes part of the query and calls no
  function: use `@collection.<name>.<field>` or a relation path there.

The language is the Go engine's, read the same way: the Go version's rule tests are replayed
against this one, error messages included.

## Trying rules

Studio's rules editor checks a rule as you type and can test it against a sample record and
person. From code: `moul.admin.testRule({ rule, record, auth })`.

## Protection in a tab

In a tab, the person at the keyboard owns the data: anyone with the page can read the file, and
rules do not stop them. Rules are there so you can build and test an app that several people will
use; they become real protection when the same database is served by Moul Hub.
