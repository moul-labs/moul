# Moul Playground

[play.moul.dev](https://play.moul.dev): Studio on a database in your browser, with no account and
no server. Start empty, from the sample app, from a `.db` file or from a spreadsheet (choose the
file, or drop it on the page); switch between the databases this browser keeps; works offline once
loaded. A join link (`#moul-join=…`) always opens a new database that follows the bucket.

After a deploy, the new service worker takes over only once no tab uses the old one, so open tabs
keep loading their code from the old version's cache; a tab that still finds code missing loads the
page again, once.

```sh
npm run dev -w moul-playground        # localhost:5175, Studio from source
npm run build -w moul-playground      # dist/: static files, a service worker and _headers
npm run test -w moul-playground       # spreadsheet import and the sample app, on the headless engine
node browser-tests/playground.mjs     # the built site in Chromium, Firefox and WebKit
```

`browser-tests/playground.mjs` also syncs two devices through a bucket when `MOUL_S3_ENDPOINT` is
set (`scripts/start-rustfs.sh` starts one locally).

It is static: `npm run deploy -w moul-playground` uploads `dist/` to Cloudflare (`wrangler.jsonc`)
at play.moul.dev. `public/_headers` sets the content security policy: only this site's own scripts,
WebAssembly, and connections to the sync bucket a builder chooses.

The address is part of the product: a browser keeps each site's data apart, so moving the
Playground to another address would strand everyone's databases.
