# Studio

Studio is Moul's admin console: collections, fields and rules, records, the local inbox, the
database file and sync. It works as the database's owner, so rules do not apply to it.

## Where it runs

- **In the Playground** at [play.moul.dev](https://play.moul.dev), full page.
- **Inside your app**, as the `<moul-studio>` element from `@moul-dev/studio`. A browser keeps each
  site's data apart, so a Studio inside your app is the way to see your app's own data.

```sh
npm install @moul-dev/studio @moul-dev/client
```

```ts
import { createClient } from '@moul-dev/client';
import '@moul-dev/studio';

const moul = createClient({ db: 'notes' });
document.querySelector('moul-studio')!.client = moul; // share the app's engine
```

```html
<moul-studio style="height: 100vh"></moul-studio>
```

Or let Studio start the engine itself: `<moul-studio db="notes"></moul-studio>`.

| Attribute / property | Meaning |
| --- | --- |
| `client` (property) | The client to work through. Studio uses it as the owner. |
| `db` | The database to open when no `client` is set. Default `app`. |
| `routing` | `memory` (default) keeps Studio's page out of your app's address bar; `hash` keeps it in the URL's fragment (`#/records/tasks`), for a page that is only Studio. |

One Studio per page. Its theme switch applies to the `<moul-studio>` element only and leaves the
page's `color-scheme` alone; the one thing it adds to the page is Moul UI's design tokens, once,
because its toasts render in `document.body`. Show it only to yourself: in a tab anyone with the page owns the data
anyway, but a Studio in a deployed app would hand every visitor the owner's tools.

## Pages

| Page | What it does |
| --- | --- |
| Overview | Collections at a glance; where the data is kept; sync |
| Collections | Fields, relations and rules, with a rule tester |
| Records | A grid per collection: search, filter, sort, edit, import and export CSV or JSON |
| Realtime | Record events as they happen, from any tab or device |
| Inbox | Sign-in codes and reset links, which a tab cannot email |
| Database | Where the data is kept, download and load the file, the change log |
| Sync | Connect a bucket, see which device writes, take over, add a device by link or QR code, conflict copies |
| Schema & types | The schema file and TypeScript types, to download or apply |

Every screen has a download button for the database file, and a banner when nothing is backed up:
not synced and not downloaded since the last change.
