# @moul-dev/client

The Moul SDK: a backend inside your web page. A database with collections and records, access
rules, sign-in, files, subscriptions and **live queries**, kept as one SQLite file in the browser
and synced, if you want, to a bucket you own or through a Moul Hub, as a Litestream replica. The
same client talks to a Moul server over HTTP.

```sh
npm install @moul-dev/client
```

```ts
import { createClient } from '@moul-dev/client';
import schema from './moul.schema.json';

const moul = createClient({ db: 'notes', schema });

await moul.collection('notes').create({ text: 'Buy oat milk' });

moul
  .collection('notes')
  .live({ sort: '-createdAt' })
  .subscribe(({ data }) => render(data?.items ?? []));
```

- **No server.** The engine (Rust, compiled to WebAssembly, about 1.5 MB gzipped) runs in a worker.
  Each tab starts one; one tab holds the database and the others send their requests to it.
- **Live by default.** A live query refreshes whenever a commit touches what it read: this tab,
  another tab, another device through sync, or a sign-in.
- **Your data is a file.** `moul.database.download()` saves it; any SQLite tool reads it.
- **Tests without a browser.** `@moul-dev/client/headless` runs the same engine in Node.
- **Sync to your other devices.** `moul.sync.configure(...)` with a bucket's key, or with a Moul
  Hub: `HubClient` signs in with an email code, and followers pull as soon as a change lands.

```ts
import { HubClient, joinHub, joinLink, readJoinLink } from '@moul-dev/client';

const hub = new HubClient('https://sync.moul.dev');
await hub.signIn('me@example.com'); // a code arrives by mail
const { token } = await hub.verify('me@example.com', '123456', { label: 'Laptop' });
await hub.createDatabase(token, 'notes');
await moul.sync.configure({ hub: hub.url, database: 'notes', token, label: 'Laptop' });

// Another device: a link with a one-time ticket, never a token.
const link = joinLink(await moul.sync.joinTicket(), location.href);
// …and on the device that opens it:
const join = readJoinLink(location.href);
if (join && 'hub' in join) await moul.sync.configure(await joinHub(join, { label: 'Phone' }));
```

With Vite, add `optimizeDeps: { exclude: ['@moul-dev/client'] }`: the worker loads through
`new URL('./worker.js', import.meta.url)`.

Docs: [getting started](https://github.com/moul-labs/moul/blob/main/docs/getting-started.md),
[the SDK](https://github.com/moul-labs/moul/blob/main/docs/sdk.md),
[rules](https://github.com/moul-labs/moul/blob/main/docs/rules.md),
[sync](https://github.com/moul-labs/moul/blob/main/docs/sync.md),
[Moul Hub](https://github.com/moul-labs/moul/blob/main/docs/hub.md),
[testing](https://github.com/moul-labs/moul/blob/main/docs/testing.md).
For AI tools: [`llms.txt`](llms.txt), [`AGENTS.md`](AGENTS.md) and [a skill](skills/moul/SKILL.md)
ship in the package.

**Status:** 0.1, the first public release. The API may change before 1.0.

## License

MIT
