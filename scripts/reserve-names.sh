#!/usr/bin/env bash
# Publish every workspace crate at the workspace version, skipping any version
# already on crates.io. Safe to re-run.
#
# crates.io lets an account publish 5 new crates at once, then 1 every
# 10 minutes, so when it is rate limited this waits and tries again.
#
# Before running: `cargo login` with a crates.io token, a verified email on
# the crates.io account, and a clean git tree (cargo refuses to publish
# uncommitted files).
set -euo pipefail
cd "$(dirname "$0")/.."

# Most important names first, in case the run is interrupted.
CRATES=(
  moul moul-core moul-cli moul-ltx moul-vfs moul-sync
  moul-store moul-rules moul-web moul-server moul-hub
)
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
UA="moul-release-script (https://github.com/moul-labs/moul)"
WAIT_SECONDS=600
LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

published() {
  curl -fsS -o /dev/null -A "$UA" "https://crates.io/api/v1/crates/$1/$VERSION" 2>/dev/null
}

for crate in "${CRATES[@]}"; do
  if published "$crate"; then
    echo "skip    $crate@$VERSION is already on crates.io"
    continue
  fi
  until cargo publish -p "$crate" 2>&1 | tee "$LOG"; do
    if grep -q "too many new crates" "$LOG"; then
      echo "rate limited; retrying $crate in $((WAIT_SECONDS / 60)) minutes"
      sleep "$WAIT_SECONDS"
    else
      echo "failed  $crate@$VERSION" >&2
      exit 1
    fi
  done
  echo "done    $crate@$VERSION"
done

echo "All ${#CRATES[@]} crates are on crates.io at $VERSION."
