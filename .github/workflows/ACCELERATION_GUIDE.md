# CI/CD Hardware Acceleration Guide

This document explains the hardware acceleration configuration for the CI/CD
workflows in this repository.

Parley is Linux-only (see [CLAUDE.md](../../CLAUDE.md)) and builds a single
GPUI desktop app (`parley-gpui`) with `./build.sh`. There are two modes:

- `cpu`: plain CPU whisper.cpp and llama.cpp. This is the default for CI
  builds and test jobs.
- `vulkan`: GPU acceleration on NVIDIA, AMD and Intel. The release workflow
  builds it alongside the CPU AppImage. Compiling it needs the Vulkan headers
  and `glslc`, not a GPU, so it builds on GitHub-hosted runners.

There is no CUDA or ROCm build. Vulkan covers NVIDIA and AMD GPUs; see
[docs/benchmarks/2026-11-gpu.md](../../docs/benchmarks/2026-11-gpu.md) for
the comparison with CUDA.

## Local acceleration

`parley-core` exposes these Cargo features (forwarded by `parley-gpui`;
`llama-helper` has a matching `vulkan` feature):

```toml
[features]
vulkan = ["whisper-rs/vulkan"]      # NVIDIA, AMD and Intel GPUs
openblas = ["whisper-rs/openblas"]  # Optimised CPU BLAS
openmp = ["whisper-rs/openmp"]
```

Both `./dev.sh` and `./build.sh` take the mode as an argument:

```bash
./dev.sh vulkan    # or cpu
./build.sh vulkan
```

With no argument they pick `vulkan` when the machine has a GPU render node
and `glslc`, and `cpu` otherwise. See the top of [dev.sh](../../dev.sh) and
[build.sh](../../build.sh) for the environment variables they set.

## Why CI doesn't use OpenBLAS

`openblas` was used in the old Tauri-based CI to speed up whisper.cpp on
CPU-only runners. It added an `apt` dependency (`libopenblas-dev`) and a
`--features` flag for a modest gain, and it isn't wired into `./build.sh`'s
modes, so CI stays on plain `cpu`. Revisit this if CI build or transcription
time in a test job becomes a problem.

## Related Documentation

- [CLAUDE.md](../../CLAUDE.md) - Project overview with build commands
- [WORKFLOWS_OVERVIEW.md](WORKFLOWS_OVERVIEW.md) - All workflows comparison
- [Whisper.cpp GitHub](https://github.com/ggerganov/whisper.cpp) - Upstream project
