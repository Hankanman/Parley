#!/bin/bash
# Print Parley's version: [workspace.package] version in the root Cargo.toml.
# Used by build.sh and the GitHub workflows so there is one source of truth.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
awk '/^\[workspace\.package\]/ {in_pkg = 1; next}
     /^\[/                      {in_pkg = 0}
     in_pkg && /^version *=/    {gsub(/"/, "", $3); print $3; exit}' "$ROOT/Cargo.toml"
