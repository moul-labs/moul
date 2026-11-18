# A replica Litestream wrote

`replica/` is a file replica that Litestream 0.5.13 wrote while a small script
ran against a database with 4,096-byte pages: level-0 files, one level-1
compaction and a level-9 snapshot. `expected.db` is what `litestream restore`
rebuilt from it.

`moul-ltx` restores it in its own tests (`crates/moul-ltx/tests/litestream_fixture.rs`),
so files from Litestream's LZ4 encoder are checked even where Go is not
installed. The compatibility suite checks it against Litestream itself.

Do not edit these files. To write them again, with Go installed:

```sh
cargo test -p litestream-compat --test replica -- --ignored regenerate_fixture
```
