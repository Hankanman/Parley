//! Shared, Tauri-free application startup and shutdown.
//!
//! Every UI shell (the Tauri app under `frontend/src-tauri`, and the
//! upcoming GPUI app in `parley-gpui`) needs the same sequence at launch —
//! resolve model/template directories, open (or defer) the database, warm up
//! the whisper engine / speaker diarizer / summary model manager, fetch any
//! missing built-in models — and the same sequence at exit — finish a
//! recording in flight, checkpoint the database, tear down the summary
//! sidecar, and unload the whisper model. This module is that sequence,
//! written once against `SharedEventSink` / `SqlitePool` instead of an
//! `AppHandle` so both shells can call it directly.
//!
//! Shell-specific concerns (creating a system tray, initializing OS
//! notifications, scheduling the delayed `first-launch-detected` event once
//! a webview is ready) stay in the shell — see
//! `frontend/src-tauri/src/lib.rs`'s `run()` for how the Tauri shell wires
//! this module in.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::audio;
use crate::database::{self, manager::DatabaseManager};
use crate::events::{EventSinkExt, SharedEventSink};
use crate::speaker_diarization;
use crate::summary::{self, summary_engine::model_manager::ModelManager};
use crate::utils;
use crate::whisper_engine;

/// Shared slot the summary engine's `ModelManager` lives in once
/// initialized — the same `Arc<Mutex<Option<Arc<ModelManager>>>>` shape as
/// `summary::summary_engine::ModelManagerState`'s inner field, kept as a bare
/// type here so this module doesn't need to depend on the Tauri-managed
/// wrapper struct.
pub type ModelManagerSlot = Arc<Mutex<Option<Arc<ModelManager>>>>;

/// Synchronous, call first: resolves and creates the whisper models
/// directory, the speaker-diarization models directory, and the
/// user-editable custom summary templates directory (all under
/// `paths::app_data_dir()`). Must run before `whisper_init` / the diarizer
/// pre-warm / anything that reads a custom template.
pub fn init_paths() {
    // First: every path below resolves under the data dir, and some of them
    // are cached for the process lifetime.
    crate::paths::migrate_legacy_data_dir();

    // Set models directory to use app_data_dir (unified storage location)
    whisper_engine::models::set_models_directory();

    // Set speaker-diarization models directory (separate from ASR models so
    // the speaker model can be downloaded independently).
    speaker_diarization::model::set_models_dir();

    // User-editable custom templates live under the same app-data root as
    // every other user-data path. Built-in templates are embedded in the
    // binary (see `summary::templates::defaults`) so no resource-dir lookup
    // is needed here.
    if let Ok(app_data_dir) = crate::paths::app_data_dir() {
        summary::templates::set_custom_templates_dir(app_data_dir.join("templates"));
    } else {
        log::warn!("Failed to resolve app data directory for custom templates");
    }
}

/// Open/migrate the database, or detect a first launch. Thin wrapper around
/// `database::setup::prepare_database_on_startup` (kept as a separate
/// function so shells have one bootstrap-module entry point to call rather
/// than reaching into `database::setup` directly).
///
/// `FirstLaunch` means there's no database yet — the shell's onboarding flow
/// creates one later. `Initialized(DatabaseManager)` is a ready-to-use
/// database; the caller is responsible for keeping it alive (as Tauri app
/// state, or in whatever the GPUI shell's equivalent is) and for cloning its
/// pool to pass to `spawn_background_init`.
pub async fn prepare_database() -> Result<database::setup::StartupOutcome, String> {
    let outcome = database::setup::prepare_database_on_startup().await?;
    // Needs the pool (stored recording paths are rewritten alongside the
    // folder move), and must finish before anything reads those paths.
    if let database::setup::StartupOutcome::Initialized(db) = &outcome {
        audio::recording_preferences::migrate_legacy_recordings_folder(db.pool()).await;
    }
    Ok(outcome)
}

/// Spawn the non-blocking background startup work on the current tokio
/// runtime: whisper engine init, speaker-diarizer pre-warm, summary
/// `ModelManager` init (into `model_manager`), the diagnostic model audit
/// log, and fetching any missing built-in models (silero VAD + speaker
/// model, then rebuilding the diarizer with them).
///
/// `pool` is the database pool from a completed `prepare_database()` call —
/// `None` on a first launch, before onboarding has created one. Passing the
/// real pool (rather than omitting it) is what lets the diarizer pre-warm
/// load voice profiles; call this *after* `prepare_database()` /
/// `init_paths()`, not before, or the pre-warm logs "DB pool unavailable"
/// and skips voice-profile loading even on a normal (non-first) launch.
///
/// Must be called from within a running tokio runtime (this function calls
/// `tokio::spawn`, which panics outside one — see
/// `tokio::runtime::Handle::try_current()` to check before calling if the
/// caller isn't certain). The Tauri shell satisfies this by invoking it from
/// inside a `tauri::async_runtime::spawn(async move { ... })` block in its
/// `setup()` closure, since `setup()` itself isn't guaranteed to run inside
/// the tokio runtime tauri manages.
pub fn spawn_background_init(
    sink: SharedEventSink,
    pool: Option<sqlx::SqlitePool>,
    model_manager: ModelManagerSlot,
) {
    // Initialize Whisper engine on startup.
    tokio::spawn(async {
        if let Err(e) = whisper_engine::models::whisper_init().await {
            log::error!("Failed to initialize Whisper engine on startup: {}", e);
        }
    });

    // Pre-warm the speaker diarizer at startup (if model is on disk) so the
    // first recording / retranscription doesn't pay the model load latency.
    // Async + non-blocking. Stores in the global slot; recording start
    // replaces it with a fresh instance to reset cluster IDs per session.
    {
        let pool = pool.clone();
        tokio::spawn(async move {
            match speaker_diarization::service::build_diarizer(pool.as_ref()).await {
                Ok(Some(diarizer)) => {
                    speaker_diarization::set_current_diarizer(Some(diarizer));
                    log::info!("✅ Speaker diarizer pre-initialized at startup");
                }
                Ok(None) => {
                    log::info!("Speaker diarizer pre-init skipped (model not downloaded yet)")
                }
                Err(e) => log::warn!("Speaker diarizer pre-init failed: {}", e),
            }
        });
    }

    // Initialize ModelManager for summary engine (async, non-blocking).
    {
        let model_manager = model_manager.clone();
        tokio::spawn(async move {
            match summary::summary_engine::service::init_model_manager_at_startup(&model_manager)
                .await
            {
                Ok(_) => log::info!("ModelManager initialized successfully at startup"),
                Err(e) => {
                    log::warn!("Failed to initialize ModelManager at startup: {}", e);
                    log::warn!("ModelManager will be lazy-initialized on first use");
                }
            }
        });
    }

    // Startup model audit — logs the on-disk state of every model the app
    // uses (Whisper / speaker diarizer / summary builtin-AI / VAD). Pure
    // diagnostic, non-fatal: helps rule out missing-model side effects when
    // investigating crashes. Runs after a short delay so the other startup
    // tasks have logged first and the audit shows the steady-state, not the
    // racing init.
    {
        let model_manager = model_manager.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            audit_models_at_startup(&model_manager).await;
        });
    }

    // Auto-download missing built-in models. Only the speaker diarization
    // model (plus the silero VAD model it's paired with here) is fetched —
    // Whisper is user-selectable so we leave it to onboarding / settings,
    // and summary models are picked by the existing built-in AI flow. Runs
    // after the audit so its log block stays unbroken.
    {
        let sink = sink.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            ensure_required_models_downloaded(sink, pool.as_ref()).await;
        });
    }
}

/// Diagnostic startup audit — logs the on-disk state of every model the app
/// depends on. Pure logging, no behavioural side effects: useful for ruling
/// out missing-model interactions when investigating crashes (especially the
/// sherpa-onnx / silero-rs / ort coexistence issue tracked in retranscription).
async fn audit_models_at_startup(model_manager: &ModelManagerSlot) {
    log::info!("───────────────── [startup-audit] models report ─────────────────");

    // ── Whisper (ASR) ─────────────────────────────────────────────────────
    match whisper_engine::models::whisper_get_available_models().await {
        Ok(models) if models.is_empty() => {
            log::warn!("[startup-audit] whisper:  NO models in models directory");
        }
        Ok(models) => {
            for m in &models {
                log::info!(
                    "[startup-audit] whisper:  {:<28} status={:?} size={}MB path={}",
                    m.name,
                    m.status,
                    m.size_mb,
                    m.path.display()
                );
            }
            let n_available = models
                .iter()
                .filter(|m| matches!(m.status, whisper_engine::ModelStatus::Available))
                .count();
            log::info!(
                "[startup-audit] whisper:  {} model(s) available, {} total catalog entries",
                n_available,
                models.len()
            );
        }
        Err(e) => log::warn!("[startup-audit] whisper:  failed to enumerate models: {}", e),
    }

    // ── Speaker diarizer ──────────────────────────────────────────────────
    let speaker_filename = speaker_diarization::model_filename();
    match speaker_diarization::default_model_path() {
        Some(path) => {
            let size_mb = path
                .metadata()
                .map(|m| m.len() / (1024 * 1024))
                .unwrap_or(0);
            let ready = speaker_diarization::model::model_is_ready(&path);
            let status = if ready { "PRESENT" } else { "MISSING" };
            log::info!(
                "[startup-audit] speaker:  {:<28} status={} size={}MB path={}",
                speaker_filename,
                status,
                size_mb,
                path.display()
            );
        }
        None => log::warn!("[startup-audit] speaker:  models directory not configured"),
    }

    // ── VAD (silero via sherpa-onnx) ──────────────────────────────────────
    match speaker_diarization::model::silero_vad_path() {
        Some(path) => {
            let size_mb = path
                .metadata()
                .map(|m| m.len() / (1024 * 1024))
                .unwrap_or(0);
            let ready = speaker_diarization::model::model_is_ready(&path);
            let status = if ready { "PRESENT" } else { "MISSING" };
            log::info!(
                "[startup-audit] vad:      silero_vad.onnx              status={} size={}MB path={}",
                status,
                size_mb,
                path.display()
            );
        }
        None => log::warn!("[startup-audit] vad:      models directory not configured"),
    }

    // ── Summary (built-in AI) ─────────────────────────────────────────────
    match summary::summary_engine::service::ensure_manager(model_manager).await {
        Ok(manager) => match summary::summary_engine::service::get_available_summary_model(&manager).await {
            Ok(Some(name)) => {
                log::info!("[startup-audit] summary:  {} (built-in AI, available)", name);
            }
            Ok(None) => {
                log::warn!(
                    "[startup-audit] summary:  NO built-in AI model available (gemma3:1b/4b not downloaded)"
                );
            }
            Err(e) => log::warn!("[startup-audit] summary:  status check failed: {}", e),
        },
        Err(e) => log::warn!("[startup-audit] summary:  status check failed: {}", e),
    }

    log::info!("─────────────────────────────────────────────────────────────────");
}

/// Background fetch of any built-in models that are missing on disk. Today
/// fetches:
/// - `silero_vad.onnx` — required for VAD (the audio pipeline can't function
///   without it; this is the model sherpa-onnx's `VoiceActivityDetector` uses).
/// - The speaker diarization model — required for "Speaker N" attribution
///   on system audio; without it, system transcripts fall back to the
///   "Speaker" placeholder.
///
/// Both are tiny (~2.3MB + ~28MB). Non-fatal: a failed download just leaves
/// the corresponding feature degraded. `sink` carries the speaker model's
/// `speaker-model-download-*` progress events to whichever shell called
/// `spawn_background_init`.
async fn ensure_required_models_downloaded(sink: SharedEventSink, pool: Option<&sqlx::SqlitePool>) {
    // ── silero VAD ──
    if let Some(silero_path) = speaker_diarization::model::silero_vad_path() {
        if !speaker_diarization::model::model_is_ready(&silero_path) {
            let url = speaker_diarization::model::silero_vad_download_url();
            log::info!(
                "[startup-download] silero-vad missing — fetching {} (~2.3MB, one-time)",
                url
            );
            match utils::download_file_to(url, &silero_path).await {
                Ok(()) => log::info!(
                    "[startup-download] silero-vad downloaded → {}",
                    silero_path.display()
                ),
                Err(e) => log::warn!(
                    "[startup-download] silero-vad download failed: {} \
                     (VAD disabled until next launch — recording / retranscription will error)",
                    e
                ),
            }
        }
    }

    // Deliberately NOT fetched here: the pyannote segmentation model (offline
    // / accurate diarization on Import). It's only needed when a user opts
    // into `offline_diarization_on_import` and imports a file worth running
    // it on, so the ~5.7MB download is deferred to first use via
    // `speaker_diarization::service::ensure_pyannote_segmentation_model`,
    // called from `audio::import` — not paid by every install/launch.

    // ── Speaker embedding ──
    let Some(speaker_path) = speaker_diarization::default_model_path() else {
        log::warn!(
            "[startup-download] speaker models dir not configured; cannot fetch speaker model"
        );
        return;
    };

    if speaker_diarization::model::model_is_ready(&speaker_path) {
        log::debug!(
            "[startup-download] speaker model already present at {}",
            speaker_path.display()
        );
    } else {
        log::info!(
            "[startup-download] speaker model missing — fetching {} (~28MB, one-time)",
            speaker_diarization::model_filename()
        );
        match speaker_diarization::service::download_speaker_model(sink.clone()).await {
            Ok(()) => log::info!(
                "[startup-download] speaker model downloaded → {}",
                speaker_path.display()
            ),
            Err(e) => {
                log::warn!(
                    "[startup-download] speaker model download failed: {} \
                     (speaker N attribution disabled)",
                    e
                );
                return;
            }
        }
    }

    // Now that both models are on disk, build the diarizer and pin it in
    // the global slot so the first recording / retranscription doesn't
    // pay the load cost.
    match speaker_diarization::service::build_diarizer(pool).await {
        Ok(Some(diarizer)) => {
            speaker_diarization::set_current_diarizer(Some(diarizer));
            log::info!("✅ [startup-download] speaker diarizer initialized");
        }
        Ok(None) => log::warn!(
            "[startup-download] speaker model present but diarizer build returned None"
        ),
        Err(e) => log::warn!("[startup-download] diarizer build failed: {}", e),
    }
}

/// If a recording is active or a stop is already draining one, run the full
/// `recording_service::stop` flow (the same `save_path` construction the
/// Tauri shell's `begin_shutdown_stop` used before this moved here) and emit
/// `recording-shutdown-progress` first so the UI can show "finishing
/// recording" while the app closes. A no-op when idle.
///
/// `ctx` should be built the same way a live recording command builds it
/// (event sink + DB pool) — see `commands::audio::recording_commands::build_context`
/// in the Tauri shell for the reference construction, which additionally
/// wraps the sink so it keeps refreshing the tray during shutdown.
pub async fn finish_recording_for_exit(ctx: audio::recording_service::RecordingContext) {
    use audio::recording_service::{self, RecordingArgs};

    let recording_active =
        recording_service::is_recording().await || recording_service::is_stop_in_progress();

    if !recording_active {
        return;
    }

    log::info!("App close requested mid-recording — finishing recording before exit...");
    let _ = ctx.sink.emit_event(
        "recording-shutdown-progress",
        &serde_json::json!({
            "stage": "app_closing",
            "message": "Finishing recording before closing...",
            "progress": 0
        }),
    );

    let save_path = crate::paths::app_data_dir()
        .map(|dir| {
            let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
            dir.join(format!("recording-{}.wav", timestamp))
                .to_string_lossy()
                .to_string()
        })
        .unwrap_or_else(|_| "recording.wav".to_string());

    if let Err(e) = recording_service::stop(ctx, RecordingArgs { save_path }).await {
        log::error!("Failed to stop recording during app close: {}", e);
    }
}

/// Exit cleanup: checkpoint/cleanup the database (if a `DatabaseManager` is
/// given — `None` when the app exits before one was ever opened, e.g. a
/// first launch abandoned during onboarding), force-shutdown the summary
/// sidecar, and unload the whisper model with a bounded 3s timeout.
///
/// Deliberately does **not** call `libc::_exit` — that decision (skip
/// destructors so ggml's CUDA/Vulkan/HIP finalizers don't race the GPU
/// driver's own atexit teardown and abort()) is shell-process-lifecycle
/// policy, not shutdown-sequencing logic, so it stays with each shell's exit
/// handler right after it calls this function. See the Tauri shell's
/// `RunEvent::Exit` handler in `lib.rs` for the full rationale and the
/// `unsafe { libc::_exit(0) }` call itself.
pub async fn shutdown(db: Option<&DatabaseManager>) {
    // Clean up database connection and checkpoint WAL.
    if let Some(db_manager) = db {
        log::info!("Starting database cleanup...");
        if let Err(e) = db_manager.cleanup().await {
            log::error!("Failed to cleanup database: {}", e);
        } else {
            log::info!("Database cleanup completed successfully");
        }
    } else {
        log::warn!("AppState not available for database cleanup (likely first launch)");
    }

    // Clean up sidecar.
    log::info!("Cleaning up sidecar...");
    if let Err(e) = summary::summary_engine::force_shutdown_sidecar().await {
        log::error!("Failed to force shutdown sidecar: {}", e);
    }

    // Unload the Whisper model (see #47 — it otherwise stays resident across
    // recordings, and would leak until process death). Best-effort and
    // bounded: this happens before the shell's `_exit`, so it's safe (unlike
    // the GPU-driver-teardown hazard `_exit` itself works around, this runs
    // while the process is still fully alive), but it must never hang app
    // shutdown if the engine is wedged.
    let engine_clone = {
        let engine_guard = whisper_engine::models::WHISPER_ENGINE.lock().unwrap();
        engine_guard.as_ref().cloned()
    };
    if let Some(engine) = engine_clone {
        match tokio::time::timeout(std::time::Duration::from_secs(3), engine.unload_model()).await
        {
            Ok(true) => log::info!("Whisper model unloaded on exit"),
            Ok(false) => log::debug!("No Whisper model was loaded on exit"),
            Err(_) => log::warn!("Whisper model unload timed out on exit; continuing shutdown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::NullSink;

    #[tokio::test]
    async fn shutdown_completes_with_no_db_no_engine_no_sidecar() {
        // No DatabaseManager, no whisper engine loaded, no sidecar running —
        // should just log and return, never panic or hang.
        shutdown(None).await;
    }

    #[tokio::test]
    async fn finish_recording_for_exit_is_noop_when_idle() {
        // With no recording active and no stop in progress, this must return
        // immediately without touching the (NullSink, no pool) context.
        let ctx = audio::recording_service::RecordingContext::new(Arc::new(NullSink), None);
        finish_recording_for_exit(ctx).await;
    }
}
