# The SDK: `@moul-dev/client`

The SDK starts the engine inside your page (each tab starts the engine's worker; one tab holds
the database and the others send their requests to it) or talks to a Moul server over HTTP. It sends the same requests either way, so code written against a database
in a tab keeps working when the app is later served by Moul Hub.

```ts
import { createClient } from '@moul-dev/client';

const moul = createClient({ db: 'notes' });        // the engine in this page
const remote = createClient({ url: 'https://…' });  // a Moul server, over HTTP
```

| Option | Meaning |
| --- | --- |
| `db` | The database name (letters, digits, `-`, `_`). Each name is its own database; the site's tabs share it. Default `app`. |
| `schema` | A schema file to apply before the first request. Collections are created and changed to match; none are deleted. See `moul.ready`. |
| `owner` | Send every request as the database's owner, so rules do not apply. Studio does this. |
| `worker`, `wasmUrl` | Where the engine's worker and `.wasm` file come from, when the bundler cannot find them. |
| `persist` | Ask the browser to keep the site's storage after the first write. Default `true`. |
| `authStorage`, `authKey` | Where the sign-in is kept: `localStorage` by default, shared by the tabs. |
| `url`, `fetch`, `adminKey` | For a remote server. |

Pass your record types (from `moul typegen`, or Studio's **Schema & types** page) to get typed
collections: `createClient<MoulSchema>({ db })`.

## Records

```ts
const tasks = moul.collection('tasks');

await tasks.create({ title: 'Mop', done: false, owner: moul.user!.id });
await tasks.create({ id: 'task-mop', title: 'Mop' });   // choose the id (a record made offline keeps it)
await tasks.update(id, { done: true });                   // only the fields given change
await tasks.delete(id);

await tasks.get(id, { expand: 'project' });
await tasks.list({ filter: 'done = false', sort: '-createdAt', expand: 'project', page: 1, perPage: 50 });
await tasks.all({ filter: "project = 'p1'" });            // every match, 500 at a time
await tasks.first('title = "Mop"');                       // or null
```

A list answers `{ page, perPage, totalItems, totalPages, items }`. Filters use the [rule
language](rules.md) and only see what the list rule lets the caller see. Values that are `Blob`s
or `File`s are uploaded first and stored as file references:

```ts
await tasks.create({ title: 'Receipt', attachment: fileInput.files[0] });
```

Failures throw `MoulError` with the engine's `status`, `message` and `data`.

## Live queries

Every read can stay current. A live query runs when its first listener subscribes and again
whenever a commit touches what it read, whoever made the change: this tab, another tab, another
device through sync, or a sign-in that changes what the rules allow.

```ts
const open = moul.collection('tasks').live({ filter: 'done = false', sort: 'due' });
const stop = open.subscribe(({ data, error, loading }) => {
  if (loading) return showSpinner();
  render(data!.items);
});
// later
stop();
```

- `loading` is true until the first answer. Later refreshes run in the background and keep `data`.
  When the signed-in person changes, the previous person's results are dropped at once (`data`
  becomes `undefined` and `loading` true) until the query has read again.
- An unchanged result keeps its identity and wakes nobody.
- A burst of writes causes one refresh.
- `liveGet(id)` follows one record (`null` once it is deleted or hidden); `liveAll()` follows every
  match of a small collection; `await query.value()` reads now and gives the result, and
  `query.refresh()` reads again, listened to or not.

How it knows: the engine answers each read with the collections it looked up (`Moul-Reads`: the
collection, the targets of relations it expanded or filtered through, the signed-in person's own
record) and announces each commit with the rows it added to the change log. A query refreshes
when the two meet, when definitions change, or when a rule it ran may read any table.

For anything else, `moul.live(run)` makes a live query from your own function, and
`moul.onChange(notice => …)` hears every commit when the engine runs in the page or headless; over
HTTP it never fires.

## Subscriptions

Live queries say what is there; subscriptions say what changed:

```ts
const stop = await moul.collection('tasks').subscribe((e) => {
  console.log(e.action, e.record, e.old_record); // 'create' | 'update' | 'delete'
}, { events: ['create', 'delete'] });
```

Events pass the collection's subscribe rule (or its view rule) as the subscriber. A record that
an update hides arrives as a delete. Pass `signal` (an `AbortSignal`) to cancel a subscription
even before it is made, and `onClose` to hear when the engine ends one (a revoked sign-in, or a
device that caught up from a snapshot, after which events in between are unknown). Over HTTP,
`onReconnect` says the stream broke and events in the break were lost.

On a device that follows another writer, changes arrive from the bucket, and a delete (or an
update that hides a record) is told only to subscribers that knew the record. Pass the ids you
already show as `visible` to hear about those; records you learn of through events are tracked
for you, and sent again when the subscription is made again on another tab.

An error a listener throws is reported (as an uncaught error in browsers) and stops neither the
subscription nor the other listeners; the same holds for live queries.

## Sign-in

```ts
const users = moul.auth('users');

await users.signUp({ email: 'ana@example.com', password: 'Password123!' });
await users.signInWithPassword('ana@example.com', 'Password123!');

await users.requestCode('ana@example.com');      // a tab cannot send mail: see Studio's Inbox
await users.signInWithCode('ana@example.com', '123456');

moul.user;                    // the signed-in record, or null
moul.onAuthChange((s) => …);  // here or in another tab
await users.refresh();
await users.signOut();
await users.requestPasswordReset(email);
await users.confirmPasswordReset(token, newPassword);
```

The sign-in is kept in `localStorage` and shared by the site's tabs. Live queries refresh when it
changes.

## Files

```ts
const file = await moul.files.upload(blob);          // { id, url, filename, size }
const src = await moul.files.url(record.attachment); // a blob URL for <img src> or <a href>
moul.files.release(record.attachment);               // free it
const blob = await moul.files.fetch(record.attachment);
```

A page has no server to serve files from, so `url()` hands out blob URLs. Types that could run
script in your page (HTML, SVG) come back as `application/octet-stream`, so they download instead
of opening. Against a remote server, `url()` returns the server's file URL.

## The database file

```ts
await moul.database.status();   // { durable, note, role, device, lastCommit, readOnly, syncing }
const bytes = await moul.database.export();   // a SQLite file: your collections and records
await moul.database.download('notes.db');     // through the browser's download
await moul.database.import(fileOrBytes);      // replaces the data; a Go engine database is converted
await moul.database.persist();                // ask the browser to keep the site's storage
```

The exported file is the shared store. Password hashes, keys and this device's own state live in
two other stores and are never exported. `durable: false` means the browser offered no storage
(some private windows): the engine runs in memory.

## Sync

```ts
await moul.sync.configure({ endpoint, bucket, accessKeyId, secretAccessKey, label: 'Laptop' });
await moul.sync.status();     // role, writer, position, pending, lastSyncAt, lastError, conflicts
await moul.sync.now();
await moul.sync.takeOver();   // this device writes; the previous writer follows
await moul.sync.disconnect();
```

See [Sync](sync.md). `joinLink(settings, page)` and `readJoinLink(url)` carry settings to another
device in a URL fragment; `readJoinLink` accepts only well-formed settings with an `https`
endpoint (or `http` to this computer). Connect from a link with `followOnly: true`: the device
then only follows a replica that exists, keeps its own data as a conflict copy, and never uploads
it to a bucket the link chose.

## The owner's tools

`moul.admin` sends everything as the owner: `listCollections`, `liveCollections`,
`createCollection`, `updateCollection`, `deleteCollection`, `schema()`, `applySchema(file, {
prune })`, `types()`, `changes({ after })`, `inbox()`, `liveInbox()` and `testRule({ rule, record,
auth })`. `collection(name).export('csv' | 'json')` and `.import(file)` move rows in and out.

`moul.asOwner()` is a client on the same engine and sign-in that sends as the owner.

## Requests underneath

Every method sends an HTTP-shaped request (`moul.send`, `moul.request`), the same one a Moul server
answers. Writes the engine logs (records, definitions, uploads, the schema file) carry a request id
when they come from the owner or a signed-in person; if the tab that holds the database closes
before answering, the client sends such a write again and the engine applies it once. Other
writes (sign-in, token refresh) are never sent twice. A path with a `.` or `..` step is refused:
a URL would resolve it to another resource.

## Transports

| Transport | Engine |
| --- | --- |
| `WorkerTransport` | In a dedicated worker in the page; one tab holds the database, the others forward to it, and another takes over when it closes |
| `EngineTransport` | In this thread: the headless engine, for tests ([Testing](testing.md)) |
| `HttpTransport` | A Moul server over HTTP; subscriptions over server-sent events. Live queries follow the collections they read through those events, and this client's own writes. |
