# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Parley** (formerly Meetily-Local, a fork of Zackriya-Solutions/meetily; the GitHub repo is still `Hankanman/Meetily-Local`) is a privacy-first AI meeting assistant that captures, transcribes, and summarizes meetings entirely on local infrastructure. It's a single self-contained GPUI desktop application — no separate backend server, no webview, no JavaScript.

### Key Technology Stack
- **Desktop shell**: [GPUI](https://www.gpui.rs/) (Rust, the UI framework behind Zed) via `gpui-kit`/`gpui-component`
- **Audio Processing**: Rust (native PipeWire capture, whisper-rs, professional audio mixing)
- **Transcription**: Whisper.cpp (local, GPU-accelerated, in-process via whisper-rs)
- **Persistence**: SQLite via sqlx in the same Rust process
- **LLM Integration**: built-in llama.cpp sidecar (`llama-helper` crate), or remote Ollama / Claude / Groq / OpenRouter / OpenAI-compatible endpoint

## Essential Development Commands

Root-level scripts (recommended — handle CUDA/Vulkan env setup and the `llama-helper` sidecar build for you):

```bash
./dev.sh                    # auto: CUDA on NVIDIA, CPU otherwise — cargo run -p parley-gpui
./dev.sh cuda                # NVIDIA CUDA
./dev.sh vulkan               # AMD/Intel Vulkan
./dev.sh cpu                  # CPU-only
./build.sh                    # production build → Parley-<version>-x86_64.AppImage
./build.sh cuda                # NVIDIA CUDA
./build.sh vulkan               # AMD/Intel Vulkan
./build.sh cpu                   # CPU-only
./clean.sh                    # nuke target/
```

`gpui` is also accepted as a no-op leading argument on both scripts (`./dev.sh gpui cuda`) for muscle memory — it's the only shell now, so it doesn't change anything.

Manual `cargo` commands, if you don't want the root scripts:

```bash
cargo run -p parley-gpui                       # debug run, CPU
cargo run -p parley-gpui --features cuda        # debug run, CUDA
cargo build --release -p parley-gpui --features vulkan   # release build
```

The app has no HTTP listener and no IPC boundary — the UI (`parley-gpui`) calls into the core (`parley-core`) as plain Rust function calls, in-process.

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                 parley-gpui (single process)                   │
│  ┌──────────────────┐    ┌──────────────────────────────────┐  │
│  │ GPUI views       │    │ parley-core                     │  │
│  │ (Rust)           │←──→│   • Audio capture + mixing + VAD │  │
│  │ recording,       │    │   • whisper-rs / parakeet        │  │
│  │ meeting, settings│    │   • SQLite via sqlx              │  │
│  │ speakers, tray   │    │   • Summary engine               │  │
│  │                  │    │   • llama-helper sidecar         │  │
│  └──────────────────┘    └──────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                ↓ optional outbound LLM calls
   Ollama (local or remote) / Claude / Groq / OpenRouter / custom OpenAI
```

### Crate Layout: Tauri-free core + GPUI shell

- **`parley-core/`** (lib `parley_core`) — everything that isn't UI glue:
  audio pipeline + PipeWire capture, transcription, recording orchestration
  (`audio/recording_service.rs`), SQLite repositories + `migrations/`,
  summaries/LLM providers + embedded `templates/`, model management, speaker
  diarization. **It must never depend on `tauri`** (there is no Tauri
  dependency anywhere in the workspace anymore) — check with
  `cargo tree -p parley-core | grep -i tauri` (must be empty).
- **`parley-gpui/`** (bin `parley-gpui`) — the GPUI desktop shell: links
  `parley-core` directly (no webview, no IPC). See `parley-gpui/src/`:
  - `main.rs` — entry point, window setup, shutdown coordination
  - `app_state.rs` — `AppServices`, the process-wide global (`cx.global::<AppServices>()`)
    holding the DB slot, the event sink, and the summary model-manager state
  - `core_events.rs` — `GpuiSink` (the `events::EventSink` impl) and
    `CoreEvents`, the entity views subscribe to for core→UI updates
  - `root.rs` — top-level view: onboarding gate, then the shell
  - `shell/` — the main app chrome (sidebar, meeting list, recording bar)
  - `views/` — one module per feature area: `recording/`, `meeting/`,
    `settings/`, `speakers/`, `onboarding/`, `import/`, `action_items/`
  - `tray.rs` — system tray (via `gpui-tray`)
  - `notifications.rs`, `recovery.rs`, `runtime.rs` — desktop notifications,
    interrupted-meeting recovery, the Tauri-free async runtime glue
  - `packaging/linux/` — `build-appimage.sh` + the `.desktop` file + app icon
    used to produce the AppImage

Core → UI communication goes through the `events::EventSink` trait
(`emit_event(name, &payload)`), never a UI handle. The shell wraps this in
`core_events::GpuiSink`, whose background task re-emits each event onto the
`CoreEvents` entity so views subscribe with `cx.subscribe(&core_events, ...)`
and decode payloads with `CoreEvent::decode::<T>()`. Tests use `NullSink` /
`RecordingSink` (defined in `parley-core::events`). Core resolves
directories with `paths::app_data_dir()` (`~/.local/share/io.github.hankanman.Parley`)
and takes the DB as a `SqlitePool` / `Option<SqlitePool>` argument instead of
reading framework-managed state. Recording persistence subscribes to
finished segments on the in-process `audio::transcript_bus`, not to UI
events. Event names and payloads are a long-standing contract (originally
shared with a since-retired Tauri/React shell) — don't change them casually
when refactoring, since existing recovery/IndexedDB-shaped logic on the Rust
side still keys off them.

GPU features (`cuda`, `vulkan`, `hipblas`, `openblas`, `openmp`) live on
`parley-core`; `parley-gpui` and `llama-helper` forward the same feature
names, so `./dev.sh cuda` etc. are unchanged.

### Audio Processing Pipeline (Critical Understanding)

The pipeline runs as one tokio task fed by two PipeWire capture streams:

```
Mic stream (48 kHz)          System stream (48 kHz, sink monitor)
      ↓ raw mono chunks             ↓ raw mono chunks
┌────────────────────────────────────────────────────────────────┐
│  AudioPipeline::run  (parley-core/src/audio/pipeline.rs) │
│   1. AudioMixerRingBuffer aligns both sources by absolute      │
│      sample position into 50 ms windows                        │
│   2. AEC3 (aec.rs) subtracts the system window from the mic    │
│   3. Mic-only enhancement: 80 Hz high-pass → capped, smoothed  │
│      EBU R128 loudness normalisation → soft clip               │
│   4. Per-source VAD (vad.rs, sherpa silero) → 16 kHz speech    │
│      segments tagged Microphone / System                        │
│   5. Stereo interleave: mic = left, system = right             │
└──────────┬───────────────────────────────┬─────────────────────┘
           ↓                               ↓
   Transcription worker            IncrementalAudioSaver
   (transcription/worker.rs,       (raw f32 PCM checkpoints every
    whisper-rs, serial, ordered)    30 s → single AAC encode at stop)
```

**Key points**: there is no mixing or ducking; the two sources stay separable
in the recording. The capture callback (`AudioCapture::process_audio_data`)
runs on PipeWire's real-time thread and only downmixes and forwards — all DSP
happens in the pipeline task. Whisper receives the VAD's 16 kHz output, which
is produced by a stateful windowed-sinc downsampler and zero-padded to at
least 1 s before decoding.

### Audio Architecture: Native PipeWire Capture

**Context**: Linux audio input was rewritten to talk to PipeWire directly, replacing the previous cpal-ALSA + `pactl` + `PIPEWIRE_NODE`-env-var stack. See the module doc comment at the top of `audio/pw/mod.rs` for the rationale.

```
parley-core/src/audio/
├── devices/                    # Device model + PipeWire-backed discovery
│   ├── discovery.rs           # list_audio_devices, trigger_audio_permission
│   └── configuration.rs       # AudioDevice, DeviceType
├── pw/                         # Native PipeWire capture layer (mic + system audio)
├── pipeline.rs                 # Audio mixing and VAD processing
├── device_detection.rs         # Bluetooth vs wired classification for adaptive buffering
├── hardware_detector.rs        # GPU/perf tier detection
├── recording_manager.rs        # High-level recording coordination
├── recording_service.rs        # start/stop/pause orchestration, called directly from parley-gpui
├── recording_saver.rs          # Audio file writing
├── import.rs                   # Import external audio files as new meetings
├── retranscription.rs          # Re-process stored audio with different settings
└── transcription/               # Provider abstraction, engine management, worker pool
```

**When working on audio features**:
- Device detection issues → `devices/discovery.rs` or `devices/configuration.rs`
- Capture issues (mic/system audio) → `pw/`
- Mixing/processing problems → `pipeline.rs`
- Recording workflow → `recording_manager.rs`

### Rust core ↔ GPUI shell (in-process, no IPC)

**Call pattern** (shell → core): the shell calls core functions directly,
usually from a GPUI view's event handler, spawned onto the app's async
executor via `cx.spawn(...)`:

```rust
// parley-gpui/src/views/recording/logic.rs (illustrative)
let pool = AppServices::global(cx).pool();
let sink = AppServices::global(cx).sink.clone();
cx.spawn(async move |_, _| {
    parley_core::audio::recording_service::start_recording(pool, sink, mic, system, name).await
}).detach();
```

**Event pattern** (core → shell): core code emits through the injected
`SharedEventSink`, which the shell wired to `core_events::GpuiSink`:

```rust
// parley-core: emit a transcript update
sink.emit_event("transcript-update", &TranscriptUpdate { text, timestamp, .. })?;
```

```rust
// parley-gpui: a view subscribes to CoreEvents and decodes the payload it cares about
cx.subscribe(&core_events, |this, _, event: &CoreEvent, cx| {
    if event.name == "transcript-update" {
        if let Some(update) = event.decode::<TranscriptUpdate>() { /* ... */ }
    }
});
```

### Whisper Model Management

**Model Storage Location**: `~/.local/share/io.github.hankanman.Parley/models/` (both dev and production — resolved via `paths::app_data_dir()`).

**Model Loading** (parley-core/src/whisper_engine/whisper_engine.rs):
```rust
pub async fn load_model(&self, model_name: &str) -> Result<()> {
    // Automatically detects GPU capabilities (CUDA/Vulkan)
    // Falls back to CPU if GPU unavailable
}
```

**GPU Acceleration**:
- CUDA (NVIDIA), Vulkan (AMD/Intel), or CPU fallback
- Configure via Cargo features: `--features cuda`, `--features vulkan` (auto-selected by `build.sh`/`dev.sh`)

## Critical Development Patterns

### 1. Audio Buffer Management

**Ring Buffer Alignment** (pipeline.rs):
- Mic and system chunks arrive asynchronously; each carries a per-source
  sample position (`timestamp` = samples sent / 48 000)
- `AudioMixerRingBuffer` pairs the two sources by absolute position into
  50 ms windows; a source lagging more than 500 ms yields a zero gap and its
  late data is dropped, so pairing never drifts
- A never-opened source (mic-only or system-only recording) is treated as
  permanent silence via `set_expected_sources`
- The trailing partial window is zero-padded and flushed at stop

### 2. Thread Safety and Async Boundaries

**Recording State** (recording_state.rs):
```rust
pub struct RecordingState {
    is_recording: Arc<AtomicBool>,
    audio_sender: Arc<RwLock<Option<mpsc::UnboundedSender<AudioChunk>>>>,
    // ...
}
```

**Key Pattern**: Use `Arc<RwLock<T>>` for shared state across async tasks, `Arc<AtomicBool>` for simple flags.

### 3. Error Handling and Logging

**Performance-Aware Logging** (lib.rs):
```rust
#[cfg(debug_assertions)]
macro_rules! perf_debug {
    ($($arg:tt)*) => { log::debug!($($arg)*) };
}

#[cfg(not(debug_assertions))]
macro_rules! perf_debug {
    ($($arg:tt)*) => {};  // Zero overhead in release builds
}
```

**Usage**: Use `perf_debug!()` and `perf_trace!()` for hot-path logging that should be eliminated in production.

### 4. GPUI State Management

**`AppServices`** (`parley-gpui/src/app_state.rs`) is the process-wide global
(`cx.global::<AppServices>()`), holding:
- `io: Io` — the Tauri-free async runtime glue (`runtime.rs`)
- `sink: SharedEventSink` — where core code emits events
- `core_events: Entity<CoreEvents>` — what views subscribe to for updates
- `db: DbSlot` — `Arc<RwLock<Option<DatabaseManager>>>`, `None` until onboarding creates a database, filled in later without a restart
- `builtin_manager: ModelManagerState` — the shared summary model-manager state

**Pattern**: a view calls a core function via `cx.spawn(...)`, which reads/writes through `AppServices::global(cx)` → core emits events on the shared sink → `CoreEvents` re-emits them as GPUI events → subscribed views update their own state and call `cx.notify()`.

**Meeting row ownership** (issue #57 slice 2): Rust owns the `meetings` row's
whole lifecycle, not just its live-recording phase. `start_recording*`
inserts the row (status `"recording"`) and mints the `meeting_id` included in
`recording-started`/`recording-stopped`; the `transcript-update` listener
upserts each segment to SQLite as it arrives via a batched writer
(`audio::transcript_db_writer`); `stop_recording` finalises the row
(`"completed"`, or `"interrupted"` on a fatal-error stop), and a startup
sweep marks any row still `"recording"` after a crash `"interrupted"`.
Recovery of an interrupted meeting is a database query
(`list_interrupted_meetings` / `recover_meeting`), surfaced by
`parley-gpui/src/recovery.rs`.

## Common Development Tasks

### Adding a New Feature

1. Put the logic in `parley-core` as a plain function (taking a
   `SharedEventSink` / `SqlitePool` if it emits or touches the DB):
   ```rust
   pub async fn do_thing(pool: SqlitePool, arg: String) -> Result<String> {
       // ...
   }
   ```
2. Call it from a `parley-gpui` view, usually via `cx.spawn(...)`:
   ```rust
   let pool = AppServices::global(cx).pool();
   cx.spawn(async move |this, cx| {
       let result = parley_core::my_module::do_thing(pool, arg).await;
       this.update(cx, |this, cx| { /* apply result to view state */ cx.notify(); })
   }).detach();
   ```
3. If the view needs to react to something happening elsewhere (e.g. a
   recording event), subscribe to `AppServices::global(cx).core_events`
   instead of polling.

### Modifying Audio Pipeline Behavior

**Location**: `parley-core/src/audio/pipeline.rs`

Key components:
- `AudioCapture`: minimal real-time capture callback (downmix + forward only)
- `AudioMixerRingBuffer`: positional mic + system alignment into 50 ms windows
- `MicEchoCanceller` (aec.rs): WebRTC AEC3 on the aligned mic window
- `HighPassFilter` / `LoudnessNormalizer` (audio_processing.rs): mic chain, after AEC
- `ContinuousVadProcessor` (vad.rs): per-source VAD + 48→16 kHz downsampling
- `AudioPipelineManager`: starts/stops the pipeline task and its channels

**Testing Audio Changes**:
```bash
# Enable verbose audio logging
RUST_LOG=parley_core::audio=debug ./dev.sh

# Monitor audio metrics in real-time
# Check Developer Console in the app (Ctrl+Shift+I)
```

## Testing and Debugging

### Debugging

**Enable Rust Logging**:
```bash
RUST_LOG=debug ./dev.sh
```

**Developer Tools**:
- Open DevTools: `Ctrl+Shift+I`
- Console Toggle: Built into app UI (console icon)
- View Rust logs: Check terminal output

### Audio Pipeline Debugging

**Key Metrics** (emitted by pipeline):
- Buffer sizes (mic/system)
- Mixing window count
- VAD detection rate
- Dropped chunk warnings

**Monitor via Developer Console**: The app includes real-time metrics display when recording.

## Platform Notes

Linux is the only supported platform (see [Repository-Specific Conventions](#repository-specific-conventions)).

- **Audio Capture**: Native PipeWire (`audio/pw/`) for both microphone and system audio — no virtual device or loopback trick needed
- **GPU**: CUDA (NVIDIA) or Vulkan (AMD/Intel) via Cargo features, CPU fallback otherwise
- **Dependencies**: Requires cmake, llvm, libomp — see [docs/building_in_linux.md](docs/building_in_linux.md)

## Performance Optimization Guidelines

### Audio Processing
- Use `perf_debug!()` / `perf_trace!()` for hot-path logging (zero cost in release)
- Batch audio metrics using `AudioMetricsBatcher` (pipeline.rs)
- Pre-allocate buffers with `AudioBufferPool` (buffer_pool.rs)
- VAD filtering reduces Whisper load by ~70% (only processes speech)

### Whisper Transcription
- **Model Selection**: Balance accuracy vs speed
  - Development: `base` or `small` (fast iteration)
  - Production: `medium` or `large-v3` (best quality)
- **GPU Acceleration**: 5-10x faster than CPU; the backend is chosen by the
  compiled Cargo feature (`hardware_detector.rs`), not by probing libraries
- **Live vs batch**: the transcription worker holds `whisper_engine::lease`
  while recording; import / retranscription / auto-refine wait on it before
  changing the loaded model

### GPUI Performance
- Views subscribe to `CoreEvents` instead of polling core state
- Transcript rendering virtualized for large meetings
- Audio level monitoring throttled to 60fps

## Important Constraints and Gotchas

1. **Audio Chunk Size**: Pipeline expects consistent 48kHz sample rate. Resampling happens at capture time.

2. **Audio Capture**: Mic + system audio both go through native PipeWire (`audio/pw/`) — no virtual device or exclusive-mode juggling needed.

3. **Whisper Model Loading**: Models are loaded once and cached. Changing models requires app restart or manual unload/reload.

4. **No external server**: meeting persistence, transcription, summary
   generation all happen inside the `parley-gpui` process. The old
   `backend/` FastAPI dir and the Tauri/Next.js shell were both deleted; if
   you see references to `:5167`, `frontend/src-tauri`, or `tauri::command`
   in code, they're stale (or, in doc comments, deliberately historical).

5. **File Paths**: Resolve directories with `parley_core::paths::app_data_dir()` — never hardcode paths.

6. **Audio Permissions**: Request microphone permission early; PipeWire handles system-audio routing without a separate OS-level screen-recording grant.

## Repository-Specific Conventions

- **Logging Format**: Uses detailed formatting with filename:line:function
- **Error Handling**: `anyhow::Result` throughout; user-facing errors get a friendly message before being surfaced in the UI
- **Naming**: Audio devices use "microphone" and "system" consistently (not "input"/"output")
- **Git Branches**:
  - `main`: Stable releases
  - `fix/*`: Bug fixes
  - `enhance/*`: Feature enhancements

## Key Files Reference

**Core Coordination**:
- [parley-gpui/src/main.rs](parley-gpui/src/main.rs) - Entry point, window setup, `AppServices` wiring, shutdown coordination
- [parley-gpui/src/app_state.rs](parley-gpui/src/app_state.rs) - `AppServices` global
- [parley-gpui/src/core_events.rs](parley-gpui/src/core_events.rs) - `GpuiSink` / `CoreEvents`, the core→UI event bridge
- [parley-core/src/audio/mod.rs](parley-core/src/audio/mod.rs) - Audio module exports

**Audio System**:
- [parley-core/src/audio/recording_manager.rs](parley-core/src/audio/recording_manager.rs) - Recording orchestration
- [parley-core/src/audio/pipeline.rs](parley-core/src/audio/pipeline.rs) - Audio mixing and VAD
- [parley-core/src/audio/recording_saver.rs](parley-core/src/audio/recording_saver.rs) - Audio file writing

**UI Components**:
- [parley-gpui/src/shell/mod.rs](parley-gpui/src/shell/mod.rs) - Main app chrome (sidebar, meeting list, recording bar)
- [parley-gpui/src/views/recording/mod.rs](parley-gpui/src/views/recording/mod.rs) - Recording home view
- [parley-gpui/src/views/meeting/](parley-gpui/src/views/meeting/) - Meeting detail (transcript, summary)

**Whisper Integration**:
- [parley-core/src/whisper_engine/whisper_engine.rs](parley-core/src/whisper_engine/whisper_engine.rs) - Whisper model management and transcription
