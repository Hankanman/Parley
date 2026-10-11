# whisper-helper

Stdio JSON-lines whisper.cpp transcription sidecar — prototype for
[issue #56](../docs/transcription-backends.md) (runtime backend selection).
Modelled directly on `../llama-helper` (see that crate + `llama-protocol`
for the pattern this follows).

**Status**: standalone spike crate. Not yet staged by `build.sh`/`dev.sh`
(which now build the GPUI app, not the retired Tauri shell this spike was
originally scoped against), and not spawned by the running app. See
`docs/transcription-backends.md` for the design and implementation plan to
get from here to a shipped sidecar.

## Build matrix

Same shape as `llama-helper`: one binary per backend, selected via Cargo
features, built exactly the way `build.sh` builds `llama-helper` today.

```bash
# CPU (default features, no GPU feature flag)
cargo build --release -p whisper-helper

# NVIDIA CUDA
cargo build --release -p whisper-helper --features cuda

# AMD/Intel Vulkan
cargo build --release -p whisper-helper --features vulkan
```

Each feature forwards to the matching `whisper-rs` (and `whisper-core`)
feature, so the same `whisper-rs-sys` build.rs / cmake logic that builds the
main app's whisper.cpp runs here — no separate GPU toolchain setup beyond
what `docs/building_in_linux.md` already documents for CUDA/Vulkan builds of
the main app.

Staging a `whisper-helper` binary into `target/gpui-dist/` next to
`llama-helper` (the same way `build.sh` already stages that sidecar for the
GPUI AppImage) is a follow-up for `build.sh`/`dev.sh` once the sidecar is
wired into the live path — see the design doc's implementation plan.

## Protocol

Newline-delimited JSON over stdin/stdout, shared with the app via the
`whisper-protocol` crate (`Request`/`Response`) so the two ends can't drift
apart silently:

| Request | Response | Purpose |
|---|---|---|
| `load_model { path }` | `loaded { error, load_ms }` | Load/switch the GGML/GGUF model |
| `transcribe { samples_b64, language, context_prompt, max_threads, greedy }` | `transcribed { text, confidence, is_partial, segments, error, decode_ms }` | Transcribe one 16kHz mono f32 PCM chunk (base64 little-endian samples) |
| `unload` | `unloaded` | Free the loaded model/context without exiting |
| `ping` | `pong` | Liveness check |
| `probe { model_path }` | `probe_result { backend, decode_ok, detail }` | Self-report compiled backend; with a model, run a 1s synthetic decode to confirm the backend actually initializes on this machine |
| `shutdown` | `goodbye` | Clean exit |

## What's shared with the in-process engine

`whisper-core` (sibling crate) holds the `FullParams` construction and
decoder-confidence formula, ported from
`parley-core/src/whisper_engine/whisper_engine.rs`'s
`transcribe_audio_with_confidence_opts`. The in-process engine itself is
**unchanged** by this spike — see `whisper-core`'s doc comment for the
follow-up that would switch it to call the shared crate too and delete the
duplication.

## Measurements (this spike, CPU build, `ggml-tiny.bin`, this container)

See `docs/transcription-backends.md` for the full write-up. Headline
numbers:

- Spawn → `ping`: ~2ms median
- `load_model` (tiny, 75MB): ~93ms
- 5s-silence `transcribe` round trip: ~630ms (of which ~624ms is whisper's
  own `full()` decode — JSON+base64 IPC overhead is ~4ms, <1% of the total)
- Release (`opt-level = "s"`) CPU binary size: **2.3MB** (dynamically linked
  against libstdc++/libgcc_s/libm/libc only; whisper.cpp/ggml are statically
  linked in)
