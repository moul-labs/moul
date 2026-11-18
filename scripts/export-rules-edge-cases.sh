#!/usr/bin/env bash
# Record the Go rule edge cases into crates/moul-rules/tests/data.
#
#   scripts/export-rules-edge-cases.sh [path to moul-dev checkout] [--out DIR] [--allow-dirty]
#
# The cases are in tools/rules-edge-cases/cases.py; the Go engine answers them
# through a build overlay, so the checkout is never modified and must be clean.
# Needs go and python3. See crates/moul-rules/README.md.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
go_repo="$(cd "${1:-$root/../../moul-dev/moul-dev}" && pwd)"
shift || true
exec python3 "$root/tools/rules-edge-cases/export.py" --go-repo "$go_repo" --out "$root/crates/moul-rules/tests/data" "$@"
