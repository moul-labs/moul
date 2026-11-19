#!/bin/sh
# Publishes a release: the crates to crates.io and the packages to npm, after
# checking that everything builds and passes. Needs `cargo login` and
# `npm login` to accounts that own the names. Run from a clean checkout of the
# tag:
#
#   git checkout v0.1.0
#   scripts/release.sh            # checks, then asks before each publish
#   scripts/release.sh --dry-run  # checks and packs, publishes nothing
#
# Before publishing it asks whether the release was checked on real devices:
# Safari on iOS (in a tab and added to the Home Screen) and Chrome on Android,
# following RELEASING.md.
#
# The Playground is deployed separately: npm run deploy -w moul-playground.
set -eu
cd "$(dirname "$0")/.."

dry=""
[ "${1:-}" = "--dry-run" ] && dry="--dry-run"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
for p in packages/client packages/studio; do
  v=$(node -p "require('./$p/package.json').version")
  [ "$v" = "$version" ] || { echo "$p is $v, the crates are $version"; exit 1; }
done
[ -z "$(git status --porcelain)" ] || { echo "the working tree is not clean"; exit 1; }

echo "== checks for $version"
cargo test --workspace --all-targets
scripts/build-wasm.sh
npm ci
npm run typecheck
npm test
npm run build

ask() {
  [ -n "$dry" ] && return 0
  printf '%s [y/N] ' "$1"
  read -r answer
  [ "$answer" = "y" ]
}

if [ -z "$dry" ]; then
  echo "== real devices (RELEASING.md)"
  ask "checked on Safari on iOS, in a tab and from the Home Screen?" || { echo "check it on iOS first"; exit 1; }
  ask "checked on Chrome on Android?" || { echo "check it on Android first"; exit 1; }
fi

# One call for every crate: cargo publishes them in dependency order and waits
# for each to reach the index before the next, and a dry run packs and builds
# each against the others' new versions.
crates="moul-ltx moul-vfs moul-store moul-rules moul-core moul-sync moul-web moul-server moul-hub moul moul-cli"
if ask "publish $crates at $version to crates.io?"; then
  # shellcheck disable=SC2086 # one -p per crate
  cargo publish $(printf -- '-p %s ' $crates) $dry
fi

for pkg in @moul-dev/client @moul-dev/studio; do
  if ask "publish $pkg $version to npm?"; then
    npm publish -w "$pkg" --access public $dry
  fi
done
echo "done"
