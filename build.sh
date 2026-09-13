#!/usr/bin/env bash
# Parley (parley-gpui) — all-in-one build script (Linux focus)
#
# Usage:
#   ./build.sh              # default: cuda on Linux with NVIDIA, cpu otherwise
#   ./build.sh cuda         # NVIDIA CUDA
#   ./build.sh vulkan       # AMD/Intel Vulkan
#   ./build.sh cpu          # CPU-only
#   ./build.sh gpui [cuda|vulkan|cpu]   # same as above — accepted as a
#                                        # no-op alias for muscle memory
#   ./build.sh --help
#
# Environment overrides (pre-set if you know better):
#   CUDAHOSTCXX         host C++ compiler nvcc should use (default: auto-detect g++-15 on Fedora)
#   CUDAARCHS           CUDA arch list (default: "75;80;86;89;90" — Turing→Hopper)
#   NO_STRIP            keep set to 1 on Fedora 43+ (linuxdeploy SHT_RELR incompatibility)
#
# Produces:
#   target/gpui-dist/               self-contained staged dir (binary + libs)
#   Parley-<ver>-x86_64.AppImage    repo root (version from parley-gpui/Cargo.toml)
#   target/release/parley-mcp      MCP server, left for the user to register
#                                    (not bundled into the AppImage — see
#                                    parley-mcp/README.md)

set -euo pipefail

# ----- repo root anchor -----
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$ROOT/$(basename "${BASH_SOURCE[0]}")"

# ----- shared helpers -----

# Sets platform env needed by any cargo build in this repo on Linux
# (CUDAHOSTCXX/CUDAARCHS for cuda mode, PIC for the CUDA linker issue,
# NO_STRIP + LD_LIBRARY_PATH for the AppImage bundling step). No-op for
# non-Linux or non-cuda modes where not applicable.
setup_linux_build_env() {
    local mode="$1"
    case "$(uname -s)" in
        Linux)
            # Fedora 44 ships gcc 16; CUDA 13.2's nvcc only supports gcc ≤ 15.
            # Auto-detect g++-15 if not overridden.
            if [[ "$mode" == "cuda" && -z "${CUDAHOSTCXX:-}" ]]; then
                if [[ -x /usr/bin/g++-15 ]]; then
                    export CUDAHOSTCXX=/usr/bin/g++-15
                    echo "==> CUDAHOSTCXX=/usr/bin/g++-15 (Fedora gcc-16 workaround)"
                elif [[ -x /usr/bin/g++-14 ]]; then
                    export CUDAHOSTCXX=/usr/bin/g++-14
                    echo "==> CUDAHOSTCXX=/usr/bin/g++-14"
                fi
            fi

            # CUDA 13 dropped sm_52 (Maxwell). Pin to Turing+ unless overridden.
            if [[ "$mode" == "cuda" && -z "${CUDAARCHS:-}" ]]; then
                export CUDAARCHS="75;80;86;89;90"
                echo "==> CUDAARCHS=$CUDAARCHS"
            fi

            # rust-lld on modern Rust requires every input to be PIE-relocatable.
            # llama-cpp-sys-2's CUDA .cu.o files default to non-PIC, which triggers
            # `R_X86_64_32 cannot be used against local symbol; recompile with -fPIC`
            # when linking the llama-helper binary.
            export CMAKE_POSITION_INDEPENDENT_CODE="${CMAKE_POSITION_INDEPENDENT_CODE:-ON}"

            # linuxdeploy's bundled `strip` chokes on SHT_RELR sections in modern Fedora libs.
            # NOTE: this only disables linuxdeploy's own strip pass during AppImage bundling —
            # it does not affect cargo's `strip = true` in the workspace [profile.release]
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
        export CMAKE_CUDA_COMPILER_LAUNCHER="${CMAKE_CUDA_COMPILER_LAUNCHER:-sccache}"
        echo "==> sccache enabled (cached compiles for Rust + C/C++ + CUDA)"
    fi
}

resolve_auto_mode() {
    if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
        echo cuda
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
        cuda)   helper_features=(--features cuda) ;;
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
        sed -n '2,23p' "$SELF" | sed 's/^# \{0,1\}//'
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
    cuda|vulkan|cpu) ;;
    *)
        echo "error: unknown mode '$MODE' (expected: cuda, vulkan, cpu, auto)" >&2
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
    cuda)   GPUI_FEATURES=(--features cuda) ;;
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
# Not bundled into the app: it's a standalone MCP server that an external
# client (Claude Desktop / Claude Code) spawns by absolute path, and it talks
# to the SQLite database directly rather than to the app. So it's built and
# left in target/release for the user to register. It has no GPU backend,
# hence no feature flags.
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

echo "==> Staged: $DIST_DIR"
du -sh "$DIST_DIR"/* | sed 's/^/    /'

# ----- package as an AppImage -----
GPUI_VERSION=$(awk -F'"' '/^version/ {print $2; exit}' "$ROOT/parley-gpui/Cargo.toml")
echo "==> Packaging AppImage (version $GPUI_VERSION)"
APPIMAGE=$("$ROOT/parley-gpui/packaging/linux/build-appimage.sh" "$DIST_DIR" "$GPUI_VERSION" "$ROOT")

echo
echo "==> Build succeeded"
echo "    Dist dir: $DIST_DIR"
echo "    AppImage: $APPIMAGE ($(du -h "$APPIMAGE" | cut -f1))"
echo "    MCP server: $MCP_BIN ($(du -h "$MCP_BIN" | cut -f1))"
echo "                register it with your MCP client — see parley-mcp/README.md"
