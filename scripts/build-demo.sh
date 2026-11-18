#!/bin/sh
# Builds the demo page: the engine for the browser, with its JavaScript glue,
# into demo/pkg. Serve the demo folder with any static server, for example
#   python3 -m http.server 8787 --directory demo
set -eu
cd "$(dirname "$0")/.."
cargo build -p moul-web --target wasm32-unknown-unknown --release
rm -rf demo/pkg
wasm-bindgen --target web --no-typescript --out-dir demo/pkg \
  target/wasm32-unknown-unknown/release/moul_web.wasm
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
    -o demo/pkg/moul_web_bg.wasm demo/pkg/moul_web_bg.wasm
fi
size=$(wc -c < demo/pkg/moul_web_bg.wasm | tr -d ' ')
gz=$(gzip -9 -c demo/pkg/moul_web_bg.wasm | wc -c | tr -d ' ')
echo "demo/pkg/moul_web_bg.wasm: $size bytes, $gz gzipped"
