# @moul-dev/studio

Moul Studio, the admin console for a Moul database, as one element: `<moul-studio>`. Collections,
fields and rules (with a rule tester), records, the local inbox of sign-in codes, the database file,
and sync with your other devices.

```sh
npm install @moul-dev/studio @moul-dev/client
```

```ts
import { createClient } from '@moul-dev/client';
import '@moul-dev/studio';

const moul = createClient({ db: 'notes' });
document.querySelector('moul-studio')!.client = moul; // share your app's engine
```

```html
<moul-studio style="height: 100vh"></moul-studio>
```

Or `<moul-studio db="notes"></moul-studio>` to let Studio start the engine itself. Studio waits a
moment after it connects before doing so, so setting `client` right away starts no second engine;
a `client` given later closes the engine Studio started. Changing `client` or `db` moves Studio to
that database and drops what it showed of the last one. `routing="hash"` keeps Studio's page in the
URL fragment, for a page that is only Studio; the default keeps it out of your app's address bar.

Studio's theme and variables stay on the element. It adds Moul UI's design tokens to the page once,
because its toasts appear in `document.body`.

Opening a join link (`#moul-join=…`, made on the Sync page) shows the Sync page with the link's
bucket filled in. Connecting asks first, and the device only follows the bucket: it copies the
bucket's data and keeps its own as a conflict copy, and never uploads it to a bucket a link chose.

Studio works as the database's owner: rules do not apply to it. Show it only to yourself.

It is the console the [Playground](https://play.moul.dev) runs. Docs:
[Studio](https://github.com/moul-labs/moul/blob/main/docs/studio.md).

## Development

```sh
npm run dev -w @moul-dev/studio     # Studio on a database in this browser, at localhost:5174
npm run build -w @moul-dev/studio   # dist/moul-studio.js and its types
```

Studio is built with Lit and [Moul UI](https://www.npmjs.com/package/@moul-dev/web-component). The
build leaves Lit, Moul UI, TanStack DB and the SDK to the app's bundler, so an app and its Studio
share one copy of each, and one engine.

## License

MIT
