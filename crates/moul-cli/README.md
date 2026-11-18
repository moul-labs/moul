# moul-cli

The moul command-line tool: pull, restore, check, serve, hub and deploy.

Part of [Moul](https://github.com/moul-labs/moul), a backend that runs entirely
in a browser tab: a database, access rules, auth, files and realtime, stored as
one SQLite file and synced as a Litestream-compatible LTX replica.

**Status:** early development. `restore`, `pull` and `info` work; the other
commands arrive with later phases.

## Commands

```sh
moul restore <replica> -o notes.db [--txid <n>] [--force]
moul pull <replica> ./notes [--watch] [--every <seconds>]
moul info <replica>
```

- **`restore`** rebuilds the database a replica holds into a plain SQLite
  file and checks its integrity, as `litestream restore` does. `--txid`
  restores an earlier state still in the replica's history.
- **`pull`** keeps a local copy in a folder, following the replica's writer.
  `./notes/main.db` is byte for byte the writer's file and any SQLite tool can
  read it while `moul` is not running. `--watch` keeps following, every 3
  seconds or every `--every` seconds (0.2 to 86400). A folder that
  held other data keeps it in `./notes/conflicts/` before taking the
  replica's.
- **`info`** shows who writes, how far the replica reaches, its files by
  level, and which TXIDs it can restore.

A replica is a folder (a path or `file:///path`, laid out as Litestream's file
replica) or an S3-compatible bucket:

```text
s3://<bucket>/<prefix>?endpoint=https://<account>.r2.cloudflarestorage.com
s3://<bucket>/<prefix>?region=eu-west-1
s3://<bucket>/<prefix>?endpoint=http://localhost:9000
```

Credentials come from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
`AWS_SESSION_TOKEN`; the endpoint and region may come from `AWS_ENDPOINT_URL`
and `AWS_REGION`.

## License

MIT
