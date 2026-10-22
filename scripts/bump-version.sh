#!/bin/bash
# Bump Parley's version (semver) in the root Cargo.toml.
#
#   scripts/bump-version.sh patch|minor|major
#   scripts/bump-version.sh 1.2.3
#
# Prints the new version. Commit the change, merge it, then run the Release
# workflow (see RELEASING.md).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CURRENT="$("$ROOT/scripts/version.sh")"
IFS=. read -r MAJOR MINOR PATCH <<<"$CURRENT"

case "${1:-}" in
  patch) NEW="$MAJOR.$MINOR.$((PATCH + 1))" ;;
  minor) NEW="$MAJOR.$((MINOR + 1)).0" ;;
  major) NEW="$((MAJOR + 1)).0.0" ;;
  [0-9]*.[0-9]*.[0-9]*) NEW="$1" ;;
  *) echo "usage: $0 patch|minor|major|X.Y.Z   (current: $CURRENT)" >&2; exit 2 ;;
esac
[[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a semver version: $NEW" >&2; exit 2; }

sed -i "/^\[workspace\.package\]/,/^\[/ s/^version = \"$CURRENT\"/version = \"$NEW\"/" "$ROOT/Cargo.toml"
[ "$("$ROOT/scripts/version.sh")" = "$NEW" ] || { echo "failed to update Cargo.toml" >&2; exit 1; }
echo "$NEW"
