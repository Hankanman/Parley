#!/bin/bash
# Build the website the way .github/workflows/pages.yml does (site/, the
# screenshots in docs/images/ and the mdBook user guide in docs/user/) into
# target/site, and optionally serve it.
#
#   scripts/docs.sh          build only
#   scripts/docs.sh serve    build, then serve on http://localhost:8000/docs/
#
# Needs mdbook 0.5 on PATH (https://github.com/rust-lang/mdBook/releases).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/site"

command -v mdbook >/dev/null || { echo "mdbook not found on PATH" >&2; exit 1; }
rm -rf "$OUT"
mkdir -p "$OUT"
cp -r "$ROOT/site/." "$OUT/"
cp -r "$ROOT/docs/images" "$OUT/images"
mdbook build "$ROOT/docs/user" --dest-dir "$OUT/docs"
echo "Built $OUT"

if [ "${1:-}" = serve ]; then
  echo "Serving http://localhost:8000/docs/"
  python3 -m http.server 8000 --directory "$OUT"
fi
