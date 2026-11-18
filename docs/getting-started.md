# Getting started

## In the Playground

Open [play.moul.dev](https://play.moul.dev). There is no account and no server: the engine starts
in the page, and the data is a SQLite file in your browser.

1. **Start a database.** Start empty, load the sample app (people, projects and tasks with rules),
   open a `.db` file, or drop in a spreadsheet (CSV): each column becomes a field and each row a
   record.
2. **Shape the data.** In Studio, add collections and fields, and set the rules: who may list,
   view, create, update and delete records ([rules](rules.md)).
3. **Try it as a user.** Sign-in codes and reset links land in Studio's **Inbox**, because a tab
   cannot send mail.
4. **Keep it.** Studio says when nothing is backed up. Download the file any time (the button is on
   every screen), or turn on [sync](sync.md) to keep it on your other devices.

Databases made in the Playground belong to play.moul.dev: a browser keeps each site's data apart.
To use one in your own app, download the file and load it there, or sync both to the same bucket.

## In your app

Install the SDK. It starts the engine in a worker inside your app's page.

```sh
npm install @moul-dev/client
```

```ts
import { createClient } from '@moul-dev/client';
import schema from './moul.schema.json';

const moul = createClient({ db: 'notes', schema });

await moul.collection('notes').create({ text: 'Buy oat milk' });

// A live query: it runs again whenever a commit touches what it read, from this tab, another
// tab, or another device through sync.
moul
  .collection('notes')
  .live({ sort: '-createdAt' })
  .subscribe(({ data }) => render(data?.items ?? []));
```

`schema` is the schema file: collections, fields and rules. Download it from Studio's **Schema &
types** page or write it by hand; the client applies it before its first request. Check it with
[`moul check`](testing.md#moul-check), and generate TypeScript types with `moul typegen` (or copy
them from Studio) to type your records:

```ts
import type { MoulSchema } from './moul-types';
const moul = createClient<MoulSchema>({ db: 'notes', schema });
```

### Bundlers

The SDK starts its worker with `new URL('./worker.js', import.meta.url)`, which Vite, webpack,
Rollup and esbuild follow. With Vite, keep the SDK out of dependency pre-bundling:

```ts
// vite.config.ts
export default { optimizeDeps: { exclude: ['@moul-dev/client'] } };
```

Without a bundler, pass the worker yourself: `createClient({ db, worker: () => new Worker(url, {
type: 'module' }) })`.

### Studio inside your app

```ts
import '@moul-dev/studio';
document.querySelector('moul-studio')!.client = moul;
```

```html
<moul-studio></moul-studio>
```

See [Studio](studio.md).

## What to know

- **The person at the keyboard owns a database in a tab.** Rules and sign-in let you build and test
  an app that several people will use; they protect data once the same database is served by Moul
  Hub. In a tab, anyone with the page can read the file.
- **One SQLite file.** The shared store is the file you download. Password hashes, keys and this
  device's own state live in two other stores that never leave the device.
- **Browsers may clear site data.** Studio asks the browser to keep it and shows when nothing is
  backed up. On iPhone and iPad, add the app to the Home Screen so Safari keeps it.
- **Private windows** may refuse durable storage. The engine then runs in memory and says so.
- **One writer at a time.** With sync, one device writes and the others follow; any can take over.
