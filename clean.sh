#!/usr/bin/env bash
# Parley (parley-gpui) — clean script: nuke build artifacts for a fresh build.
#
# Usage:
#   ./clean.sh              # remove build artifacts (default)
#   ./clean.sh --all        # also wipe whisper-rs-sys cargo cache (forces full whisper.cpp recompile next build)
#   ./clean.sh -y / --yes   # skip confirmation
#   ./clean.sh --help
#
# Does NOT touch:
#   - User data (~/.local/share/io.github.hankanman.Parley/  — Whisper/Parakeet models, db, settings)
#   - Your git working tree
#
# After running, do:  ./build.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$ROOT/$(basename "${BASH_SOURCE[0]}")"

MODE=full   # full | all
ASSUME_YES=0

for arg in "$@"; do
    case "$arg" in
        --help|-h)
            sed -n '2,13p' "$SELF" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        --all)   MODE=all ;;
        --yes|-y) ASSUME_YES=1 ;;
        *)
            echo "error: unknown arg '$arg' (try --help)" >&2
            exit 2
            ;;
    esac
done

# ----- collect targets -----
TARGETS=(
    "$ROOT/target"                          # cargo build output (workspace)
)

if [[ "$MODE" == "all" ]]; then
    # Forces fresh whisper.cpp + CUDA compile next build (~10 min).
    # Useful if you've changed CUDA toolkit or want to verify a vendor-free build.
    TARGETS+=(
        "$HOME/.cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/whisper-rs-sys-0.15.0.crate"
        "$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/whisper-rs-sys-0.15.0"
    )
fi

# ----- filter to existing paths and report sizes -----
EXISTING=()
TOTAL_BYTES=0
for path in "${TARGETS[@]}"; do
    if [[ -e "$path" ]]; then
        SIZE_HUMAN=$(du -sh "$path" 2>/dev/null | cut -f1)
        SIZE_BYTES=$(du -sb "$path" 2>/dev/null | cut -f1)
        TOTAL_BYTES=$(( TOTAL_BYTES + ${SIZE_BYTES:-0} ))
        echo "  $path  ($SIZE_HUMAN)"
        EXISTING+=("$path")
    fi
done

if [[ ${#EXISTING[@]} -eq 0 ]]; then
    echo "Nothing to clean."
    exit 0
fi

# Pretty-print total
TOTAL_HUMAN=$(numfmt --to=iec --suffix=B "$TOTAL_BYTES" 2>/dev/null || echo "${TOTAL_BYTES}B")
echo
echo "Total: $TOTAL_HUMAN across ${#EXISTING[@]} path(s)"

# ----- confirm and remove -----
if [[ "$ASSUME_YES" -ne 1 ]]; then
    read -r -p "Proceed? [y/N] " ANSWER
    case "$ANSWER" in
        y|Y|yes|YES) ;;
        *) echo "Aborted."; exit 1 ;;
    esac
fi

for path in "${EXISTING[@]}"; do
    rm -rf "$path"
    echo "  removed: $path"
done

echo
echo "Done. Next step: ./build.sh"
