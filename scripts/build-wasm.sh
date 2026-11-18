#!/bin/sh
# Builds the engine for the browser (and Node) into packages/client/wasm:
# moul_web.js, its types and moul_web_bg.wasm. The SDK's worker and its
# headless engine load it from there.
set -eu
cd "$(dirname "$0")/.."
out=packages/client/wasm
cargo build -p moul-web --target wasm32-unknown-unknown --release
rm -rf "$out"
wasm-bindgen --target web --out-dir "$out" \
  target/wasm32-unknown-unknown/release/moul_web.wasm
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
    -o "$out/moul_web_bg.wasm" "$out/moul_web_bg.wasm"
fi
size=$(wc -c < "$out/moul_web_bg.wasm" | tr -d ' ')
gz=$(gzip -9 -c "$out/moul_web_bg.wasm" | wc -c | tr -d ' ')
echo "$out/moul_web_bg.wasm: $size bytes, $gz gzipped"
