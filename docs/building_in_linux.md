## 🐧 Building on Linux

Parley is Linux-only. This guide covers building from source using the
root-level `build.sh` / `dev.sh` / `clean.sh` scripts, which handle GPU-mode
selection, the gnarly Fedora/CUDA build-environment quirks, and the
`llama-helper` sidecar build for you.

---

## 🚀 Quick Start

### 1. Install Dependencies

```bash
# Ubuntu/Debian
sudo apt update
sudo apt install build-essential cmake git \
  patchelf libasound2-dev libopenblas-dev libx11-dev libxtst-dev libxrandr-dev \
  libpipewire-0.3-dev libclang-dev meson ninja-build

# Fedora/RHEL
sudo dnf install gcc-c++ cmake git llvm openmp-devel \
  patchelf alsa-lib-devel openblas-devel pipewire-devel clang-devel meson ninja-build

# Arch Linux
sudo pacman -S base-devel cmake git \
  patchelf alsa-lib openblas pipewire clang meson ninja
```

`libpipewire` is the audio capture backend (PipeWire ≥ 0.3.44 at runtime),
`libclang` is needed by its Rust bindings, and `meson` + `ninja` build the
bundled WebRTC echo-cancellation library (the app no longer needs the system
`webrtc-audio-processing-2` package).

### Building offline or behind a proxy

Two things are fetched at build time: whisper.cpp sources (via
`whisper-rs-sys`) and a prebuilt `sherpa-onnx` archive (via `sherpa-onnx-sys`).
Cargo's registry proxy settings cover the first; the second can be pre-seeded:

```bash
# sherpa-onnx: download the archive the sys crate expects and point it there
mkdir -p ~/.cache/sherpa-onnx
curl -L -o ~/.cache/sherpa-onnx/sherpa-onnx-v1.13.7-linux-x64-shared-lib.tar.bz2 \
  https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.7/sherpa-onnx-v1.13.7-linux-x64-shared-lib.tar.bz2
export SHERPA_ONNX_ARCHIVE_DIR=~/.cache/sherpa-onnx
```

(`ffmpeg` is fetched lazily at *runtime* by `parley-core`'s
`audio/ffmpeg.rs`, not at build time — nothing to pre-seed for a build.)

The exact sherpa-onnx version is the one pinned in `Cargo.lock`
(`sherpa-onnx-sys`); once built, the archive is cached under
`target/sherpa-onnx-prebuilt/`.

You'll also need a Rust toolchain (`rustup`).

### 2. Build and Run

```bash
# From the repo root
./dev.sh              # development mode, hot reload
./build.sh             # production build → AppImage
```

**That's it.** Both scripts auto-detect NVIDIA GPUs (via `nvidia-smi`) and
build with CUDA; everything else falls back to CPU. Pass a mode explicitly to
override:

```bash
./dev.sh cuda          # NVIDIA CUDA
./dev.sh vulkan        # AMD/Intel Vulkan
./dev.sh cpu           # CPU-only

./build.sh cuda
./build.sh vulkan
./build.sh cpu
```

Run `./build.sh --help` or `./dev.sh --help` for the full usage notes
(environment variable overrides, etc.) straight from the script.

---

## 🧠 How It Works

`build.sh` and `dev.sh` are self-contained — no separate GPU-detection script
is involved:

1. **Mode resolution**: `auto` (default) checks for a working `nvidia-smi` →
   `cuda`, else `cpu`. Pass `cuda` / `vulkan` / `cpu` to force a mode.
2. **Platform env setup** (Linux): auto-detects a compatible `g++` for `nvcc`
   on Fedora (CUDA 13 needs gcc ≤ 15), sets `CUDAARCHS`, enables
   `CMAKE_POSITION_INDEPENDENT_CODE` (required for `rust-lld` to link the CUDA
   `.cu.o` objects), and sets `NO_STRIP=1` (linuxdeploy's bundled `strip`
   chokes on Fedora 43+'s `SHT_RELR` sections).
3. **Sidecar build**: builds the `llama-helper` crate (release) with the
   matching GPU feature; `dev.sh` points `PARLEY_LLAMA_HELPER` at it directly,
   `build.sh` stages a copy into `target/gpui-dist/`.
4. **GPUI build/run**: `dev.sh` runs `cargo run -p parley-gpui --features
   {cuda,vulkan}` as needed; `build.sh` runs `cargo build --release -p
   parley-gpui`, stages the binary + native libs into `target/gpui-dist/`,
   and packages that into an AppImage via
   `parley-gpui/packaging/linux/build-appimage.sh`.

| Mode     | Feature Flag          | Typical Speedup |
| -------- | ---------------------- | ---------------- |
| CUDA     | `--features cuda`      | 5-10x            |
| Vulkan   | `--features vulkan`    | 3-6x             |
| CPU      | (none)                 | 1x (baseline)    |

---

## 🔧 GPU Setup

### 🟢 NVIDIA CUDA

**Prerequisites:** NVIDIA GPU + CUDA toolkit installed.

```bash
# Ubuntu/Debian
sudo apt install nvidia-driver-550 nvidia-cuda-toolkit

# Verify
nvidia-smi          # Shows GPU info
nvcc --version       # Shows CUDA version

# Build (auto-detected if nvidia-smi works, or force it)
./build.sh cuda
```

`build.sh` defaults `CUDAARCHS` to `"75;80;86;89;90"` (Turing→Hopper) for a
portable release binary. `dev.sh` instead detects your specific compute
capability via `nvidia-smi --query-gpu=compute_cap` for a much faster
incremental build. Override either with `CUDAARCHS=... ./build.sh cuda`.

### 🔵 Vulkan (Cross-Platform Fallback)

Works on NVIDIA, AMD, and Intel GPUs — good choice if CUDA isn't available.

```bash
# Ubuntu/Debian
sudo apt install vulkan-sdk libopenblas-dev

# Fedora
sudo dnf install vulkan-devel openblas-devel

# Arch Linux
sudo pacman -S vulkan-devel openblas

./build.sh vulkan
```

### Other backends (AMD ROCm / OpenBLAS)

`whisper-rs` also exposes `hipblas` (AMD ROCm) and `openblas` Cargo features,
but they aren't wired into `build.sh`/`dev.sh` as a `--mode`. If you need
them, build the workspace directly, e.g.:

```bash
cargo build --release -p llama-helper --features hipblas
cargo build --release -p parley-gpui --features hipblas
```

This path is unsupported by the helper scripts — expect to hand-manage the
`llama-helper` sidecar staging and AppImage packaging steps yourself (see
step 3-4 above, or run `parley-gpui/packaging/linux/build-appimage.sh`
directly against a dist dir you assemble by hand).

---

## 🎯 Advanced Usage

### Environment Variable Reference

| Variable            | Purpose                                       | Set by                    |
| -------------------- | ---------------------------------------------- | -------------------------- |
| `CUDAHOSTCXX`         | Host C++ compiler for `nvcc`                   | auto (Fedora g++-15/14)    |
| `CUDAARCHS`           | CUDA arch list                                 | auto (`75;80;86;89;90` for `build.sh`, single-arch for `dev.sh`) |
| `NO_STRIP`            | Skip AppImage symbol stripping                 | `build.sh` (`1`)           |
| `RUST_LOG`            | Log filter                                     | `dev.sh` (`info,whisper_rs=warn`) |
| `RUST_BACKTRACE`      | Full Rust backtraces on panic                  | `dev.sh` (`full`)          |

### Build Output Location

```
target/gpui-dist/                       staged binary + native libs
Parley-<version>-x86_64.AppImage        repo root — the final packaged app
```

Only the AppImage bundle is produced — it embeds all native libs (sherpa-onnx,
onnxruntime, llama-helper) via linuxdeploy, so it runs on a clean host without
a `.deb`/`.rpm`'s dependency resolution.

---

## 🧭 Troubleshooting

### "CUDA toolkit not found"
- **Fix:** Install `nvidia-cuda-toolkit` or ensure `nvcc --version` works.

### Fedora 44 build fails with a gcc/nvcc mismatch
- **Fix:** `build.sh`/`dev.sh` auto-detect `g++-15`/`g++-14` and set
  `CUDAHOSTCXX` for you. If neither is installed: `sudo dnf install gcc-c++15`
  (or the appropriate compat package for your Fedora release).

### AppImage build strips symbols / crashes at runtime
- **Fix:** Already handled — `build.sh` sets `NO_STRIP=1` on Linux
  automatically (Fedora 43+'s `SHT_RELR` sections trip up linuxdeploy's
  bundled `strip`).

### `Could not find dependency: libsherpa-onnx-c-api.so`
- **Fix:** Already handled — `build.sh` and
  `parley-gpui/packaging/linux/build-appimage.sh` point linuxdeploy at the
  staged dist dir via `LD_LIBRARY_PATH` so it can find and bundle the lib. If
  you invoke `build-appimage.sh` by hand against your own dist dir, export it
  yourself; a build without it produces an AppImage that is missing the
  library or is truncated to a few bytes. CI verifies the AppImage is at
  least 50 MB and contains the library before treating the build as green.

### Build works but no GPU acceleration
- **Check:** `nvidia-smi` (NVIDIA) should work before `./build.sh` (or
  `./dev.sh`) auto-selects CUDA; otherwise pass the mode explicitly.

---

## ✅ Compiler Cache (optional, faster rebuilds)

If [`sccache`](https://github.com/mozilla/sccache) is installed, `build.sh`
and `dev.sh` enable it automatically for Rust + C/C++ + CUDA compiles.

```bash
cargo install sccache
./dev.sh   # picks it up automatically
```

---

**Need help?** Open an issue on [Hankanman/Parley](https://github.com/Hankanman/Parley/issues) with your GPU type, distro, and the output from `./build.sh`.
