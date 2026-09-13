// audio/recording_service.rs
//
// Tauri-free recording orchestration: start (in its several parameterised
// forms), stop, pause, resume, and the fatal-error auto-stop path. Extracted
// from `recording_commands.rs` (Phase 0 WP-F) so this logic depends only on
// [`RecordingContext`] (an event sink + an optional DB pool) instead of a
// Tauri `AppHandle`. `recording_commands.rs` now holds only thin
// `#[tauri::command]` wrappers that build a context (and, for `start`, the
// two hooks below) from the `AppHandle` and delegate here.
//
// Every ordering guarantee documented on the functions in this module was
// hard-won against real bugs (issues #24, #25, #26, #57 slice 2) — preserve
// it exactly when touching this file.

use anyhow::Result;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

use super::devices::{AudioDevice, DeviceType};
use super::recording_phase::{self, RecordingPhase};
use super::recording_preferences::{self, RecordingPreferences};
use super::transcript_db_writer::TranscriptDbWriter;
use super::RecordingManager;
use crate::database::repositories::meeting::MeetingsRepository;
use crate::events::{EventSinkExt, SharedEventSink};

use super::transcription::{self, reset_speech_detected_flag, TranscriptUpdate};

// ============================================================================
// SHELL-FREE CONTEXT AND HOOKS
// ============================================================================

/// Everything the service needs from the shell, as plain values. Built once
/// per command invocation by `recording_commands.rs` from the live
/// `AppHandle`/`AppState`; a future non-Tauri shell builds one the same way
/// from whatever it uses instead.
#[derive(Clone)]
pub struct RecordingContext {
    pub sink: SharedEventSink,
    /// `None` when `AppState` hasn't been managed yet (e.g. a first-launch
    /// cold start racing a start/stop call before the frontend has created
    /// the database) — see `db_pool()`'s doc comment on the Tauri side.
    /// Every call site here treats a missing pool as "log and continue"
    /// rather than failing the recording.
    pub pool: Option<sqlx::SqlitePool>,
}

impl RecordingContext {
    pub fn new(sink: SharedEventSink, pool: Option<sqlx::SqlitePool>) -> Self {
        Self { sink, pool }
    }
}

/// A one-shot async hook returning `T`, boxed so it can cross an `async fn`
/// boundary without infecting the service with a generic Tauri `Runtime`
/// parameter.
type StartHook<T> = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = T> + Send>> + Send>;

/// Shell-side calls `start()` needs at specific points mid-flow. Both
/// underlying functions (`transcription::validate_transcription_model_ready`,
/// `speaker_diarization::service::try_init_for_recording`) are Tauri-free
/// (`Option<&SqlitePool>`), so `recording_commands::build_start_hooks` builds
/// these by closing over the pool already resolved into `RecordingContext`
/// rather than an `AppHandle`. Kept as hooks (rather than called directly
/// here) so this module still doesn't need to know how the pool is sourced.
pub struct StartHooks {
    /// Currently `transcription::validate_transcription_model_ready`.
    pub validate_transcription_model: StartHook<Result<(), String>>,
    /// Currently `speaker_diarization::service::try_init_for_recording`.
    pub init_speaker_diarizer: StartHook<Result<bool, String>>,
}

/// Build the standard [`StartHooks`] every shell wires up: both underlying
/// functions (`transcription::validate_transcription_model_ready`,
/// `speaker_diarization::service::try_init_for_recording`) are already
/// Tauri-free (`Option<&SqlitePool>`), so this just closes over `pool` for
/// each. Extracted here (rather than duplicated per shell) so the Tauri
/// command layer and the GPUI shell build byte-for-byte the same hooks; a
/// shell only needs this when it doesn't want to build custom hooks itself.
pub fn default_start_hooks(pool: Option<sqlx::SqlitePool>) -> StartHooks {
    let pool_for_validate = pool.clone();
    let pool_for_diarizer = pool;
    StartHooks {
        validate_transcription_model: Box::new(move || {
            Box::pin(async move {
                transcription::validate_transcription_model_ready(pool_for_validate.as_ref())
                    .await
            })
        }),
        init_speaker_diarizer: Box::new(move || {
            Box::pin(async move {
                crate::speaker_diarization::service::try_init_for_recording(
                    pool_for_diarizer.as_ref(),
                )
                .await
                .map_err(|e| e.to_string())
            })
        }),
    }
}

// ============================================================================
// GLOBAL STATE
// ============================================================================

// Global recording manager and transcription task to keep them alive during recording
static RECORDING_MANAGER: Mutex<Option<RecordingManager>> = Mutex::new(None);
static TRANSCRIPTION_TASK: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Batched SQLite writer for the current session's live transcript segments
/// (issue #57 slice 2), and its task handle. Started once `start()` has a
/// `meeting_id` and a DB pool; shut down (sender dropped, task awaited)
/// partway through `stop()`, after the transcription drain has published
/// every tail segment to the transcript bus.
static TRANSCRIPT_DB_WRITER: Mutex<Option<TranscriptDbWriter>> = Mutex::new(None);
static TRANSCRIPT_DB_WRITER_TASK: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Best-effort: mark `meeting_id`'s row "interrupted" (issue #57 slice 2).
/// Used on every abnormal stop path — a fatal recording error, or the audio
/// streams themselves failing to stop — so a row never lingers at
/// "recording" while the app keeps running (only a real crash needs the
/// startup sweep; this covers every case that doesn't crash the process).
/// Never fails the caller's own error path: logs and returns either way.
async fn mark_meeting_interrupted_best_effort(
    pool: Option<&sqlx::SqlitePool>,
    meeting_id: &Option<String>,
) {
    let Some(mid) = meeting_id else { return };
    let Some(pool) = pool else {
        warn!(
            "No DB pool available; meeting {} row was not marked interrupted",
            mid
        );
        return;
    };
    match MeetingsRepository::mark_meeting_interrupted(pool, mid).await {
        Ok(true) => info!("DB: meeting {} row marked interrupted", mid),
        Ok(false) => warn!("DB: meeting {} row not found to mark interrupted", mid),
        Err(e) => warn!("DB: failed to mark meeting {} row interrupted: {}", mid, e),
    }
}

/// Held from the first line of `start()` until it returns. `is_recording()`
/// only flips true once the manager is stored — after several awaits (model
/// validation, preferences, device enumeration, PipeWire stream open) —
/// so without this two overlapping starts both pass the "already recording"
/// check and the second silently drops the first manager and its
/// un-finalised audio.
static START_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Held for the whole of `stop()` — `is_recording()` reads false ~100 ms in
/// (force-flush clears the state) while the transcription drain, model
/// unload and audio merge run for seconds to minutes. A start that sneaks in
/// during that window would take over the global manager slot and the tail
/// of the stop would then finalise (and drop) the *new* recording.
static STOP_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// RAII flag holder: clears the flag on every exit path, including `?`.
struct PhaseGuard(&'static AtomicBool);

impl PhaseGuard {
    /// Atomically claim `flag`; `None` if it is already held.
    fn try_acquire(flag: &'static AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| PhaseGuard(flag))
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// True while a stop is still draining / finalising a previous recording.
pub fn is_stop_in_progress() -> bool {
    STOP_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Move the canonical recording-state machine (`audio::recording_phase`) to
/// `phase` and broadcast the resulting snapshot as `recording-state`,
/// merging in the live duration/queue-depth data this module owns
/// (`RECORDING_MANAGER`, the transcription queue). This is the single place
/// every phase transition in this file goes through.
fn emit_phase(sink: &dyn crate::events::EventSink, phase: RecordingPhase) {
    let (active_duration_secs, total_pause_secs) = {
        let guard = RECORDING_MANAGER.lock().unwrap();
        match guard.as_ref() {
            Some(m) => (
                m.get_active_recording_duration(),
                m.get_total_pause_duration(),
            ),
            None => (None, 0.0),
        }
    };
    let chunks_in_queue = transcription::queue_depth();
    recording_phase::set_phase(sink, phase, active_duration_secs, total_pause_secs, chunks_in_queue);
}

/// Shared entry check for every start path. Refuses while another start is
/// already running or a previous stop is still finalising, then re-checks the
/// live recording flag. Returns the guard that must be held until the start
/// call returns.
async fn begin_start_phase() -> Result<PhaseGuard, String> {
    let guard = PhaseGuard::try_acquire(&START_IN_PROGRESS)
        .ok_or_else(|| "Recording start already in progress".to_string())?;

    if is_stop_in_progress() {
        return Err(
            "The previous recording is still being finalised. Please wait a moment and try again."
                .to_string(),
        );
    }

    let current_recording_state = is_recording().await;
    info!("🔍 recording state check: {}", current_recording_state);
    if current_recording_state {
        return Err("Recording already in progress".to_string());
    }

    Ok(guard)
}

/// Snapshot the transcript segments accumulated so far in the current recording
/// session — in memory, before they're persisted on stop. Empty when nothing is
/// recording. Used by the live action-item extractor, which has no `meeting_id`
/// or DB rows to read during a recording.
pub fn snapshot_segments() -> Vec<crate::audio::common::TranscriptSegment> {
    RECORDING_MANAGER
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|m| m.get_transcript_segments()))
        .unwrap_or_default()
}

/// Subscribe this session's persistence to the transcript bus: every finished
/// segment is enqueued for SQLite (issue #57 slice 2) and handed to the
/// recording manager. `meeting_id` is captured by value so each segment
/// carries it without touching the (frequently swapped) `RECORDING_MANAGER`
/// lock to look it up. Removed by `transcript_bus::unsubscribe` in `stop()`
/// once the transcription worker has drained.
fn subscribe_transcript_persistence(meeting_id: String) {
    let replaced_stale = super::transcript_bus::subscribe(move |update: &TranscriptUpdate| {
        let segment = crate::audio::recording_saver::TranscriptSegment {
            id: format!("seg_{}", update.sequence_id),
            text: update.text.clone(),
            timestamp: None,
            audio_start_time: Some(update.audio_start_time),
            audio_end_time: Some(update.audio_end_time),
            duration: Some(update.duration),
            display_time: Some(update.timestamp.clone()), // Use wall-clock timestamp for display
            confidence: Some(update.confidence),
            sequence_id: Some(update.sequence_id),
            speaker: update.speaker.clone(),
            voice_profile_id: update.voice_profile_id.clone(),
            source: Some(update.source.clone()),
        };

        // Persist to SQLite via the batched writer — a cheap channel send,
        // non-blocking, independent of the RecordingSaver copy saved below.
        if let Ok(writer_guard) = TRANSCRIPT_DB_WRITER.lock() {
            if let Some(writer) = writer_guard.as_ref() {
                writer.enqueue(meeting_id.clone(), segment.clone());
            }
        }

        if let Ok(manager_guard) = RECORDING_MANAGER.lock() {
            if let Some(manager) = manager_guard.as_ref() {
                manager.add_transcript_segment(segment);
            } else {
                // Manager is briefly out of the slot (e.g. mid force-flush
                // during stop) — buffer instead of dropping; replayed once
                // it's back (issue #25).
                PENDING_SEGMENT_BUFFER.lock().unwrap().push(segment);
            }
        }
    });
    // A stale subscriber means a previous session never reached its
    // unsubscribe (error-path stop, crash recovery); it has just been
    // replaced, otherwise every segment would be persisted twice.
    if replaced_stale {
        warn!("⚠️ Replaced a stale transcript subscriber from a previous session");
    }
    info!("✅ Transcript persistence subscribed for this session");
}

/// Transcript segments that arrived on the transcript bus
/// while `RECORDING_MANAGER` was briefly empty — e.g. during the
/// `stop_streams_and_force_flush().await` call in `stop()`, which
/// takes the manager out of the slot for its duration. Without this they'd
/// be silently dropped (issue #25): buffered here instead, then replayed
/// into the manager via `replay_buffered_segments` as soon as it's back.
static PENDING_SEGMENT_BUFFER: Mutex<Vec<crate::audio::recording_saver::TranscriptSegment>> =
    Mutex::new(Vec::new());

/// Called from `RecordingState::report_error` (via `set_error_callback`)
/// exactly once per session on a fatal error. Emits a user-facing
/// `recording-error` event, then runs the exact same `stop()` flow the Stop
/// button runs — draining transcription, finalising audio, releasing the
/// manager and emitting `recording-stopped` — so a fatal error can never
/// leave streams/pipeline/worker dangling with the user's own Stop button
/// reduced to a silent no-op (issue #24).
///
/// `report_error` (and therefore this) can run on PipeWire's own
/// event/real-time thread, not a tokio task (see `audio/stream.rs`'s
/// `PwStreamEvent` handler) — so this spawns onto `runtime`, a
/// `tokio::runtime::Handle` captured up front in `start()` while a tokio
/// context was definitely current, rather than `tokio::spawn` (which
/// panics off-runtime) or Tauri's own async runtime.
fn spawn_fatal_error_stop(
    ctx: RecordingContext,
    runtime: &tokio::runtime::Handle,
    error: &super::recording_state::AudioError,
) {
    warn!(
        "Fatal recording error ({}); auto-stopping via the full stop flow",
        error.user_message()
    );
    let _ = ctx.sink.emit_event("recording-error", error.user_message());
    recording_phase::set_error_message(Some(error.user_message().to_string()));
    emit_phase(ctx.sink.as_ref(), RecordingPhase::Error);

    // `stop()` doesn't actually use `save_path` for anything beyond ensuring
    // its parent directory exists (see the Tauri `stop_recording` command,
    // which is the caller for a user-initiated stop); build one the same way
    // the tray's stop handlers do.
    let save_path = crate::paths::app_data_dir()
        .map(|dir| {
            let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
            dir.join(format!("recording-error-{}.wav", timestamp))
                .to_string_lossy()
                .to_string()
        })
        .unwrap_or_else(|_| "recording-error.wav".to_string());

    runtime.spawn(async move {
        if let Err(e) = stop(ctx, RecordingArgs { save_path }).await {
            error!("Auto-stop after fatal recording error failed: {}", e);
        }
    });
}

/// Drain `buffer` into `manager`, in arrival order. Pure function (no
/// statics touched) so it's unit-testable on its own — see the `tests`
/// module at the bottom of this file.
fn replay_buffered_segments(
    buffer: &mut Vec<crate::audio::recording_saver::TranscriptSegment>,
    manager: &RecordingManager,
) {
    for segment in buffer.drain(..) {
        manager.add_transcript_segment(segment);
    }
}

// ============================================================================
// PUBLIC TYPES
// ============================================================================

/// Post-meeting improvement passes, run after a recording's row and
/// folder exist (shells call this in the background once `recording-stopped`
/// arrives with a `meeting_id` and `folder_path`):
/// 1. speaker refinement over the meeting's stored embeddings, then
/// 2. the auto-refine re-transcription pass (skips itself when no
///    higher-accuracy model is downloaded).
///
/// Never fails the caller: a refinement error is logged and leaves the
/// labels as recorded, and never blocks the transcription pass — they're
/// independent improvements to the same meeting.
pub async fn post_meeting_refine(
    ctx: RecordingContext,
    meeting_id: String,
    meeting_folder_path: String,
) {
    match ctx.pool.as_ref() {
        Some(pool) => {
            if let Err(e) = crate::speaker_diarization::service::refine_and_persist(
                &ctx.sink,
                pool,
                &meeting_id,
            )
            .await
            {
                log::warn!(
                    "Speaker refinement failed for meeting {}: {} (transcript labels left as recorded)",
                    meeting_id,
                    e
                );
            }
        }
        None => log::warn!(
            "No DB pool available; skipping speaker refinement for meeting {}",
            meeting_id
        ),
    }

    crate::audio::retranscription::spawn_auto_refine(
        ctx.sink.clone(),
        ctx.pool.clone(),
        meeting_id,
        meeting_folder_path,
    );
}

#[derive(Debug, Deserialize)]
pub struct RecordingArgs {
    pub save_path: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct TranscriptionStatus {
    pub chunks_in_queue: usize,
    pub is_processing: bool,
    pub last_activity_ms: u64,
}

/// Parameters for `start()`. `mic_device_name`/`system_device_name` both
/// `None` selects saved-preference-or-default device resolution (the
/// "default devices" flow); either being `Some` selects the "specific
/// devices" flow, which uses the given ids directly. This mirrors the
/// dispatch the Tauri command layer used to do between
/// `start_recording_with_meeting_name` and
/// `start_recording_with_devices_and_meeting` — genuinely the same flow
/// modulo how devices are resolved, so it's one function parameterised on
/// that instead of two near-duplicates.
pub struct StartRequest {
    pub mic_device_name: Option<String>,
    pub system_device_name: Option<String>,
    pub meeting_name: Option<String>,
}

// ============================================================================
// RECORDING SERVICE
// ============================================================================

/// Start a recording session. See [`StartRequest`] for the two device
/// resolution flows this covers, and [`StartHooks`] for the two
/// still-AppHandle-shaped calls it needs mid-flow.
pub async fn start(
    ctx: RecordingContext,
    hooks: StartHooks,
    req: StartRequest,
) -> Result<(), String> {
    let use_default_device_resolution =
        req.mic_device_name.is_none() && req.system_device_name.is_none();

    if use_default_device_resolution {
        info!(
            "Starting recording with default devices, meeting: {:?}",
            req.meeting_name
        );
    } else {
        info!(
            "Starting recording with specific devices: mic={:?}, system={:?}, meeting={:?}",
            req.mic_device_name, req.system_device_name, req.meeting_name
        );
    }

    // Claim the start phase (rejects overlapping starts and starts during a
    // still-finalising stop) and check the live recording flag.
    let _start_phase = begin_start_phase().await?;

    // The canonical state machine's first transition: Idle -> Starting,
    // before any of the awaits below (model validation, preferences, device
    // enumeration, PipeWire stream open) that can take a noticeable moment.
    emit_phase(ctx.sink.as_ref(), RecordingPhase::Starting);

    // Validate that transcription models are available before starting recording
    info!("🔍 Validating transcription model availability before starting recording...");
    if let Err(validation_error) = (hooks.validate_transcription_model)().await {
        error!("Model validation failed: {}", validation_error);

        // Emit error event for frontend - actionable: false to show toast instead of modal
        // (download progress is already shown in top-right toast)
        let _ = ctx.sink.emit_event(
            "transcription-error",
            &serde_json::json!({
                "error": validation_error,
                "userMessage": format!("Recording cannot start: {}", validation_error),
                "actionable": false
            }),
        );

        emit_phase(ctx.sink.as_ref(), RecordingPhase::Idle);
        return Err(validation_error);
    }
    info!("✅ Transcription model validation passed");

    info!("🚀 Starting async recording initialization");

    // Create new recording manager
    let mut manager = RecordingManager::new();

    // Load recording preferences for auto_save / streaming_partials (and,
    // for the default-devices flow, preferred mic/system device ids).
    let preferences = match recording_preferences::load_recording_preferences(ctx.pool.clone())
        .await
    {
        Ok(prefs) => {
            info!(
                "📋 Loaded recording preferences: auto_save={}, preferred_mic={:?}, preferred_system={:?}",
                prefs.auto_save, prefs.preferred_mic_device, prefs.preferred_system_device
            );
            prefs
        }
        Err(e) => {
            warn!(
                "Failed to load recording preferences, using defaults: {}",
                e
            );
            RecordingPreferences::default()
        }
    };
    let auto_save = preferences.auto_save;
    let streaming_partials = preferences.streaming_partials;

    let (microphone_device, system_device) = if use_default_device_resolution {
        // ========================================================================
        // DEVICE RESOLUTION: saved preference (PipeWire node id) → default.
        // A stale saved id (device unplugged/renamed) falls back to default.
        // ========================================================================
        let known_ids: Vec<String> = match super::devices::list_audio_devices().await {
            Ok(devices) => devices.into_iter().map(|d| d.id).collect(),
            Err(e) => {
                warn!("Could not enumerate devices ({}); trusting saved ids", e);
                Vec::new()
            }
        };
        let resolve = |pref: Option<String>, role: &str| -> String {
            match pref {
                Some(id) if known_ids.is_empty() || known_ids.iter().any(|k| *k == id) => {
                    info!("✅ Using preferred {}: '{}'", role, id);
                    id
                }
                Some(id) => {
                    warn!(
                        "⚠️ Preferred {} '{}' not present; falling back to default",
                        role, id
                    );
                    "default".to_string()
                }
                None => {
                    info!("🎧 No {} preference set, using system default", role);
                    "default".to_string()
                }
            }
        };

        (
            Some(Arc::new(AudioDevice::new(
                resolve(preferences.preferred_mic_device, "microphone"),
                DeviceType::Input,
            ))),
            Some(Arc::new(AudioDevice::new(
                resolve(preferences.preferred_system_device, "system audio"),
                DeviceType::Output,
            ))),
        )
    } else {
        // Which stream a device drives is determined by the parameter it
        // arrives in — ids are opaque PipeWire node names (or "default").
        (
            req.mic_device_name
                .as_ref()
                .map(|id| Arc::new(AudioDevice::new(id.clone(), DeviceType::Input))),
            req.system_device_name
                .as_ref()
                .map(|id| Arc::new(AudioDevice::new(id.clone(), DeviceType::Output))),
        )
    };

    // Always ensure a meeting name is set so incremental saver initializes
    let effective_meeting_name = req.meeting_name.clone().unwrap_or_else(|| {
        // Example: Meeting 2025-10-03_08-25-23
        let now = chrono::Local::now();
        format!("Meeting {}", now.format("%Y-%m-%d_%H-%M-%S"))
    });
    manager.set_meeting_name(Some(effective_meeting_name.clone()));

    // Mint the meeting_id up front (issue #57 slice 2): kept for the whole
    // session so live transcript upserts, `recording-started`/
    // `recording-stopped`, and the `meetings` row itself all agree on one
    // id. Minting it doesn't depend on the DB insert below succeeding — a
    // failed insert still leaves every other use of this id well-defined
    // (transcript upserts just won't have a row to match against).
    let meeting_id = uuid::Uuid::new_v4().to_string();
    manager.set_meeting_id(Some(meeting_id.clone()));

    // Set up error callback: on a fatal error (report_error only calls this
    // once per session — see recording_state::report_error) tell the user
    // and run the exact same full stop flow the Stop button runs, so
    // streams/pipeline/worker/save/`recording-stopped` all still happen
    // instead of leaving everything dangling (issue #24).
    //
    // Captured here, while we're definitely inside a tokio context, so the
    // callback (which can fire from PipeWire's own thread) has a runtime
    // handle to spawn the stop flow onto.
    let runtime_handle = tokio::runtime::Handle::current();
    let ctx_for_error = ctx.clone();
    manager.set_error_callback(move |error| {
        spawn_fatal_error_stop(ctx_for_error.clone(), &runtime_handle, error);
    });

    // Start recording with resolved devices
    let transcription_receiver = match manager
        .start_recording(microphone_device, system_device, auto_save, streaming_partials)
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            emit_phase(ctx.sink.as_ref(), RecordingPhase::Idle);
            return Err(format!("Failed to start recording: {}", e));
        }
    };

    // Recording itself only needs raw PCM, but finalizing it into a
    // playable file needs ffmpeg — warn (once, non-fatal) rather than
    // blocking start on it.
    if super::ffmpeg::find_ffmpeg_path().is_none() {
        warn!("FFmpeg not found; recording will be kept as PCM checkpoints until it is installed");
        let _ = ctx.sink.emit_event(
            "transcription-warning",
            "FFmpeg is not installed; the recording will be kept as PCM checkpoints until it is.",
        );
    }

    // Claim the streaming-partial receiver before the manager is moved into
    // the global (None when partials are disabled).
    let partial_receiver = manager.take_partial_receiver();

    // Record the meeting name/folder for the canonical snapshot while we
    // still own `manager` locally (the folder is created inside
    // `start_recording` above, via the recording saver's accumulation).
    let folder_path_for_phase = manager
        .get_meeting_folder()
        .map(|p| p.to_string_lossy().to_string());
    recording_phase::set_meeting_info(
        Some(effective_meeting_name.clone()),
        folder_path_for_phase.clone(),
        Some(meeting_id.clone()),
    );

    // Create the `meetings` row now, status "recording" (issue #57 slice 2)
    // — Rust owns the row's whole lifecycle from here, so a crash mid-
    // recording leaves a real "recording"-status row for the next startup's
    // sweep to mark "interrupted" instead of nothing at all. Best-effort:
    // the recording itself must never fail because this insert did.
    if let Some(pool) = ctx.pool.clone() {
        if let Err(e) = MeetingsRepository::create_recording_meeting(
            &pool,
            &meeting_id,
            &effective_meeting_name,
            folder_path_for_phase.as_deref(),
        )
        .await
        {
            warn!(
                "Failed to create meeting row {} at recording start (continuing without it): {}",
                meeting_id, e
            );
        }
        // Batched writer for live transcript-segment upserts, regardless of
        // whether the insert above succeeded — see its own doc comment.
        let (writer, writer_task) = TranscriptDbWriter::start(pool);
        *TRANSCRIPT_DB_WRITER.lock().unwrap() = Some(writer);
        let stale_task = TRANSCRIPT_DB_WRITER_TASK.lock().unwrap().replace(writer_task);
        if let Some(stale_task) = stale_task {
            // Same defensive cleanup as the stale transcript subscriber
            // below: a previous session's task should already be gone, but
            // never leave two writers racing against the same DB rows.
            stale_task.abort();
            warn!("⚠️ Aborted a stale transcript DB writer task from a previous session");
        }
    } else {
        warn!(
            "No DB pool available yet; meeting {} won't be persisted to SQLite live (transcripts.ndjson on disk is unaffected)",
            meeting_id
        );
    }

    // Store the manager globally to keep it alive
    {
        let mut global_manager = RECORDING_MANAGER.lock().unwrap();
        *global_manager = Some(manager);
    }

    // Reset speech detection flag for the new recording session. Recording
    // state itself is already tracked by `RecordingState` (set inside
    // `manager.start_recording()` above) — no separate flag to flip here.
    info!("🔍 Resetting SPEECH_DETECTED_EMITTED for new recording session");
    reset_speech_detected_flag(); // Reset for new recording session

    // Initialize the speaker diarizer for this session if the model is on disk.
    // Failure is non-fatal: recording proceeds with the "Speaker" placeholder.
    match (hooks.init_speaker_diarizer)().await {
        Ok(true) => info!("🗣️ Speaker diarization enabled for this session"),
        Ok(false) => info!("🗣️ Speaker diarization disabled (model not downloaded)"),
        Err(e) => warn!("Speaker diarizer init failed: {}", e),
    }

    // Start optimized parallel transcription task and store handle
    let task_handle = transcription::start_transcription_task(
        ctx.sink.clone(),
        ctx.pool.clone(),
        transcription_receiver,
    );
    {
        let mut global_task = TRANSCRIPTION_TASK.lock().unwrap();
        *global_task = Some(task_handle);
    }

    // Start the streaming-partial preview task (best-effort, additive to the
    // final path). Detached — it ends when the pipeline drops its sender.
    if let Some(rx) = partial_receiver {
        transcription::start_partial_decode_task(ctx.sink.clone(), rx);
    }

    // Persist every finished segment for this session (SQLite writer +
    // RecordingSaver copy) via the in-process transcript bus.
    subscribe_transcript_persistence(meeting_id.clone());

    // Emit success event. The device list in the payload intentionally
    // differs by flow: the default-devices flow always reports the generic
    // "Default Microphone"/"Default System Audio" labels (matching the
    // original `start_recording_with_meeting_name` behaviour) regardless of
    // which concrete device preference was resolved; the specific-devices
    // flow reports the ids the caller actually passed in.
    let devices = if use_default_device_resolution {
        [
            "Default Microphone".to_string(),
            "Default System Audio".to_string(),
        ]
    } else {
        [
            req.mic_device_name
                .clone()
                .unwrap_or_else(|| "Default Microphone".to_string()),
            req.system_device_name
                .clone()
                .unwrap_or_else(|| "Default System Audio".to_string()),
        ]
    };
    let message = if use_default_device_resolution {
        "Recording started successfully with parallel processing"
    } else {
        "Recording started with custom devices and parallel processing"
    };
    ctx.sink
        .emit_event(
            "recording-started",
            &serde_json::json!({
                "message": message,
                "devices": devices,
                "workers": 3,
                "meeting_id": meeting_id
            }),
        )?;

    // The canonical `recording-state` emit below drives the shell's tray
    // refresh (see `recording_commands.rs`'s `TrayRefreshingSink`) — no
    // separate tray call needed here.
    emit_phase(ctx.sink.as_ref(), RecordingPhase::Recording);

    info!("✅ Recording started successfully with async-first approach");

    Ok(())
}

/// Stop recording with optimized graceful shutdown ensuring NO transcript chunks are lost
pub async fn stop(ctx: RecordingContext, _args: RecordingArgs) -> Result<(), String> {
    info!(
        "🛑 Starting optimized recording shutdown - ensuring ALL transcript chunks are preserved"
    );

    // "Nothing to stop" is decided by whether a manager is actually present
    // (and no stop is already draining one) — NOT by `is_recording()`.
    // `report_error` (see recording_state.rs) no longer force-stops on a
    // fatal error, but even before that: `stop_streams_and_force_flush`
    // itself calls `state.cleanup()`, so `is_recording()` already reads
    // false while a manager is still present and mid-drain. Basing the
    // early return on the atomic made the user's own Stop button a silent
    // no-op in both cases (issue #24) — a manager in the slot, or a stop
    // already in progress, always means there's real work to finish here.
    {
        let manager_present = RECORDING_MANAGER.lock().unwrap().is_some();
        if !manager_present && !is_stop_in_progress() {
            info!("Recording was not active");
            return Ok(());
        }
    }

    // Hold the stop phase until this function returns so no start can take
    // over the manager slot while the drain / save below is still running.
    let _stop_phase = PhaseGuard::try_acquire(&STOP_IN_PROGRESS)
        .ok_or_else(|| "Recording stop already in progress".to_string())?;

    // Captured before the Stopping transition below moves the canonical
    // phase away from Error — distinguishes a fatal-error-triggered stop
    // (issue #57 slice 2: the meeting row should end up "interrupted") from
    // a normal user-initiated stop ("completed"). This function runs
    // unchanged for both — `spawn_fatal_error_stop` just calls it.
    let was_fatal_error = recording_phase::current_phase() == RecordingPhase::Error;

    // The meeting_id for this session, if any (issue #57 slice 2) — read
    // once, up front, while the manager is still definitely present; used at
    // the end of this function to finalise the `meetings` row.
    let meeting_id_for_stop: Option<String> = RECORDING_MANAGER
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get_meeting_id());

    // Canonical state machine: Recording/Paused/Error -> Stopping, as early
    // as possible in the stop flow.
    emit_phase(ctx.sink.as_ref(), RecordingPhase::Stopping);

    // Emit shutdown progress to frontend
    let _ = ctx.sink.emit_event(
        "recording-shutdown-progress",
        &serde_json::json!({
            "stage": "stopping_audio",
            "message": "Stopping audio capture...",
            "progress": 20
        }),
    );

    // Step 1: Stop audio capture immediately (no more new chunks). Take the
    // manager out just long enough to force-flush the pipeline, then put it
    // BACK in the global slot so the transcript bus subscriber can still reach
    // it while the worker drains the flushed tail below. `force_flush` calls
    // `state.cleanup()`, so `is_recording()` already reads false here even with
    // the manager present. It's taken out again for the final save after the
    // drain (Step 4).
    let manager = {
        let mut global_manager = RECORDING_MANAGER.lock().unwrap();
        global_manager.take()
    };

    let stop_result = if let Some(mut manager) = manager {
        // Use FORCE FLUSH to immediately process all accumulated audio - eliminates 30s delay!
        info!("🚀 Using FORCE FLUSH to eliminate pipeline accumulation delays");
        let result = manager.stop_streams_and_force_flush().await;
        // Replay any segments the transcript bus subscriber buffered while
        // the manager was out of RECORDING_MANAGER during the await above
        // (issue #25) — before putting the manager back, so nothing else
        // can observe it as "present but missing tail segments".
        {
            let mut pending = PENDING_SEGMENT_BUFFER.lock().unwrap();
            if !pending.is_empty() {
                info!(
                    "↩️ Replaying {} transcript segment(s) buffered during force-flush",
                    pending.len()
                );
                replay_buffered_segments(&mut pending, &manager);
            }
        }
        // Return the manager to the global slot for the drain window so the
        // bus subscriber can persist the tail segments transcribed below.
        *RECORDING_MANAGER.lock().unwrap() = Some(manager);
        result
    } else {
        warn!("No recording manager found to stop");
        Ok(())
    };

    match stop_result {
        Ok(_) => {
            info!("✅ Audio streams stopped successfully - no more chunks will be created");
            // Canonical state machine: Stopping -> Finalising — the force
            // flush is done, and the drain/save that follows below can take
            // seconds to minutes.
            emit_phase(ctx.sink.as_ref(), RecordingPhase::Finalising);
        }
        Err(e) => {
            error!("❌ Failed to stop audio streams: {}", e);
            recording_phase::set_error_message(Some(e.to_string()));
            emit_phase(ctx.sink.as_ref(), RecordingPhase::Error);
            mark_meeting_interrupted_best_effort(ctx.pool.as_ref(), &meeting_id_for_stop).await;
            return Err(format!("Failed to stop audio streams: {}", e));
        }
    }

    // NOTE: the transcript bus subscriber and the speaker diarizer are torn
    // down *after* the transcription drain below — not here. The force-flush
    // above only *queues* the tail segments; they're transcribed during the
    // drain and their segments must still reach the subscriber
    // (which persists them) and the diarizer (which attributes them). Removing
    // either now drops the final utterance(s) — and for a short recording whose
    // entire content is one un-closed utterance, that means zero saved
    // segments even though the live partials looked perfect.

    // Step 2: Signal transcription workers to finish processing ALL queued chunks
    let _ = ctx.sink.emit_event(
        "recording-shutdown-progress",
        &serde_json::json!({
            "stage": "processing_transcripts",
            "message": "Processing remaining transcript chunks...",
            "progress": 40
        }),
    );

    // Wait for transcription task with enhanced progress monitoring (NO TIMEOUT - we must process all chunks)
    let transcription_task = {
        let mut global_task = TRANSCRIPTION_TASK.lock().unwrap();
        global_task.take()
    };

    if let Some(mut task_handle) = transcription_task {
        info!("⏳ Waiting for ALL transcription chunks to be processed (draining the queue, no fixed cap)");

        // Issue #26: there is no hard cap on drain time any more as long as
        // the worker is making progress — only a *stall* (the queue depth
        // hasn't shrunk at all for 10 minutes) aborts the wait. This avoids
        // silently discarding a legitimate backlog just because it took
        // longer than some fixed budget to transcribe.
        const STALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(600);
        let shutdown_start = std::time::Instant::now();
        let mut last_progress_at = shutdown_start;
        let mut last_depth = transcription::queue_depth();

        let outcome = loop {
            tokio::select! {
                biased;
                res = &mut task_handle => {
                    break Some(res);
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(500)) => {
                    let depth = transcription::queue_depth();
                    if depth < last_depth {
                        last_progress_at = std::time::Instant::now();
                    }
                    last_depth = depth;

                    let elapsed = shutdown_start.elapsed().as_secs();
                    let _ = ctx.sink.emit_event(
                        "recording-shutdown-progress",
                        &serde_json::json!({
                            "stage": "processing_transcripts",
                            "message": format!(
                                "Processing transcripts... ({}s elapsed, {} chunk(s) queued)",
                                elapsed, depth
                            ),
                            "progress": 40,
                            "detailed": true,
                            "elapsed_seconds": elapsed,
                            "chunks_in_queue": depth
                        }),
                    );

                    if last_progress_at.elapsed() >= STALL_LIMIT {
                        warn!(
                            "⏱️ Transcription queue stalled at {} chunk(s) for {}s with no progress, aborting to prevent indefinite hang",
                            depth,
                            STALL_LIMIT.as_secs()
                        );
                        task_handle.abort();
                        break None;
                    }
                }
            }
        };

        match outcome {
            Some(Ok(())) => {
                info!("✅ ALL transcription chunks processed successfully - no data lost");
            }
            Some(Err(e)) => {
                warn!("⚠️ Transcription task completed with error: {:?}", e);
                // Continue anyway - the worker may have processed most chunks
            }
            None => {
                warn!("⏱️ Transcription drain stalled and was aborted, continuing shutdown (some chunks may be unprocessed)");
            }
        }
    } else {
        info!("ℹ️ No transcription task found to wait for");
    }

    // The worker has drained: the flushed tail segments are transcribed and
    // were published to the transcript bus synchronously as they finished,
    // so every one has already been persisted. Removing the subscriber
    // earlier than this would lose the last utterance(s) — see the note above.
    if super::transcript_bus::unsubscribe() {
        info!("✅ Transcript persistence unsubscribed (after transcription drain)");
    }

    // Shut down the transcript DB writer (issue #57 slice 2): every segment
    // this session enqueued has been sent by now (the subscriber above is
    // gone), so dropping the writer closes its channel — the task then
    // flushes whatever's left in its current batch and returns, which this
    // awaits (bounded) before moving on.
    {
        let writer = TRANSCRIPT_DB_WRITER.lock().unwrap().take();
        drop(writer);
        let task = TRANSCRIPT_DB_WRITER_TASK.lock().unwrap().take();
        if let Some(task) = task {
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), task).await {
                Ok(_) => info!("✅ Transcript DB writer task drained and finished"),
                Err(_) => warn!(
                    "⏱️ Transcript DB writer task still running after drain; abandoning wait (transcripts.ndjson on disk is unaffected)"
                ),
            }
        }
    }

    // The streaming-partial task ends on its own once the pipeline drops its
    // sender (it clears the overlay as it exits). Give it a moment, then
    // abort anything still running so a late partial can never re-populate
    // the overlay after `recording-stopped`.
    if let Some(partial_task) = transcription::take_partial_task_handle() {
        match tokio::time::timeout(tokio::time::Duration::from_secs(5), partial_task).await {
            Ok(_) => info!("✅ Streaming-partial task finished"),
            Err(_) => warn!("⏱️ Streaming-partial task still running after drain; aborting"),
        }
    }

    // Drop the speaker diarizer now (not before the drain) so a fresh session
    // starts with empty cluster IDs — the tail segments needed it above.
    crate::speaker_diarization::service::shutdown_for_recording();

    // Take the manager back out of the global slot now the tail is persisted;
    // it's needed (owned) for the final save in Step 4.
    let manager_for_cleanup = { RECORDING_MANAGER.lock().unwrap().take() };

    // Step 3: Whisper model stays resident across recordings by default
    // (#47) — reloading it at the start of every recording was the main
    // cost this shutdown path used to pay for no benefit, since the same
    // model is almost always used for the next recording too. Only unload
    // it here when the user has explicitly opted into freeing it between
    // recordings via `unload_model_after_recording` (see
    // `recording_preferences::should_unload_after_stop`); otherwise it's
    // left loaded and idle (no background work runs against it while no
    // recording or batch job is using it).
    let preferences = recording_preferences::load_recording_preferences(ctx.pool.clone())
        .await
        .unwrap_or_default();

    if recording_preferences::should_unload_after_stop(&preferences) {
        let _ = ctx.sink.emit_event(
            "recording-shutdown-progress",
            &serde_json::json!({
                "stage": "unloading_model",
                "message": "Unloading speech recognition model...",
                "progress": 70
            }),
        );

        info!("🧠 unload_model_after_recording is set — unloading Whisper model...");
        let engine_clone = {
            let engine_guard = crate::whisper_engine::models::WHISPER_ENGINE
                .lock()
                .unwrap();
            engine_guard.as_ref().cloned()
        };

        if let Some(engine) = engine_clone {
            let current_model = engine
                .get_current_model()
                .await
                .unwrap_or_else(|| "unknown".to_string());
            info!("Current Whisper model before unload: '{}'", current_model);

            if engine.unload_model().await {
                info!("✅ Whisper model '{}' unloaded successfully", current_model);
            } else {
                warn!("⚠️ Failed to unload Whisper model '{}'", current_model);
            }
        } else {
            warn!("⚠️ No Whisper engine found to unload model");
        }
    } else {
        info!("🧠 All transcript chunks processed. Keeping Whisper model resident for the next recording.");
    }

    // Step 4: Finalize recording state and cleanup resources safely
    let _ = ctx.sink.emit_event(
        "recording-shutdown-progress",
        &serde_json::json!({
            "stage": "finalizing",
            "message": "Finalizing recording and cleaning up resources...",
            "progress": 90
        }),
    );

    // Perform final cleanup with the manager if available
    let (meeting_folder, meeting_name, final_audio_path, final_duration_seconds) =
        if let Some(mut manager) = manager_for_cleanup {
            info!("🧹 Performing final cleanup and saving recording data");

            // Extract meeting info BEFORE async operations
            let meeting_folder = manager.get_meeting_folder();
            let meeting_name = manager.get_meeting_name();

            let (audio_path, duration_seconds) = match tokio::time::timeout(
                tokio::time::Duration::from_secs(300), // 5 minutes max for file I/O
                manager.save_recording_only(ctx.sink.as_ref()),
            )
            .await
            {
                Ok(Ok((audio_path, duration_seconds))) => {
                    info!("✅ Recording data saved successfully during cleanup");
                    (audio_path, duration_seconds)
                }
                Ok(Err(e)) => {
                    warn!(
                        "⚠️ Error during recording cleanup (transcripts preserved): {}",
                        e
                    );
                    // Don't fail shutdown - transcripts are already preserved
                    (None, None)
                }
                Err(_) => {
                    warn!(
                        "⏱️ File I/O timeout (5 minutes) reached during save, continuing shutdown"
                    );
                    // Don't fail shutdown - transcripts are already preserved
                    (None, None)
                }
            };

            (meeting_folder, meeting_name, audio_path, duration_seconds)
        } else {
            info!("ℹ️ No recording manager available for cleanup");
            (None, None, None, None)
        };

    // Recording state was already cleared when the manager was taken out of
    // `RECORDING_MANAGER` (and via `RecordingState::cleanup()`/`stop_recording()`
    // internally) earlier in this shutdown sequence — nothing left to flip here.

    // Step 4.5: Finalise the meeting row (issue #57 slice 2) and prepare
    // metadata for the frontend. The row itself — title, transcripts,
    // status — is entirely Rust's; the frontend's post-stop save now only
    // ever touches fields it still owns (see `api_save_meeting_title`).
    let (folder_path_str, meeting_name_str) = match (&meeting_folder, &meeting_name) {
        (Some(path), Some(name)) => (Some(path.to_string_lossy().to_string()), Some(name.clone())),
        _ => (None, None),
    };

    info!("📤 Preparing recording metadata for frontend");
    info!("   folder_path: {:?}", folder_path_str);
    info!("   meeting_name: {:?}", meeting_name_str);
    info!("   meeting_id: {:?}", meeting_id_for_stop);

    if let Some(mid) = &meeting_id_for_stop {
        if let Some(pool) = ctx.pool.clone() {
            let result = if was_fatal_error {
                MeetingsRepository::mark_meeting_interrupted(&pool, mid)
                    .await
                    .map(|_| ())
            } else {
                MeetingsRepository::mark_meeting_completed(
                    &pool,
                    mid,
                    final_duration_seconds,
                    final_audio_path.as_deref(),
                )
                .await
                .map(|_| ())
            };
            match result {
                Ok(_) => info!(
                    "DB: meeting {} row finalised ({})",
                    mid,
                    if was_fatal_error { "interrupted" } else { "completed" }
                ),
                Err(e) => warn!("DB: failed to finalise meeting {} row: {}", mid, e),
            }
        } else {
            warn!(
                "No DB pool available; meeting {} row was not finalised",
                mid
            );
        }
    }

    // Step 5: Complete shutdown
    let _ = ctx.sink.emit_event(
        "recording-shutdown-progress",
        &serde_json::json!({
            "stage": "complete",
            "message": "Recording stopped successfully",
            "progress": 100
        }),
    );

    // Emit final stop event with folder_path, meeting_name and meeting_id.
    // The frontend no longer needs meeting_id to look up *whether* it has a
    // meeting to update — the row already exists — only to know which row.
    ctx.sink
        .emit_event(
            "recording-stopped",
            &serde_json::json!({
                "message": "Recording stopped",
                "folder_path": folder_path_str,
                "meeting_name": meeting_name_str,
                "meeting_id": meeting_id_for_stop
            }),
        )?;

    // The canonical `recording-state` emit below drives the shell's tray
    // refresh — no separate tray call needed here.
    // Canonical state machine: Finalising -> Idle, now that
    // `recording-stopped` has been emitted.
    emit_phase(ctx.sink.as_ref(), RecordingPhase::Idle);

    info!("🎉 Recording stopped successfully with ZERO transcript chunks lost");
    Ok(())
}

/// Check if recording is active. Single source of truth: reads through the
/// live `RecordingManager` (in turn backed by `RecordingState`'s own atomic)
/// rather than a separately-flipped flag, so it can never drift out of sync.
pub async fn is_recording() -> bool {
    RECORDING_MANAGER
        .lock()
        .unwrap()
        .as_ref()
        .map(|m| m.is_recording())
        .unwrap_or(false)
}

/// Get recording statistics
pub async fn get_transcription_status() -> TranscriptionStatus {
    TranscriptionStatus {
        chunks_in_queue: transcription::queue_depth(),
        is_processing: is_recording().await,
        last_activity_ms: 0,
    }
}

/// Pause the current recording
pub async fn pause_recording(ctx: &RecordingContext) -> Result<(), String> {
    info!("Pausing recording");

    // Check if currently recording
    if !is_recording().await {
        return Err("No recording is currently active".to_string());
    }

    // Access the recording manager and pause it. Scoped so the lock is
    // dropped before `emit_phase` below re-locks the same (non-reentrant)
    // static to read live duration data.
    {
        let manager_guard = RECORDING_MANAGER.lock().unwrap();
        match manager_guard.as_ref() {
            Some(manager) => manager.pause_recording().map_err(|e| e.to_string())?,
            None => return Err("No recording manager found".to_string()),
        }
    }

    // Emit pause event to frontend
    ctx.sink.emit_event(
        "recording-paused",
        &serde_json::json!({
            "message": "Recording paused"
        }),
    )?;

    emit_phase(ctx.sink.as_ref(), RecordingPhase::Paused);

    info!("Recording paused successfully");
    Ok(())
}

/// Resume the current recording
pub async fn resume_recording(ctx: &RecordingContext) -> Result<(), String> {
    info!("Resuming recording");

    // Check if currently recording
    if !is_recording().await {
        return Err("No recording is currently active".to_string());
    }

    // Access the recording manager and resume it. Scoped so the lock is
    // dropped before `emit_phase` below re-locks the same (non-reentrant)
    // static to read live duration data.
    {
        let manager_guard = RECORDING_MANAGER.lock().unwrap();
        match manager_guard.as_ref() {
            Some(manager) => manager.resume_recording().map_err(|e| e.to_string())?,
            None => return Err("No recording manager found".to_string()),
        }
    }

    // Emit resume event to frontend
    ctx.sink.emit_event(
        "recording-resumed",
        &serde_json::json!({
            "message": "Recording resumed"
        }),
    )?;

    emit_phase(ctx.sink.as_ref(), RecordingPhase::Recording);

    info!("Recording resumed successfully");
    Ok(())
}

/// Check if recording is currently paused
pub async fn is_recording_paused() -> bool {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();
    if let Some(manager) = manager_guard.as_ref() {
        manager.is_paused()
    } else {
        false
    }
}

/// Get detailed recording state
pub async fn get_recording_state() -> serde_json::Value {
    let (
        is_recording_flag,
        is_paused_flag,
        is_active_flag,
        recording_duration,
        active_duration,
        total_pause_duration,
        current_pause_duration,
    ) = {
        let manager_guard = RECORDING_MANAGER.lock().unwrap();
        match manager_guard.as_ref() {
            Some(manager) => (
                manager.is_recording(),
                manager.is_paused(),
                manager.is_active(),
                manager.get_recording_duration(),
                manager.get_active_recording_duration(),
                manager.get_total_pause_duration(),
                manager.get_current_pause_duration(),
            ),
            None => (false, false, false, None, None, 0.0, None),
        }
    };
    let chunks_in_queue = transcription::queue_depth();

    // The canonical snapshot (phase, started_at_ms, meeting_name,
    // folder_path, error, seq) merged with the live duration/queue data
    // above — the same fields `recording-state` events carry, so a fresh
    // window/tab that only calls this once on mount gets exactly what it
    // would have received had it been listening from the start.
    let snapshot = recording_phase::build_snapshot(active_duration, total_pause_duration, chunks_in_queue);

    let mut value = serde_json::to_value(&snapshot).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(map) = &mut value {
        // Legacy keys kept for compatibility with existing callers.
        map.insert("is_recording".to_string(), serde_json::json!(is_recording_flag));
        map.insert(
            "is_finalising".to_string(),
            serde_json::json!(is_stop_in_progress()),
        );
        map.insert("is_paused".to_string(), serde_json::json!(is_paused_flag));
        map.insert("is_active".to_string(), serde_json::json!(is_active_flag));
        map.insert(
            "recording_duration".to_string(),
            serde_json::json!(recording_duration),
        );
        map.insert(
            "active_duration".to_string(),
            serde_json::json!(active_duration),
        );
        map.insert(
            "total_pause_duration".to_string(),
            serde_json::json!(total_pause_duration),
        );
        map.insert(
            "current_pause_duration".to_string(),
            serde_json::json!(current_pause_duration),
        );
    }
    value
}

/// Get the meeting folder path for the current recording
/// Returns the path if a meeting name was set and folder structure initialized
pub async fn get_meeting_folder_path() -> Result<Option<String>, String> {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();
    if let Some(manager) = manager_guard.as_ref() {
        Ok(manager
            .get_meeting_folder()
            .map(|p| p.to_string_lossy().to_string()))
    } else {
        Ok(None)
    }
}

/// Get accumulated transcript segments from current recording session
/// Used for syncing frontend state after page reload during active recording
pub async fn get_transcript_history() -> Result<Vec<crate::audio::recording_saver::TranscriptSegment>, String>
{
    let manager_guard = RECORDING_MANAGER.lock().unwrap();

    if let Some(manager) = manager_guard.as_ref() {
        Ok(manager.get_transcript_segments())
    } else {
        Ok(Vec::new()) // No recording active, return empty
    }
}

/// Get meeting name from current recording session
/// Used for syncing frontend state after page reload during active recording
pub async fn get_recording_meeting_name() -> Result<Option<String>, String> {
    let manager_guard = RECORDING_MANAGER.lock().unwrap();

    if let Some(manager) = manager_guard.as_ref() {
        Ok(manager.get_meeting_name())
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::recording_saver::TranscriptSegment;
    use crate::events::NullSink;

    fn segment(id: &str, sequence_id: u64) -> TranscriptSegment {
        TranscriptSegment {
            id: id.to_string(),
            text: format!("text for {}", id),
            timestamp: None,
            audio_start_time: None,
            audio_end_time: None,
            duration: None,
            display_time: None,
            confidence: None,
            sequence_id: Some(sequence_id),
            speaker: None,
            voice_profile_id: None,
            source: None,
        }
    }

    // Issue #25: segments buffered while the manager was out of
    // RECORDING_MANAGER must be replayed into it, in arrival order, and the
    // buffer must end up empty so nothing is replayed twice.
    #[test]
    fn replay_buffered_segments_drains_in_order_into_manager() {
        let manager = RecordingManager::new();
        let mut buffer = vec![segment("seg_1", 1), segment("seg_2", 2), segment("seg_3", 3)];

        replay_buffered_segments(&mut buffer, &manager);

        assert!(buffer.is_empty(), "buffer should be fully drained");
        let stored = manager.get_transcript_segments();
        let ids: Vec<&str> = stored.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["seg_1", "seg_2", "seg_3"]);
    }

    #[test]
    fn replay_buffered_segments_is_a_noop_on_empty_buffer() {
        let manager = RecordingManager::new();
        let mut buffer: Vec<TranscriptSegment> = Vec::new();

        replay_buffered_segments(&mut buffer, &manager);

        assert!(buffer.is_empty());
        assert!(manager.get_transcript_segments().is_empty());
    }

    // `PhaseGuard` is the RAII primitive behind START_IN_PROGRESS /
    // STOP_IN_PROGRESS; exercised here against a private, test-local static
    // so it never touches (or races against) the real process-global flags.
    #[test]
    fn phase_guard_releases_on_every_exit_path_including_early_return() {
        static FLAG: AtomicBool = AtomicBool::new(false);

        fn fallible_scope(flag: &'static AtomicBool, fail: bool) -> Result<(), String> {
            let _guard = PhaseGuard::try_acquire(flag).ok_or("already held")?;
            if fail {
                return Err("boom".to_string()); // exercises the `?`/early-return drop path
            }
            Ok(())
        }

        assert!(!FLAG.load(Ordering::SeqCst));
        assert!(fallible_scope(&FLAG, true).is_err());
        // Guard must have been dropped (and the flag cleared) even though
        // `fallible_scope` returned early via `?`.
        assert!(!FLAG.load(Ordering::SeqCst));

        // A second acquire attempt while one is *actually* held must fail...
        let held = PhaseGuard::try_acquire(&FLAG).expect("first acquire succeeds");
        assert!(PhaseGuard::try_acquire(&FLAG).is_none());
        drop(held);
        // ...and succeed again once released.
        assert!(PhaseGuard::try_acquire(&FLAG).is_some());
    }

    // The only test that touches the process-global START_IN_PROGRESS /
    // STOP_IN_PROGRESS statics — kept to a single test so parallel test
    // execution can never race it against another test mutating the same
    // statics (same convention as recording_phase.rs's PHASE_STATE test).
    #[tokio::test]
    async fn begin_start_phase_refuses_while_a_stop_is_finalising() {
        assert!(!STOP_IN_PROGRESS.load(Ordering::SeqCst));
        assert!(!START_IN_PROGRESS.load(Ordering::SeqCst));

        let stop_guard = PhaseGuard::try_acquire(&STOP_IN_PROGRESS).expect("claim stop flag");
        let result = begin_start_phase().await;
        assert!(
            result.is_err(),
            "start must be refused while a stop is finalising"
        );
        // The START_IN_PROGRESS guard `begin_start_phase` claimed internally
        // must have been released again on this error path.
        assert!(!START_IN_PROGRESS.load(Ordering::SeqCst));
        drop(stop_guard);

        // Once the stop clears, a start is allowed again.
        let start_guard = begin_start_phase()
            .await
            .expect("start allowed once the stop flag clears");
        drop(start_guard);
        assert!(!START_IN_PROGRESS.load(Ordering::SeqCst));
    }

    #[test]
    fn recording_context_is_cloneable_for_the_fatal_error_callback() {
        // spawn_fatal_error_stop needs to clone the context into a 'static
        // closure captured by RecordingManager::set_error_callback; this
        // would fail to compile if RecordingContext ever stopped being Clone.
        let ctx = RecordingContext::new(Arc::new(NullSink), None);
        let _clone = ctx.clone();
    }
}
