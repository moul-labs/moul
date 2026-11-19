# Sync

Sync keeps a database on your other devices through a bucket you own. Every commit becomes an LTX
file, the replica format Litestream writes, so the bucket is also a backup that `moul restore` or
`litestream restore` rebuilds without Moul.


Rather not set up a bucket? [Moul Hub](hub.md) keeps the replica for you:
each device signs in with your email, followers hear of changes as they land,
and the hub compacts the replica. The rest of this page is about your own
bucket.

## Set up a bucket

Any S3-compatible store with conditional writes works: Cloudflare R2, Amazon S3, MinIO.

1. **Make a bucket.**
2. **Make a key for it.** It should read and write only this bucket: the key is kept in the
   browser, and a script injected into your app could read it.
3. **Allow pages to reach the bucket** with a CORS rule. Narrow `AllowedOrigins` to your app's
   address (and `https://play.moul.dev` if you use the Playground):

   ```json
   [
     {
       "AllowedOrigins": ["https://your-app.example"],
       "AllowedMethods": ["GET", "PUT", "HEAD", "DELETE"],
       "AllowedHeaders": ["*"],
       "ExposeHeaders": ["ETag"],
       "MaxAgeSeconds": 3600
     }
   ]
   ```

4. **Connect** from Studio's Sync page, or from code:

   ```ts
   await moul.sync.configure({
     endpoint: 'https://<account>.r2.cloudflarestorage.com',
     bucket: 'notes',
     accessKeyId: '…',
     secretAccessKey: '…',
     label: 'Laptop',
   });
   ```

   For R2 leave `region` empty; for S3 give the bucket's region. Path-style addresses are on by
   default, which R2 and MinIO need. `prefix` keeps several databases in one bucket.

## Content security policy

The bucket key is kept in the browser, so a script injected into your app could read it and send
it anywhere it can reach. A content security policy narrows where that is: list your bucket's
address in `connect-src` and nothing else that is not yours. The engine needs:

| Directive | Why |
| --- | --- |
| `script-src 'self' 'wasm-unsafe-eval'` | The engine is WebAssembly, compiled in the page's worker |
| `worker-src 'self'` | Each tab starts the engine's module worker from the SDK's own files |
| `connect-src 'self' https://<your bucket's address>` | The worker fetches the engine's `.wasm` from your site, and sync talks to the bucket |
| `img-src 'self' blob:` and `media-src 'self' blob:` | Files are shown through `blob:` URLs (Studio also needs `data:` in `img-src`, for icons its styles draw) |

For example, for an app syncing to R2:

```text
Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; connect-src 'self' https://<account>.r2.cloudflarestorage.com; img-src 'self' blob: data:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'
```

`style-src 'unsafe-inline'` and `img-src data:` are there for Studio, whose components set inline
styles and draw some icons from `data:` URLs. The Playground
lets builders pick any bucket, so its policy allows `connect-src https:`; see its
[`_headers`](../sites/playground/public/_headers).

## Devices

- **One device writes at a time.** It pushes about once a second while the page is open. The
  others follow: they check the bucket every few seconds, apply what changed, and their live
  queries refresh. A follower is read-only.
- **Add a device** from Studio's Sync page: it makes a link and a QR code that carry the settings in
  the URL fragment, which browsers never send to a server. The link holds the key, so share it
  only with your own devices. Opening it in the Playground starts a new database that follows the
  bucket. A device joining from a link only follows: Studio asks before connecting, names the
  bucket, and if the device has data of its own it keeps it as a separate copy rather than
  uploading it anywhere.
- **Take over writing** on a caught-up follower; the previous writer becomes a follower on its next
  push. Two devices cannot both write one transaction: the bucket's conditional upload stops the
  second.
- **Turning sync off** keeps the data and the changes made from then on, so they go up when the
  device connects to the same bucket again. **Leaving the bucket** (`disconnect({ forget: true })`,
  or Studio's "Leave this bucket") stops keeping them; connecting again starts over.
- **Conflicts are kept, not merged.** If a device had changes the bucket does not have when it lost
  the writer role, they are saved as a separate copy of the database. Studio lists them to download
  or delete.

## Limits

- No sync from a closed browser: changes go out the next time the app is open, and Studio shows the
  last sync time.
- Moul does not encrypt the replica: whoever can read the bucket can read the data.
- Sign-ins, keys and codes are not synced: they stay on the device that wrote them.
- Followers apply pages, not requests; editing on several devices at once arrives with Moul Hub.

## Restore without a browser

```sh
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=…
moul restore 's3://notes?endpoint=https://<account>.r2.cloudflarestorage.com' -o notes.db
moul pull 's3://notes?endpoint=https://<account>.r2.cloudflarestorage.com' ./notes --watch
```

See the [`moul` command](../crates/moul-cli/README.md) for the replica URL forms and credentials.
