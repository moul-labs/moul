# moul-web

Browser host for the Moul engine: worker entry point, message protocol and tab coordination.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1, the first public release. The API may change before 1.0.

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

## Requests

The leading tab runs the engine (`moul-core`). Requests are plain objects
with an `op`: `api` carries the same HTTP-shaped request a Moul server takes
(method, path, query, headers, a JSON, form or bytes body, `owner`, a request
id) and answers `{ status, headers, json | bytes }`; `subscribe` and
`unsubscribe` manage record subscriptions (a subscribe answer, `{ ok, id,
leader }`, names in `leader` the leading tab that holds the subscription, the
id its `{ type: "leader", tab }` messages carry, so a page can drop the
answers of a tab that no longer leads; `MoulEngine` has no tabs and leaves it
out); `status`, `export` and `import`
work on the database file; `exec` and `query` run the owner's raw SQL
(`query` only statements that read). See the `api` and `host` modules.

After every commit the leader pushes `{ type: "changes", commit, changes,
all }` to every tab, with the new rows of the change log; live queries refresh
from it. Subscription events go only to the tab that subscribed. Each tab
holds a Web Lock while it lives, so the leader drops the subscriptions of tabs
that closed. Tabs hear `{ type: "leader" }` when another tab takes over, and
then subscribe again. `MoulWorker.onPush` sets the function that receives
these; it is called on a microtask, in order, so it may call back into the
worker. When the data is replaced in a way the log does not describe event by
event (the device caught up from a snapshot, or a database was imported),
every subscription ends with `{ type: "closed", id, reason }`.

`MoulEngine` is the same host without a worker, tabs or OPFS: the engine in
memory, in the calling thread, for tests in Node (`@moul-dev/client/headless`)
and tools. It can run on a fixed clock and a seed, so a test replays exactly.

If the browser keeps refusing OPFS for no stated reason (WebKit's private
windows answer `UnknownError`), the engine runs in memory and `status` says
why, unless OPFS already holds a database by that name: then it keeps trying,
and never hides that database behind an empty one. Where the browser refuses
OPFS itself (`getDirectory()` fails), nothing stored there can be read or
overwritten, so the engine runs in memory whatever else the site stores, such
as a service worker's cache. If the database cannot be
opened after ten tries, the leader answers every request with the reason
instead of leaving pages waiting, and keeps trying, at most five seconds
apart: once the database opens, it serves it, without a reload, and pushes
`{ type: "leader", tab, opened: true }` to every tab, so pages read again and
subscribe again to what it refused.
`MoulWorker.close()` stops background sync and releases the leader lock once
the files are closed, so the next tab can open them; a worker dropped without
it stops trying and lets go of the lock too.

## Sync

The leading worker syncs the shared store with an S3-compatible bucket, or
through a Moul Hub (`moul-sync`), about once a second while the page is open.
The requests are `syncConfigure`, `syncStatus`, `syncNow`, `takeOver`,
`syncDisconnect`, `syncJoinTicket` (a one-time ticket from the hub for a join
link), and `conflicts`, `conflictExport` and `conflictDelete` for the copies
kept when a device had changes the replica does not (in OPFS,
`moul/<db>-conflicts/`). They answer with a promise. The settings, keys and
hub tokens included, stay in the device store; a status never shows a secret
key or a token. Through a hub, the worker holds the hub's event stream open
and pulls as soon as another device pushes, polls every 30 seconds besides,
and leaves compaction to the hub. While the device follows another writer, the engine answers writes
with 409, `exec` runs with `PRAGMA query_only` and `import` is refused. So
it does while sync replaces the database (joining a replica, or stepping
down as the writer), so that no acknowledged write is lost. When
pages from the writer arrive, the engine reads the change log and the leader
pushes a notice marked `all`, so every live query refreshes. When sync
replaces the database as a whole (joining a replica, resyncing after another
device took over, or a snapshot), every subscription ends with
`{ type: "closed" }`.

`syncDisconnect` stops syncing but remembers the replica. A device that
disconnects keeps what it writes meanwhile: connecting to the same replica
again pushes it if the device still writes, or keeps it as a conflict copy if
another device took over. To do that it keeps its level-0 files (the LTX
files of its commits) in OPFS, and they grow with every commit until it
connects again. `syncDisconnect` with `{ forget: true }` also forgets the
replica: the device counts as one that never joined, so those files go at
once and after every commit from then on. Connecting again later joins the
replica as a new device would, under a new device id: what the device holds
then is kept as a conflict copy, and it takes the replica's data and follows,
even if it was the writer. If disconnecting fails, sync still stops (and the
replica is still forgotten) before the error is returned.

## License

MIT
