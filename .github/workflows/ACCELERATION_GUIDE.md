# CI/CD Hardware Acceleration Guide

This document explains the hardware acceleration configuration for the CI/CD
workflows in this repository.

Parley is Linux-only (see [CLAUDE.md](../../CLAUDE.md)) and
builds a single GPUI desktop app (`parley-gpui`) via `./build.sh`. GitHub
Actions runners have no GPU, so every CI build (`build.yml`, `build-linux.yml`,
`build-test.yml`, `release.yml`) runs `./build.sh cpu` — plain CPU whisper.cpp,
no `--features` flag. This keeps CI simple and matches what a user without a
GPU gets locally with `./dev.sh cpu` / `./build.sh cpu`.

## Local acceleration

`parley-core` exposes these Cargo features (forwarded by `parley-gpui` and
`llama-helper`):

```toml
[features]
cuda = ["whisper-rs/cuda"]          # NVIDIA CUDA
vulkan = ["whisper-rs/vulkan"]      # AMD/Intel Vulkan
hipblas = ["whisper-rs/hipblas"]    # AMD ROCm
openblas = ["whisper-rs/openblas"]  # Optimized CPU BLAS
```

Use them locally via the mode argument, which both `./dev.sh` and `./build.sh`
accept:

```bash
./dev.sh cuda      # or vulkan, or cpu
./build.sh cuda
```

`./dev.sh` / `./build.sh` auto-detect NVIDIA hardware (`nvidia-smi`) and pick
`cuda` by default when present, `cpu` otherwise. See the root of
[dev.sh](../../dev.sh) and [build.sh](../../build.sh) for the full env-var
list (CUDA arch pinning, `NO_STRIP`, sccache, etc).

## Why CI doesn't use GPU or OpenBLAS features

- GitHub-hosted runners have no NVIDIA/AMD GPU, so `cuda`/`vulkan` builds
  aren't possible there.
- `openblas` was used in the old Tauri-based CI to speed up whisper.cpp on
  CPU-only runners; it added an `apt` dependency (`libopenblas-dev`) and a
  `--features` flag for a modest win. It isn't currently wired into
  `./build.sh`'s mode selection (`cuda`/`vulkan`/`cpu` only), so CI stays on
  plain `cpu` for simplicity. Revisit this if CI build/transcribe time in a
  test job becomes a problem.

## Related Documentation

- [CLAUDE.md](../../CLAUDE.md) - Project overview with build commands
- [WORKFLOWS_OVERVIEW.md](WORKFLOWS_OVERVIEW.md) - All workflows comparison
- [Whisper.cpp GitHub](https://github.com/ggerganov/whisper.cpp) - Upstream project
