#!/usr/bin/env bash
# Packages a staged parley-gpui dist dir into a Parley-<version>-x86_64.AppImage
# using linuxdeploy. Called from the root build.sh — not meant to be run
# standalone, but it validates its inputs so it fails loudly if it is.
#
# Args:
#   $1  DIST_DIR   staged dir: parley-gpui binary + .so files + llama-helper
#   $2  VERSION    version string for the output filename
#   $3  OUT_DIR    directory the final .AppImage is written into
#
# Env:
#   NO_STRIP=1     passed through to linuxdeploy (Fedora 43+ SHT_RELR
#                  workaround — see docs/building_in_linux.md)
#
# Tool caching: linuxdeploy is downloaded once into
# ~/.cache/parley-appimage-tools/ and reused on subsequent builds.

set -euo pipefail

DIST_DIR="${1:?usage: build-appimage.sh <dist-dir> <version> <out-dir>}"
VERSION="${2:?usage: build-appimage.sh <dist-dir> <version> <out-dir>}"
OUT_DIR="${3:?usage: build-appimage.sh <dist-dir> <version> <out-dir>}"

PACKAGING_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$PACKAGING_DIR/../../.." && pwd)"

if [[ ! -x "$DIST_DIR/parley" ]]; then
    echo "error: $DIST_DIR/parley not found or not executable" >&2
    exit 1
fi

# ----- fetch linuxdeploy (cached) -----
TOOLS_CACHE_DIR="${PARLEY_APPIMAGE_TOOLS_DIR:-$HOME/.cache/parley-appimage-tools}"
mkdir -p "$TOOLS_CACHE_DIR"
LINUXDEPLOY="$TOOLS_CACHE_DIR/linuxdeploy-x86_64.AppImage"
if [[ ! -x "$LINUXDEPLOY" ]]; then
    echo "==> Downloading linuxdeploy into $TOOLS_CACHE_DIR" >&2
    curl -fL -o "$LINUXDEPLOY" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
    chmod +x "$LINUXDEPLOY"
fi

# ----- assemble AppDir -----
APPDIR="$ROOT/target/gpui-appdir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/512x512/apps"

cp "$DIST_DIR"/* "$APPDIR/usr/bin/"
chmod +x "$APPDIR/usr/bin/parley" "$APPDIR/usr/bin/llama-helper" 2>/dev/null || true

DESKTOP_FILE="$APPDIR/usr/share/applications/io.github.hankanman.Parley.desktop"
cp "$PACKAGING_DIR/parley.desktop" "$DESKTOP_FILE"

ICON_SRC="$PACKAGING_DIR/icon.png"
if [[ ! -f "$ICON_SRC" ]]; then
    echo "error: icon not found at $ICON_SRC" >&2
    exit 1
fi
cp "$ICON_SRC" "$APPDIR/usr/share/icons/hicolor/512x512/apps/io.github.hankanman.Parley.png"

# ----- run linuxdeploy -----
# NO_STRIP (set by build.sh on Linux) works around Fedora 43+'s bundled
# `strip` choking on SHT_RELR sections — see root Cargo.toml comment and
# docs/building_in_linux.md. Cargo's own `strip = true` release profile
# already stripped the binaries, so this only affects linuxdeploy's pass.
export NO_STRIP="${NO_STRIP:-1}"

# rpath on parley-gpui is $ORIGIN (see parley-gpui/build.rs), and the
# sherpa-onnx / onnxruntime .so files are already staged next to it in
# usr/bin/, so linuxdeploy's ldd-based dependency scan resolves them without
# needing LD_LIBRARY_PATH — but set it anyway as a safety net.
export LD_LIBRARY_PATH="$DIST_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# linuxdeploy is chatty on stdout (mksquashfs progress, etc.) — this script's
# own stdout is captured by build.sh as `APPIMAGE=$(...)` and must contain
# only the final path echoed below, so redirect linuxdeploy's stdout to
# stderr here.
# CUDA builds: never bundle the NVIDIA *driver* libraries (libcuda.so.1,
# libnvidia-*) — they must match the host's kernel driver, so a bundled copy
# breaks on any machine with a different driver version. The CUDA *runtime*
# libraries (libcudart, libcublas, libcublasLt) are redistributable and are
# still bundled so the AppImage runs without the CUDA toolkit installed.
mkdir -p "$OUT_DIR"
(
    cd "$OUT_DIR"
    "$LINUXDEPLOY" \
        --appdir "$APPDIR" \
        --executable "$APPDIR/usr/bin/parley" \
        --desktop-file "$DESKTOP_FILE" \
        --icon-file "$APPDIR/usr/share/icons/hicolor/512x512/apps/io.github.hankanman.Parley.png" \
        --exclude-library 'libcuda.so*' \
        --exclude-library 'libnvidia-*' \
        --output appimage 1>&2
)

# linuxdeploy names the output from the .desktop file's basename + arch;
# normalize to Parley-<version>-x86_64.AppImage.
RAW_APPIMAGE=$(find "$OUT_DIR" -maxdepth 1 -name '*.AppImage' -printf '%T@ %p\n' | sort -nr | head -1 | cut -d' ' -f2-)
if [[ -z "$RAW_APPIMAGE" ]]; then
    echo "error: linuxdeploy did not produce an AppImage" >&2
    exit 1
fi
FINAL_APPIMAGE="$OUT_DIR/Parley-${VERSION}-x86_64.AppImage"
if [[ "$RAW_APPIMAGE" != "$FINAL_APPIMAGE" ]]; then
    mv "$RAW_APPIMAGE" "$FINAL_APPIMAGE"
fi

echo "$FINAL_APPIMAGE"
