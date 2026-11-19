# Sync through Moul Hub

Sync normally goes straight from the browser to a bucket you own. Moul Hub is
the other way: a server that fronts the bucket for you. Each device signs in
with your email instead of holding a bucket key, followers hear about new
changes the moment they land instead of polling, and the hub compacts the
replica on its own clock, so a phone that sleeps for a week still finds a
tidy replica.

The replica a hub keeps is the same Litestream-compatible replica as a bucket
holds: `moul restore`, `moul pull` and `litestream restore` read it.

| | Your own bucket | Moul Hub |
| --- | --- | --- |
| What a device holds | A key scoped to the bucket | Its own device token, revocable |
| Who sets it up | You: bucket, CORS rule, key | Nothing: sign in with your email |
| New changes reach followers | Every few seconds, by polling | As they land, by a push |
| Compaction and retention | The writing device, when it is open and idle | The hub, on a timer |
| Who can read the replica | Whoever has the bucket's keys | Whoever runs the hub, too |

The hub is one program, `moul hub`. Run your own, or use the hosted beta at
`https://sync.moul.dev` (below).

## Running a hub

```sh
moul hub ./hub-data --listen 0.0.0.0:8091 --public-url https://sync.example.com
```

The folder holds the hub's directory (accounts, devices, databases) and, by
default, the replicas under `replicas/`. `--store s3://bucket/prefix?endpoint=…`
keeps the replicas in a bucket instead (credentials as for `moul pull`).
Sign-in codes go out by mail through the hub's mail settings (the `email_*`
settings of [the server](server.md), set with `PATCH /api/settings` and the
admin key, or the `EMAIL_*` environment variables); until mail is set up
`moul hub` prints each code as it is written.

| Flag | Default | |
| --- | --- | --- |
| `--listen` | `127.0.0.1:8091` | Address and port |
| `--public-url` | `http://<listen>` | The address devices use, for links in mail |
| `--store` | `<folder>/replicas` | Where replicas live: a folder or an `s3://` URL |
| `--signup` | `closed` | `open`: anyone may sign up; `invite`: with an invite code; `closed`: only accounts the admin made |
| `--max-databases` | `10` | Databases per account |
| `--max-bytes` | `1073741824` | Bytes stored per account (1 GiB) |
| `--admin-key` | made on first start | For the admin endpoints below; also `MOUL_ADMIN_KEY` |

The admin, with the admin key as `X-Admin-Key`:

| Request | Body | Answer |
| --- | --- | --- |
| `GET /api/hub/admin/accounts` | | every account, with its databases and bytes |
| `POST /api/hub/admin/accounts` | `{"email"}` | `201`: an account that may sign in whatever `--signup` says |
| `POST /api/hub/admin/invites` | `{"uses"?}` | `201 {"code", "uses"}`: a code for `--signup invite` |

## The protocol

Everything is under `/api/hub` and speaks JSON, except object bodies, which
are raw bytes. Errors are `{"message": "…"}` with an HTTP status. A device
sends its token as `Authorization: Bearer <token>`.

There are two kinds of device token, both starting `mh_`:

- An **account token**, from signing in with an email code. It reaches every
  database of the account and the account's settings.
- A **database token**, from a join link. It reaches one database only, so a
  phone that scanned a QR code cannot read the account's other databases.

Tokens do not expire; they end when revoked (`DELETE /api/hub/devices/:id`).
The hub stores only their SHA-256. Times in answers are milliseconds since
the epoch.

### Signing in

| Request | Body | Answer |
| --- | --- | --- |
| `POST /api/hub/signin` | `{"email", "invite"?}` | `200 {"sent": true}`. The same answer whether or not a code was sent, so it does not say which emails have accounts. At most 6 codes an hour for one email and 30 for one address (`429`). |
| `POST /api/hub/verify` | `{"email", "code", "label"?}` | `200 {"token", "device", "account"}`; `400` for a wrong or expired code; `429` after too many guesses |
| `POST /api/hub/join` | `{"ticket", "label"?}` | `200 {"token", "device", "database"}`, where `database` is the name: a database token; `400` for a used or expired ticket |
| `GET /api/hub/me` | | `200 {"account", "device", "limits", "usage"}`; for a database token, `account` is only `{"id"}` and `usage` that database's |
| `GET /api/hub/devices` | | `200 {"items": [device…]}` (account tokens) |
| `DELETE /api/hub/devices/:id` | | `204`; that device's token stops working at once. A database token may revoke only itself. |

`account` is `{"id", "email"}`. `device` is `{"id", "label", "database"
(null for an account token), "createdAt", "lastSeenAt", "current"}`.
`limits` and `usage` are `{"databases", "bytes"}`.

### Databases

| Request | Body | Answer |
| --- | --- | --- |
| `GET /api/hub/databases` | | `200 {"items": [database…]}` |
| `POST /api/hub/databases` | `{"name"}` | `201 database`; `409` if the account has one by that name; `403` over the account's limits |
| `DELETE /api/hub/databases/:name` | | `204`; the replica is deleted |
| `POST /api/hub/databases/:name/joins` | | `201 {"ticket", "expiresAt"}`: a ticket for one device, valid 15 minutes. A database token may ask for its own database. |

`database` is `{"name", "createdAt", "bytes", "head"}` where `head` is the
replica's `moul/head.json` (`{"txid", "writer", "generation", "replica",
"at"}`) or `null` before the first push. A name is 1 to 63 of `a-z`, `0-9`
and `-`, starting with a letter or digit.

### The replica

These five calls are the replica interface (`get`, `put` with a condition,
`list`, `delete`) over HTTP, plus a stream of pushes.

| Request | Answer |
| --- | --- |
| `GET /api/hub/db/:name/o/<key>` | `200` with the bytes and `ETag: "<etag>"`; `404` if there is no such object |
| `PUT /api/hub/db/:name/o/<key>` | Body: the bytes. `If-None-Match: *` writes only a new key; `If-Match: "<etag>"` only over that version. `200 {"etag"}` with `ETag`; `412` when the condition fails and nothing was written; `413` over the size limit; `403` over the account's limit |
| `DELETE /api/hub/db/:name/o/<key>` | `204`, also for a missing key |
| `GET /api/hub/db/:name/list?prefix=<p>` | `200 {"items": [{"key", "size", "modified"}]}` in key order; `modified` in milliseconds since the epoch |
| `GET /api/hub/db/:name/events` | Server-sent events, below |

A key is one of `ltx/<level>/<min>-<max>.ltx` (level 0 to 9, 16 hex digits
each), `moul/head.json`, `moul/writer.json`, the same three under
`moul/writer-only/`, or `files/<sha256>`. Anything else answers `400`. One
object may be at most 256 MiB, and one account sends at most four at a time.

The hub checks each condition and writes under one lock per database, and
passes the condition on to a bucket that supports it, so two devices can never
both write a TXID, whatever store is behind the hub.

The event stream starts with `event: connected` (`data: {"database"}`), sends
`event: head` with the new `moul/head.json` each time a device writes it, and
`: ping` every 15 seconds. A follower that holds the stream open syncs as soon
as `head` arrives and polls only as a fallback.

The hub compacts each database it fronts, as a writer would (level 1 every 5
minutes, level 2 every hour, a snapshot every day, 7 days of history), so
devices that sync through a hub do not compact.

## The hosted beta

`https://sync.moul.dev` runs `moul hub` for builders who would rather not run
their own. During the beta:

- **Sign-up is by invitation.** Ask for a code; sign in with it once, then
  with your email alone.
- **Each account keeps up to 10 databases and 1 GiB.**
- **The hub can read your replicas.** They are not encrypted; Moul will follow
  Litestream's per-page encryption once it exists. Keep data you would not
  trust to a host in your own bucket, or run your own hub.
- **It is a beta.** Keep a copy (`moul pull`, or Studio's database file) of
  anything you cannot lose. Leaving is `moul pull` and pointing the app at
  your own bucket or hub.

Pricing is decided before the beta ends; a self-hosted hub stays free.

## In the SDK and Studio

```ts
import { HubClient } from '@moul-dev/client';

const hub = new HubClient('https://sync.moul.dev');
await hub.signIn('me@example.com');                 // a code arrives by mail
const { token } = await hub.verify('me@example.com', '123456', { label: 'Laptop' });
await hub.createDatabase(token, 'notes');
await moul.sync.configure({ hub: 'https://sync.moul.dev', database: 'notes', token, label: 'Laptop' });
```

A join link for a hub database carries the hub's address, the database and a
one-time ticket, never a token; the joining device trades the ticket for a
database token of its own.

Studio's Sync page offers both: your own bucket, or a hub.

## From the command line

```sh
moul login https://sync.moul.dev          # asks for your email, then the code
moul pull moul+https://sync.moul.dev/notes ./notes --watch
moul restore moul+https://sync.moul.dev/notes -o notes.db
```

`moul login` keeps the account token in `~/.config/moul/hubs.json`, readable
only by you. `MOUL_HUB_TOKEN` overrides it.
