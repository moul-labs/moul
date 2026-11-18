# Releasing

`scripts/release.sh` publishes a release: the crates to crates.io and `@moul-dev/client` and
`@moul-dev/studio` to npm. Run it from a clean checkout of the tag; `--dry-run` checks and packs
without publishing (it needs Rust 1.90 or later, for one `cargo publish` of several crates). Pushing the tag also starts `.github/workflows/release.yml`, which builds the
`moul` command for each platform and makes a draft GitHub release.

## Before publishing

CI covers Chromium, Firefox and WebKit headless. Phones are checked by hand, on real devices, with
the Playground built from the tag (`npm run build -w moul-playground`, then serve `sites/playground/dist`
over HTTPS, or deploy it to a preview address):

**Safari on iOS**, in a tab and again after adding it to the Home Screen:

- [ ] A new database opens; make a collection and a few records; reload and they are still there.
- [ ] Two tabs on the same database: a record made in one appears in the other.
- [ ] Offline (Airplane Mode), the app opens from the Home Screen and records can be made.
- [ ] Download the database file from Studio's header; the "not backed up" notice goes away.
- [ ] Sync to a test bucket, then join from a second device with the QR code; a record made on the
      writer appears on the follower.

**Chrome on Android**: the same list, with "Install app" instead of the Home Screen.

`scripts/release.sh` asks for both before it publishes anything.

## After publishing

- Deploy the Playground: `npm run deploy -w moul-playground` (play.moul.dev).
- Publish the draft GitHub release that the release workflow made.
