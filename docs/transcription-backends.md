# Runtime whisper backend selection (issue #56 spike)

Engineering spike: can Parley ship one binary that picks a CPU/Vulkan/CUDA
whisper backend at runtime, instead of building (and shipping) a separate
binary per backend at compile time as it does today? This document reports
what was found, what was prototyped, and what a real implementation would
cost.

**Bottom line**: `whisper-rs` 0.16 / `whisper-rs-sys` 0.15 give us no
built-in dynamic-backend mechanism, and upgrading won't help — 0.16.0 is
already the latest release and its feature set is unchanged from 0.15. The
sidecar-per-backend pattern already proven by `llama-helper` is the
practical path, and it works: this spike built a working `whisper-helper`
CPU sidecar sharing decode logic with the in-process engine, measured its
overhead, and found it acceptable (see [Measurements](#measurements)).
Recommendation: proceed with the sidecar architecture, scoped as a
multi-milestone project (see [Implementation plan](#implementation-plan)) —
not a small patch.

## 1. Confirmed whisper-rs facts

Read directly from
`~/.cargo/registry/src/*/whisper-rs-sys-0.15.0/build.rs`:

- **No `GGML_BACKEND_DL` anywhere.** The build script defines
  `GGML_CUDA`, `GGML_VULKAN`, `GGML_BLAS_VENDOR` (OpenBLAS), etc. as CMake
  `ON`/link flags gated by `cfg!(feature = "...")`, and links the resulting
  static libs (`static=ggml-cuda`, `static=ggml-vulkan`, ...) directly into
  the Rust binary. There is no code path that builds ggml with
  `GGML_BACKEND_DL=ON` (upstream ggml's actual dynamic-backend-loading
  option) or that loads a `.so` backend plugin at runtime. Every backend is
  baked into the binary at compile time, exactly as the CLAUDE.md doc for
  this repo already describes.
- **whisper-rs 0.16.0 is the latest version on crates.io** (checked via
  `cargo info whisper-rs` through the proxy) and its feature list —
  `cuda`, `vulkan`, `hipblas`, `coreml`, `metal`, `openblas`, `openmp`,
  `intel-sycl`, `raw-api`, `log_backend` — is the same shape as 0.15's;
  none of them enable dynamic loading. whisper-rs is a thin wrapper over
  whisper.cpp/ggml and doesn't add backend-selection logic of its own.
- Upstream **ggml** (the C library whisper.cpp sits on) does have a real
  `GGML_BACKEND_DL` CMake option in recent versions, which builds each
  backend as a separate `.so` loaded via `dlopen` at runtime — this is
  the mechanism llama.cpp's own release binaries use. But `whisper-rs-sys`
  0.15's vendored ggml snapshot and build.rs don't wire it up, and there is
  no whisper-rs release that does. Adopting it would mean forking
  `whisper-rs-sys`'s build.rs to add `config.define("GGML_BACKEND_DL", "ON")`
  plus per-backend shared-library CMake targets, and reworking whisper-rs's
  FFI layer (`raw-api` feature) to `dlopen` the right backend at
  `WhisperContext` construction instead of linking one in — a substantial,
  upstream-affecting change with no guarantee of being accepted, and one
  this repo doesn't control (whisper-rs is a third-party crate, unlike
  llama-cpp-2 which Parley doesn't fork either).

**Conclusion**: waiting for or patching in `GGML_BACKEND_DL` support is not
a near-term option. The sidecar pattern — already shipped for LLM summaries
via `llama-helper` — is the pragmatic way to get runtime backend selection
without forking a C++ build system.

## 2. What was built

Three new workspace crates, modelled directly on `llama-helper` +
`llama-protocol`:

```
whisper-protocol/   Shared wire-protocol types (Request/Response), like llama-protocol
whisper-core/       FullParams construction + decoder-confidence scoring,
                     ported from whisper_engine.rs — usable from both the
                     in-process engine (future) and any sidecar
whisper-helper/     The sidecar binary itself (stdio JSON-lines server)
```

Plus, behind a Cargo feature that is **off by default** and touches nothing
in the live recording path:

```
parley-core/src/audio/transcription/backend_probe.rs   (feature = "backend_probe")
```

### whisper-protocol

`Request`: `load_model`, `transcribe` (base64 f32le samples + language +
context prompt + per-call thread/greedy overrides), `unload`, `ping`,
`probe` (see below), `shutdown`. `Response`: `loaded`, `transcribed`
(text + confidence + per-segment breakdown + timing), `unloaded`, `pong`,
`probe_result`, `goodbye`, `error`. Round-trip and forward-compatibility
(unknown-field-ignored) unit tests included, same style as
`llama-protocol`'s.

### whisper-core

Extracted the `FullParams` construction and the decoder-confidence formula
out of `WhisperEngine::transcribe_audio_with_confidence_opts` into a
standalone function, `transcribe_pcm16k(ctx, samples, language,
context_prompt, decode_config, options) -> TranscriptionOutcome`. It takes
hardware-adaptive knobs (`DecodeConfig { beam_size, max_threads,
temperature }`) as plain arguments rather than reaching for
`audio::HardwareProfile` itself, so it has zero dependency on Tokio,
Tauri, or the app crate — a sidecar binary can use it standalone.

**The in-process `WhisperEngine` is left unmodified for this spike.** The
extraction is a faithful port (same params, same confidence math, same
`pad_to_min_whisper_input` floor), not yet a shared call site — see
[Implementation plan](#implementation-plan) milestone 2 for wiring
`WhisperEngine` itself to call `whisper-core` and deleting the ~150 lines of
duplicated logic it currently owns.

### whisper-helper

A stdio JSON-lines server with the same lifecycle as `llama-helper`: read
one `Request` per line from stdin, write exactly one terminal `Response`
line to stdout, serve until stdin closes or `Shutdown` arrives, restore
default `SIGPIPE` disposition at startup so a vanished parent exits cleanly
instead of panicking. `Probe` is new relative to `llama-helper`'s protocol —
purpose-built for backend selection (see §3): with no model path it's a
ping-equivalent; with one, it loads the model and runs a 1s synthetic
(silent) decode, so a `cuda`-featured binary that can't actually find an
NVIDIA driver fails the probe instead of reporting false success.

Cargo features mirror the main app's and `llama-helper`'s exactly:

```bash
cargo build --release -p whisper-helper                    # CPU
cargo build --release -p whisper-helper --features cuda    # NVIDIA
cargo build --release -p whisper-helper --features vulkan  # AMD/Intel
```

Only the CPU variant was built in this container (no CUDA/Vulkan
toolchains available here — same constraint noted in the task). See
`whisper-helper/README.md` for the full build matrix and protocol table.

### backend_probe.rs (selection logic prototype)

`audio::transcription::backend_probe`, compiled only under
`--features backend_probe` (off by default; **not** added to `lib.rs`'s
`invoke_handler![]`, so it ships inert even if the feature were flipped on).
It:

1. Enumerates `whisper-helper-{cuda,vulkan,cpu}` next to the running
   executable (mirrors where `build.sh` stages `llama-helper-<triple>`
   today, i.e. `frontend/src-tauri/binaries/`).
2. Spawns each, sends a `Probe` request, reads the terminal response.
3. Picks the first that reports `decode_ok: Some(true)` in priority order
   **cuda → vulkan → cpu**.
4. Returns a `BackendSelection { candidates, recommended, manual_override }`
   — `manual_override` is a placeholder field for a persisted user choice;
   actually wiring it to config storage is implementation-phase work (see
   below), not part of this spike.
5. `get_transcription_backends()` is the shape a future
   `#[tauri::command]` for a settings UI would take (currently a plain
   `pub fn`, not registered as a command).

This module type-checks and its unit tests pass in isolation (verified via
a scratch crate depending only on `whisper-protocol`, `anyhow`, `serde` —
see below on why it couldn't be checked in-tree in this container).

## 3. Probe design

Why a real decode instead of just `ping`: a `cuda`-featured
`whisper-helper` binary links against `libcudart`/`libcuda` and will often
still **spawn and respond to `ping`** on a machine with no NVIDIA GPU or
driver (dynamic linking succeeds; only CUDA API calls at runtime fail) —
whisper.cpp's CUDA init path fails inside `WhisperContext::new_with_params`
or on the first `full()` call, not at process start. `ping` alone would
therefore misreport `cuda` as available on non-NVIDIA machines. The `probe`
request forces an actual `WhisperContext` load + a 1s silent decode through
that backend, which is the earliest point a real CUDA/Vulkan failure
surfaces. Cost: ~1.2s wall time per candidate with a model on disk (measured
below) — acceptable for a one-time startup scan, not for anything
per-recording.

When no model is available yet (first run, before any model download),
`probe` degrades to a ping-only check and reports `decode_ok: None` —
`backend_probe` treats that as "runnable" but not "GPU-confirmed", which is
the best available signal until a model exists.

## 4. Measurements

All measured in this container: CPU-only sidecar build (`opt-level = "s"`,
`codegen-units = 1`, matching `llama-helper`'s release profile), model
`ggml-tiny.bin` (~74MB, fetched through the proxy), 5 runs each unless
noted, `ping`/`transcribe` measured via a Python harness driving the
compiled binary directly over stdio.

| Measurement | Result |
|---|---|
| Process spawn → `ping` response | **~2.0ms** median (1.87–2.29ms across 5 runs) |
| `load_model` (tiny, 74MB, cold) | **93ms** |
| `transcribe` round trip, 5s silence, JSON+base64 | **628–694ms** (3 runs) |
| ...of which whisper's own `full()` decode | **624ms** (`decode_ms` field) |
| ⇒ IPC overhead (JSON encode/decode + base64 + pipe) | **~4ms**, <1% of the round trip |
| `probe` (spawn + load + 1s synthetic decode), cold | **1.20s** |
| Base64 payload size for 5s @ 16kHz f32 | 426,668 bytes (320,000 bytes raw × 4/3 base64 overhead) |
| CPU sidecar binary size (release, `opt-level = "s"`) | **2.3MB**, dynamically linked only against libstdc++/libgcc_s/libm/libc (whisper.cpp/ggml statically linked in) |

**Reading these numbers**: sidecar spawn and IPC are cheap relative to
actual decode time — even on the tiny model, `full()` dominates the round
trip by ~150×. For `base`/`small`/`medium` models (Parley's actual
dev/production tiers per CLAUDE.md), decode time only grows, so the
JSON+base64 transport is very unlikely to be the bottleneck; a shared-memory
or tmpfile transport would shave single-digit milliseconds off something
that already takes hundreds of milliseconds to tens of seconds. **IPC
transport choice should not drive the architecture decision** — see §5.

**CUDA/Vulkan variants were not built or measured** — this container has no
CUDA or Vulkan toolchain (confirmed: no `nvcc`, no Vulkan SDK). Their
startup/decode numbers should track `llama-helper`'s CUDA/Vulkan builds
closely (same lazy-spawn sidecar shape, same whisper.cpp/ggml compute core)
but that should be re-measured on real hardware before shipping, not assumed
from this container's CPU numbers.

### AppImage size delta (estimated, not measured)

The CPU sidecar's 2.3MB is not representative of the other two — whisper.cpp
built with `GGML_CUDA=ON` statically links CUDA compute kernels for every
target SM architecture (`build.sh` pins `CUDAARCHS="75;80;86;89;90"`, i.e.
Turing through Hopper — 5 architectures), and `GGML_VULKAN=ON` embeds
compiled SPIR-V shaders. Based on published whisper.cpp/llama.cpp CUDA
build sizes for a comparable multi-arch static link, a reasonable estimate
is:

| Sidecar | Estimated size |
|---|---|
| `whisper-helper-cpu` | ~2–3MB (measured: 2.3MB) |
| `whisper-helper-vulkan` | ~15–40MB (SPIR-V shader modules, no per-SM duplication) |
| `whisper-helper-cuda` | ~150–350MB (5 SM architectures' worth of CUDA kernels) |

Shipping all three sidecars in one AppImage would add roughly **170–390MB**
over today's CPU-only AppImage — dominated almost entirely by the CUDA
variant. This is the single most important number for the go/no-go decision
in §5 and **must be measured on real hardware with the actual CUDA/Vulkan
toolchains** before committing to "ship all three in every release"; the
range above is wide enough that it could be a non-issue or a real problem
depending on where it lands.

## 5. Recommended architecture

**Sidecar-per-backend**, following `llama-helper`'s already-proven pattern,
not a fork of `whisper-rs-sys` to chase `GGML_BACKEND_DL`. Specifics:

- **IPC**: keep JSON-lines over stdio (matching `llama-helper`) for control
  messages (`load_model`, `ping`, `probe`, `unload`, `shutdown`) and for
  `transcribe` on typical chunk sizes. The measurements above show IPC
  overhead is negligible next to decode time even with base64 inflation.
  **Do not** add shared-memory/tmpfile transport in the first
  implementation — it's premature optimization for a cost that's currently
  <1% of the round trip, and it adds real complexity (lifecycle of a shared
  segment across process crashes, platform differences). Revisit only if
  profiling on real hardware (larger models, longer chunks, GPU decode
  being fast enough that IPC becomes a bigger fraction) shows it matters.
- **Distribution**: do not ship all three sidecars in every release by
  default given the AppImage size estimate above. Two realistic options,
  in order of preference:
  1. **Ship CPU inline** (small, always works) **+ fetch CUDA/Vulkan
     sidecars on first use**, keyed off `backend_probe`'s hardware
     detection, the same way whisper models themselves are already
     downloaded on demand (`WHISPER_MODEL_CATALOG` /
     `utils::download::DownloadGuard`). This keeps the default AppImage
     close to today's size and only pays the CUDA/Vulkan cost on machines
     that can use it.
  2. **Separate release artifacts** (`parley-cpu.AppImage`,
     `parley-cuda.AppImage`, ...) as today, but each bundling *only* its
     own sidecar (which is a no-op today since the backend is compiled in)
     — this is the status quo and doesn't actually solve issue #56's stated
     problem (official releases shipping CPU-only), so it's listed only as
     a fallback if on-demand fetch proves impractical.
- **Selection & override**: `backend_probe::scan_backends` at first-run /
  settings-open time, in priority order cuda → vulkan → cpu, persisted in
  the existing transcript config/preferences store (co-located with
  `WHISPER_MODEL_CATALOG`'s config, not a new subsystem) with a manual
  override field. Re-probe on app update (a new sidecar build) and on
  demand from the settings UI (the "test backend" button pattern most audio
  apps use).
- **Failure/fallback behavior**:
  - No sidecar binaries present at all (e.g. mid-migration, or a build that
    didn't stage them) → fall back to the in-process `WhisperEngine`
    exactly as it works today. This is why milestone 2 (below) keeps the
    in-process path alive rather than deleting it.
  - Chosen backend's sidecar crashes or fails to respond mid-recording →
    the recording worker should catch the broken pipe / timeout, log it,
    and fall back one step down the priority list (cuda→vulkan→cpu→
    in-process), surfacing a toast/notification rather than silently
    dropping transcription. This mirrors `SidecarManager`'s existing
    idle/lifecycle handling for `llama-helper` — reuse that manager rather
    than writing a second one.
  - `probe` reporting `decode_ok: Some(false)` for the top-priority backend
    at startup → skip it silently in `recommended`, but still list it in
    `candidates` with its `detail` string so a settings UI can show *why*
    (e.g. "cuda: synthetic decode failed: <driver error>") rather than just
    omitting it.

## 6. Implementation plan

Effort estimates assume one engineer familiar with this codebase; sizes are
rough (S ≈ 1-2 days, M ≈ 3-5 days, L ≈ 1-2 weeks).

1. **[S] Land the spike crates as-is, wire in CI.** `whisper-protocol`,
   `whisper-core`, `whisper-helper` already build clean (0 warnings) and
   have unit tests; add them to whatever CI matrix builds `llama-helper`
   today. No behavior change to the shipped app (nothing new is wired into
   `lib.rs`).
2. **[M] Switch `WhisperEngine` to call `whisper-core`.** Replace
   `transcribe_audio_with_confidence_opts`'s inline `FullParams`/confidence
   code with a call to `whisper_core::transcribe_pcm16k`, feeding it
   `HardwareProfile`-derived `DecodeConfig`. Deletes ~150 duplicated lines,
   makes the in-process engine and the future sidecar provably identical in
   behavior (same function, not a hand-kept-in-sync port). Needs care around
   `whisper-core`'s `set_no_timestamps(false)` (sidecar wants per-segment
   timestamps; the in-process engine currently forces `true` — reconcile
   via a `DecodeConfig`/params flag rather than silently changing existing
   behavior) and the `clean_repetitive_text`/`is_meaningless_output`
   post-processing the in-process engine does that wasn't ported into
   `whisper-core` for this spike (needs a decision: move into
   `whisper-core` too, or keep as an engine-side post-process step callable
   from both paths).
3. **[M] `SidecarManager`-style lifecycle for whisper-helper.** Reuse/
   generalize `summary/summary_engine/sidecar.rs`'s idle-timeout/respawn
   logic rather than writing a parallel one; wire `backend_probe` results
   into it as the "which binary to spawn" input.
4. **[M] Build/staging integration.** Extend `build.sh`/`dev.sh` to build
   `whisper-helper` alongside `llama-helper` (same `HELPER_FEATURES` pattern
   already in `build.sh`) and stage it into `binaries/` under Tauri's
   `externalBin` naming convention. Decide and implement the
   on-demand-fetch-for-GPU-variants distribution model from §5 — this is
   the highest-uncertainty item; depends on real AppImage size numbers from
   milestone 1's CI builds on GPU-capable runners.
5. **[L] Route the live recording path through the sidecar,** behind the
   `backend_probe`-selected backend, with the in-process engine as the
   fallback described in §5. Includes: settings UI consuming
   `get_transcription_backends` (registering it as an actual
   `#[tauri::command]`), the manual-override persistence `backend_probe`
   currently stubs, and end-to-end testing on real CUDA/Vulkan hardware
   (unavailable in this spike's container).
6. **[S] Remove compile-time `cuda`/`vulkan`/`hipblas`/`openblas` features
   from the main app crate** once milestone 5 ships and is validated, since
   the sidecar makes them redundant — cleans up `frontend/src-tauri/
   Cargo.toml`'s feature list and the per-backend release build matrix in
   `build.sh`/CI.

Milestones 1–2 are safe to land independently and incrementally de-risk the
rest; 3–5 are where the actual runtime-selection payoff (issue #56's ask)
lands, and 4 in particular needs real GPU hardware to validate the
distribution-size tradeoff before committing.

## Appendix: build/test status in this environment

- `whisper-protocol`, `whisper-core`, `whisper-helper`: `cargo build
  --release -p whisper-helper` and `cargo test -p whisper-core -p
  whisper-protocol --release` both pass with **zero errors and zero
  warnings** (`CARGO_TARGET_DIR=/home/user/Meetily-Local/target`,
  `SHERPA_ONNX_ARCHIVE_DIR` set per the task instructions).
- `backend_probe.rs`: could not run `cargo check` on the full
  `frontend/src-tauri` crate in this container — it fails **before**
  reaching this module, in an unrelated pre-existing dependency
  (`webrtc-audio-processing-sys` needs system lib
  `webrtc-audio-processing-2` ≥ 2.1 via pkg-config; this container only has
  `libwebrtc-audio-processing-dev` 0.3.1, the older v1 API/soname). This is
  an environment gap unrelated to anything touched in this spike — verified
  by confirming the failure occurs during `webrtc-audio-processing-sys`'s
  build script, entirely before `audio::transcription` (or any code this
  spike added) is compiled. To still validate `backend_probe.rs`, it was
  checked in an isolated scratch crate depending only on `whisper-protocol`,
  `anyhow`, and `serde` (its actual dependency set under the
  `backend_probe` feature): `cargo check` and `cargo test` both pass clean,
  including its two unit tests.
