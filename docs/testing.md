# Testing

## Without a browser

`@moul-dev/client/headless` runs the same engine in memory, in the test's own thread, in Node or any
JavaScript runtime with WebAssembly. Each call makes a fresh, empty database.

```ts
// tasks.test.ts, with Vitest
import { test, expect } from 'vitest';
import { createHeadlessClient } from '@moul-dev/client/headless';
import { MoulClient } from '@moul-dev/client';
import schema from '../moul.schema.json';

test('people see only their own tasks', async () => {
  const owner = await createHeadlessClient({ schema, owner: true, now: '2026-11-18T09:00:00Z', seed: 1 });
  const app = new MoulClient(owner.transport, { authStorage: null }); // the app's own view, with rules

  await app.auth('users').signUp({ email: 'ana@example.com', password: 'Password123!' });
  await app.auth('users').signInWithPassword('ana@example.com', 'Password123!');
  await app.collection('tasks').create({ title: 'Mop', owner: app.user!.id });

  await app.auth('users').signOut();
  expect((await app.collection('tasks').list()).totalItems).toBe(0);
});
```

| Option | Meaning |
| --- | --- |
| `schema` | A schema file to apply first; the call fails if it does not apply |
| `owner` | The returned client sends as the owner |
| `now` | Fix the clock (a date, string or milliseconds); move it with `client.advance(ms)` |
| `seed` | Make ids, tokens and salts repeat from run to run |

Live queries, subscriptions, files and sign-in codes (`owner.admin.inbox()`) all work headless.

## `moul check`

`moul check` validates a schema file before it reaches anyone: every definition, every rule
compiled against its collection, relations to collections that exist, and warnings for rules that
let anyone write.

```sh
moul check                    # ./moul.schema.json
moul check app.db             # the definitions in a database file
moul check --json --strict    # for CI and agents: warnings fail too
```

It exits 0 when the file is valid, 1 when it has errors (or warnings, with `--strict`), and 2 when
it cannot read the file. `--json` prints `{ ok, file, collections, errors, warnings }`, each finding
with a `location` such as `collections[2] (tasks).rules.listRule` and a `message`.

Install the command with `cargo install moul-cli`, or download a build from the
[releases](https://github.com/moul-labs/moul/releases).
