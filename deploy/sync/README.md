# The hosted sync beta

`sync.moul.dev` is `moul hub` on one machine: the directory (accounts,
devices, databases) on a volume, the replicas in a Cloudflare R2 bucket, mail
through Resend. These files build and run it; deploying needs the Moul team's
Fly.io, Cloudflare and Resend accounts.

| File | |
| --- | --- |
| `Dockerfile` | The `moul` binary in a small Debian image, running `moul hub /data` |
| `fly.toml` | One Fly.io machine with a volume, HTTPS, and a health check on `/api/health` |

## First deploy

1. Make an R2 bucket (`moul-sync`) and an API token scoped to it; put its
   account id in `MOUL_STORE` in `fly.toml`.
2. Verify the sending domain in Resend.
3. Run the commands at the top of `fly.toml`, then point `sync.moul.dev` at
   the app (`fly certs add sync.moul.dev`).
4. Make invite codes:

   ```sh
   curl -X POST https://sync.moul.dev/api/hub/admin/invites \
     -H "X-Admin-Key: $MOUL_ADMIN_KEY" -H 'Content-Type: application/json' -d '{"uses": 1}'
   ```

## Before opening it

- [ ] Sign in from Studio (Sync → Moul Hub) with an invite, make a database,
      turn on sync; join from a phone with the QR code.
- [ ] `moul login https://sync.moul.dev`, then `moul pull` the database.
- [ ] `litestream restore` the replica straight from R2.
- [ ] Restart the machine; devices carry on without signing in again.
- [ ] Back up the volume (`fly volumes snapshots`); the directory is not
      replicated yet.
