#!/bin/sh
# Starts RustFS, an S3-compatible store, at http://127.0.0.1:9000 with the bucket and keys the S3
# and hub tests default to (MOUL_S3_BUCKET, MOUL_S3_ACCESS_KEY_ID, MOUL_S3_SECRET_ACCESS_KEY), and
# allows any origin so a page can sync through it. CI runs the S3 suites against it.
#
# The pinned release is downloaded once and checked against its SHA-256. Everything lives in
# $RUSTFS_DIR: target/rustfs, or the runner's temp directory in CI (out of the Rust cache).
# Stop it with: kill "$(cat target/rustfs/rustfs.pid)"
set -eu
cd "$(dirname "$0")/.."

version=1.0.6
dir=${RUSTFS_DIR:-${RUNNER_TEMP:-target}/rustfs}
endpoint=http://127.0.0.1:9000
bucket=${MOUL_S3_BUCKET:-moul-test}
key=${MOUL_S3_ACCESS_KEY_ID:-moul-test}
secret=${MOUL_S3_SECRET_ACCESS_KEY:-moul-test-secret}

# From the release's SHA256SUMS.
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) asset=linux-x86_64-musl sha=a4d109a09857c04bf5d11b2cec0fbbf0887819b9392e5d46d7d153dae6a1398f ;;
  Linux/aarch64) asset=linux-aarch64-musl sha=dcbe86dbb0c521cb6738f72bb36a2d056866667d3d137c2118ee7e2cc9475855 ;;
  Darwin/arm64) asset=macos-aarch64 sha=e0bf7e8aeee45490034576fde6513377db81844d8c40e24c57269e9bb274882f ;;
  *) echo "RustFS $version has no build for $(uname -sm); run the rustfs/rustfs:$version image" >&2; exit 1 ;;
esac

bin=$dir/rustfs-$version
if [ ! -x "$bin" ]; then
  zip=$dir/rustfs-$asset-v$version.zip
  mkdir -p "$dir"
  curl -fsSL -o "$zip" "https://github.com/rustfs/rustfs/releases/download/$version/rustfs-$asset-v$version.zip"
  echo "$sha  $zip" | shasum -a 256 -c - >/dev/null
  rm -rf "$dir/unzipped"
  unzip -q "$zip" -d "$dir/unzipped"
  mv "$dir/unzipped/rustfs" "$bin"
  chmod +x "$bin"
  rm -rf "$zip" "$dir/unzipped"
fi

if curl -fs "$endpoint/health/ready" >/dev/null 2>&1; then
  echo "RustFS is already running at $endpoint"
else
  mkdir -p "$dir/data"
  RUSTFS_ACCESS_KEY=$key RUSTFS_SECRET_KEY=$secret \
  RUSTFS_ADDRESS=127.0.0.1:9000 RUSTFS_CONSOLE_ENABLE=false \
  RUSTFS_CORS_ALLOWED_ORIGINS='*' \
    nohup "$bin" "$dir/data" >"$dir/rustfs.log" 2>&1 &
  echo $! >"$dir/rustfs.pid"
  i=0
  until curl -fs "$endpoint/health/ready" >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 30 ]; then
      cat "$dir/rustfs.log" >&2
      exit 1
    fi
    sleep 1
  done
  echo "RustFS $version is running at $endpoint (log: $dir/rustfs.log)"
fi

# Create the bucket; 409 means it is already there.
status=$(curl -s -o /dev/null -w '%{http_code}' -X PUT --user "$key:$secret" \
  --aws-sigv4 aws:amz:us-east-1:s3 "$endpoint/$bucket")
case "$status" in
  200 | 409) echo "bucket: $bucket" ;;
  *) echo "creating bucket $bucket failed: HTTP $status" >&2; exit 1 ;;
esac
