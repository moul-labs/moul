# moul-hub

Moul Hub: runs many Moul apps side by side, each in its own database, opened
on demand. Its first part, in this release, is sync for a builder's devices.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** 0.1. The sync hub works; hosting apps comes later.

## Sync for a builder's devices

`moul hub` fronts a folder or a bucket of replicas. Each device signs in with
its owner's email instead of holding a bucket key, followers hear about new
changes the moment they land, and the hub compacts each replica on its own
clock. The replicas are the ones Litestream writes, so `litestream restore`
and `moul restore` read them.

```sh
moul hub ./hub-data --listen 0.0.0.0:8091 --public-url https://sync.example.com --signup invite
```

| Part | Job |
| --- | --- |
| `directory` | Accounts (an auth collection, signed in by email code), devices with hashed tokens, databases, join tickets and invites, in a Moul database of the hub's own |
| `store` | Where replicas live: one prefix per database per account, in a folder or a bucket |
| `api` | The HTTP API: sign-in, devices, databases, the replica calls with conditional writes, push events, compaction |

The protocol is in [`docs/hub.md`](https://github.com/moul-labs/moul/blob/main/docs/hub.md).

## License

MIT
