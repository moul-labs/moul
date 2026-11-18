# Demo: SQL console with sync

A static page that runs the engine and SQLite in a worker, keeps the database
in the browser's private storage (OPFS), and syncs it to a bucket you own as a
Litestream-compatible replica. Nothing is sent anywhere until you connect a
bucket.

## Build and run

```sh
cargo install wasm-bindgen-cli --version 0.2.130 --locked   # once
scripts/build-demo.sh
python3 -m http.server 8787 --directory demo
```

Open <http://localhost:8787>. `?db=<name>` opens a separate database, which
behaves like a separate device; **Open another device** does that in a new tab
with the same sync settings, so you can watch one follow the other and hand
writing back and forth.

The page is static: deploy the `demo` folder (with `demo/pkg`) to any static
host. It must be served over HTTPS (or from `localhost`) for OPFS and workers.

## A bucket for it

Any S3-compatible store with conditional writes works. Create a bucket, a key
that can read and write only that bucket, and a CORS rule that lets the page
call it:

```json
[
  {
    "AllowedOrigins": ["https://demo.example.com", "http://localhost:8787"],
    "AllowedMethods": ["GET", "PUT", "DELETE"],
    "AllowedHeaders": ["authorization", "x-amz-date", "x-amz-content-sha256", "if-match", "if-none-match"],
    "ExposeHeaders": ["ETag"],
    "MaxAgeSeconds": 3600
  }
]
```

- **Cloudflare R2:** endpoint `https://<account>.r2.cloudflarestorage.com`,
  region `auto`. Set the rule under the bucket's Settings → CORS policy, and
  make an API token with Object Read & Write on that bucket only.
- **Amazon S3:** endpoint `https://s3.<region>.amazonaws.com` and the region.
  Set the rule under Permissions → CORS.
- **MinIO:** allows any origin by default.

The keys stay in this browser's device store (`device.db`), which is never
replicated, and are sent only to the bucket. A script injected into the page
could read them, which is why the key should reach one bucket and nothing else.

## What to try

1. Create a table and insert rows, then connect a bucket: this device becomes
   the writer and pushes TXID 1, the whole database.
2. Open another device: it joins from the replica and follows. Writes there
   are refused until it takes over.
3. **Take over writing** on the second device and change something. The first
   device steps down on its next check and picks the change up.
4. To see a conflict copy, give a device data of its own before connecting it
   to an existing replica: it keeps its database under **Conflict copies**
   before taking the replica's.
5. Restore the replica anywhere with `moul restore` or `litestream restore`.
