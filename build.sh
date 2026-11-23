#!/usr/bin/env bash
# Parley (parley-gpui) — all-in-one build script (Linux focus)
#
# Usage:
#   ./build.sh              # default: vulkan if a GPU and glslc are present, cpu otherwise
#   ./build.sh vulkan       # GPU via Vulkan (NVIDIA, AMD, Intel)
#   ./build.sh cpu          # CPU-only
#   ./build.sh gpui [vulkan|cpu]   # same as above — accepted as a
#                                   # no-op alias for muscle memory
#   ./build.sh --help
#
# Environment overrides (pre-set if you know better):
#   NO_STRIP            keep set to 1 on Fedora 43+ (linuxdeploy SHT_RELR incompatibility)
#
# Produces:
#   target/gpui-dist/               self-contained staged dir (binary + libs)
#   Parley-<ver>-x86_64.AppImage    repo root (version from scripts/version.sh)
#   target/release/parley-mcp      MCP server, also bundled in the AppImage
#                                    (see parley-mcp/README.md)

set -euo pipefail

# ----- repo root anchor -----
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$ROOT/$(basename "${BASH_SOURCE[0]}")"

# ----- shared helpers -----

# Sets platform env needed by any cargo build in this repo on Linux
# (CMake defaults for the native deps, NO_STRIP + LD_LIBRARY_PATH for the
# AppImage bundling step).
setup_linux_build_env() {
    local mode="$1"
    case "$(uname -s)" in
        Linux)
            # CMake defaults for whisper.cpp/llama.cpp: a fixed AVX2 ggml
            # baseline instead of the CI runner's own CPU (issue #155), and no
            # OpenSSL in llama.cpp (fails on OpenSSL 4). See cmake/native-deps.cmake.
            export CMAKE_PROJECT_INCLUDE="${CMAKE_PROJECT_INCLUDE:-$ROOT/cmake/native-deps.cmake}"

            # linuxdeploy's bundled `strip` chokes on SHT_RELR sections in modern Fedora libs.
            # NOTE: this only disables linuxdeploy's own strip pass during AppImage bundling —
            # it does not affect cargo's `strip = "debuginfo"` in the workspace [profile.release]
            # (see root Cargo.toml), which still strips every binary during `cargo build --release`.
            export NO_STRIP="${NO_STRIP:-1}"

            # sherpa-onnx-sys drops `libsherpa-onnx-c-api.so` into target/release/ but
            # leaves the parley binary without a RUNPATH, so linuxdeploy fails with
            # `Could not find dependency: libsherpa-onnx-c-api.so`. Point its
            # dependency-resolver at the cargo output dir so it can bundle the lib.
            export LD_LIBRARY_PATH="$ROOT/target/release${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            ;;
        Darwin)
            # macOS Metal/CoreML auto-enabled by whisper-rs feature flags
            : ;;
        *)
            echo "warning: unsupported uname '$(uname -s)'; proceeding anyway" >&2
            ;;
    esac
}

setup_sccache() {
    if command -v sccache >/dev/null 2>&1; then
        export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
        export CMAKE_C_COMPILER_LAUNCHER="${CMAKE_C_COMPILER_LAUNCHER:-sccache}"
        export CMAKE_CXX_COMPILER_LAUNCHER="${CMAKE_CXX_COMPILER_LAUNCHER:-sccache}"
        echo "==> sccache enabled (cached compiles for Rust + C/C++)"
    fi
}

# Vulkan when there's a GPU render node and glslc (needed to compile ggml's
# Vulkan shaders); CPU otherwise. Vulkan covers NVIDIA too: CUDA builds were
# dropped (docs/benchmarks/2026-11-gpu.md).
resolve_auto_mode() {
    if compgen -G '/dev/dri/renderD*' >/dev/null && command -v glslc >/dev/null 2>&1; then
        echo vulkan
    else
        echo cpu
    fi
}

# Builds llama-helper --release with the GPU feature matching $1 and returns
# the path to the built binary on stdout.
build_llama_helper() {
    local mode="$1"
    local helper_features=()
    case "$mode" in
        vulkan) helper_features=(--features vulkan) ;;
        cpu)    ;;
    esac
    echo "==> Building llama-helper sidecar (${mode})" >&2
    ( cd "$ROOT/llama-helper" && cargo build --release "${helper_features[@]}" )
    echo "$ROOT/target/release/llama-helper"
}

# ----- arg parsing -----

FIRST="${1:-auto}"
case "$FIRST" in
    --help|-h)
        sed -n '2,19p' "$SELF" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
esac

# "gpui" is accepted as a no-op alias for muscle memory from when the GPUI
# shell was opt-in; the GPUI shell is the only app now.
if [[ "$FIRST" == "gpui" ]]; then
    MODE="${2:-auto}"
else
    MODE="$FIRST"
fi

if [[ "$MODE" == "auto" ]]; then
    MODE="$(resolve_auto_mode)"
fi

case "$MODE" in
    vulkan|cpu) ;;
    cuda)
        echo "error: CUDA builds were dropped; use 'vulkan', which runs on NVIDIA GPUs too" >&2
        exit 2
        ;;
    *)
        echo "error: unknown mode '$MODE' (expected: vulkan, cpu, auto)" >&2
        exit 2
        ;;
esac

echo "==> Build mode: $MODE"

setup_sccache
cd "$ROOT"
setup_linux_build_env "$MODE"

build_llama_helper "$MODE" >/dev/null

GPUI_FEATURES=()
case "$MODE" in
    vulkan) GPUI_FEATURES=(--features vulkan) ;;
    cpu)    ;;
esac

echo "==> Building parley-gpui (${MODE}, release)"
cargo build --release -p parley-gpui "${GPUI_FEATURES[@]}"

GPUI_BIN="$ROOT/target/release/parley"
if [[ ! -x "$GPUI_BIN" ]]; then
    echo "error: $GPUI_BIN not found after build" >&2
    exit 1
fi

# ----- parley-mcp -----
# A standalone MCP server that an external client (Claude Desktop / Claude
# Code) spawns by absolute path; it reads the SQLite database directly
# rather than talking to the app. Packaged next to `parley` in the AppImage,
# from where install.sh or Settings → Integrations copies it to ~/.local/bin
# (a path into the AppImage mount changes every launch). It has no GPU
# backend, hence no feature flags.
echo "==> Building parley-mcp (MCP server)"
cargo build --release -p parley-mcp
MCP_BIN="$ROOT/target/release/parley-mcp"

# ----- stage a self-contained dist dir -----
DIST_DIR="$ROOT/target/gpui-dist"
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

cp "$GPUI_BIN" "$DIST_DIR/parley"

# Dynamically-linked sherpa-onnx / onnxruntime libs, dropped into
# target/release/ by sherpa-onnx-sys's build script (see parley-gpui's
# $ORIGIN rpath in build.rs, which expects them next to the binary).
shopt -s nullglob
SO_FILES=("$ROOT"/target/release/*.so*)
shopt -u nullglob
if (( ${#SO_FILES[@]} == 0 )); then
    echo "error: no .so files found in target/release (expected libsherpa-onnx-c-api.so / libonnxruntime.so)" >&2
    exit 1
fi
cp -P "${SO_FILES[@]}" "$DIST_DIR/"

# llama-helper sidecar: parley-core's resolver fuzzy-matches any file
# starting with "llama-helper" next to the executable, so the plain name
# works both here and once packaged into the AppImage.
cp "$ROOT/target/release/llama-helper" "$DIST_DIR/llama-helper"
cp "$MCP_BIN" "$DIST_DIR/parley-mcp"

echo "==> Staged: $DIST_DIR"
du -sh "$DIST_DIR"/* | sed 's/^/    /'

# ----- package as an AppImage -----
GPUI_VERSION=$("$ROOT/scripts/version.sh")
echo "==> Packaging AppImage (version $GPUI_VERSION)"
APPIMAGE=$("$ROOT/parley-gpui/packaging/linux/build-appimage.sh" "$DIST_DIR" "$GPUI_VERSION" "$ROOT")

echo
echo "==> Build succeeded"
echo "    Dist dir: $DIST_DIR"
echo "    AppImage: $APPIMAGE ($(du -h "$APPIMAGE" | cut -f1))"
echo "    MCP server: $MCP_BIN ($(du -h "$MCP_BIN" | cut -f1)), also bundled in the AppImage"
echo "                register it with your MCP client — see parley-mcp/README.md"
