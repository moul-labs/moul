# moul-web

Browser host for the Moul engine: worker entry point, message protocol and tab coordination.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** early development. The API changes without notice until the first
release.

## How tabs share one database

Only one worker per site can open the database files, because an OPFS sync
access handle locks its file. Every tab starts a dedicated worker running
`MoulWorker`. The workers ask for the same Web Lock (`moul:<db>`); the one
that gets it opens the stores and answers requests, and the others forward
theirs over a `BroadcastChannel`. When the leading tab closes, the browser
hands the lock to the next tab, which reopens the database; requests that
were in flight fail and can be sent again.

Without OPFS (private windows in some browsers) the engine runs in memory and
`status` says so. After the first write, the leading worker posts
`{ type: "moul:persist" }` to its page, which should call
`requestPersistence()`: only pages may ask for persistent storage.

The Phase 1 requests are `exec`, `query`, `export`, `import` and `status`.
They exercise storage end to end and are replaced by named requests in Phase 4.

## License

MIT
