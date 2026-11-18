#!/usr/bin/env bash
# Record fixtures from the Go engine's test suite into fixtures/go.
#
#   scripts/export-go-fixtures.sh [path to moul-dev checkout] [extra go-fixtures flags]
#
# The Go checkout must be clean; the commit it is at is written to
# fixtures/go/MANIFEST.json. See fixtures/go/README.md.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
go_repo="$(cd "${1:-$root/../../moul-dev/moul-dev}" && pwd)"
shift || true
cd "$root/tools/go-fixtures"
exec go run . -go-repo "$go_repo" -out "$root/fixtures/go" "$@"
