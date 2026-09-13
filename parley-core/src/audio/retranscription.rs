// Retranscription module - allows re-processing stored audio with different settings

use super::common::{create_transcript_segments, write_transcripts_json};
use super::constants::AUDIO_EXTENSIONS;
use crate::audio::decoder::{decode_audio_file_with_progress, ProgressCallback};
use crate::audio::vad::get_speech_chunks_with_progress;
use crate::config::DEFAULT_WHISPER_MODEL;
use crate::database::repositories::setting::SettingsRepository;
use crate::database::repositories::transcript::TranscriptsRepository;
use crate::events::{EventSink, EventSinkExt, SharedEventSink};
use crate::whisper_engine::WhisperEngine;
use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Global flag to track if retranscription is in progress
pub static RETRANSCRIPTION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Global flag to signal cancellation
static RETRANSCRIPTION_CANCELLED: AtomicBool = AtomicBool::new(false);

/// RAII guard for RETRANSCRIPTION_IN_PROGRESS flag
/// Ensures flag is cleared even if retranscription panics or returns early
struct RetranscriptionGuard;

impl RetranscriptionGuard {
    /// Create guard and set flag atomically
    fn acquire() -> Result<Self, String> {
        if RETRANSCRIPTION_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("Retranscription already in progress".to_string());
        }
        Ok(RetranscriptionGuard)
    }
}

impl Drop for RetranscriptionGuard {
    fn drop(&mut self) {
        RETRANSCRIPTION_IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

/// VAD redemption time in milliseconds — the silence gap that terminates a
/// speech segment. 800ms is short enough to cut at natural meeting pauses
/// (most fall in 500ms-1500ms) but long enough not to fragment mid-sentence
/// breaths. Live pipeline uses 400ms; batch uses 800ms because we need
/// segments short enough for the diarizer to embed cleanly (one speaker per
/// segment) but long enough that Whisper has lexical context.
///
/// History: was 2000ms before 2026-05-05; that turned out to be longer than
/// any silence in real meeting audio, so the VAD collapsed full meetings
/// into one segment and the 25s post-VAD splitter was doing all the work.
const VAD_REDEMPTION_TIME_MS: u32 = 800;

/// Progress update emitted during retranscription
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionProgress {
    pub meeting_id: String,
    pub stage: String, // "decoding", "transcribing", "saving"
    pub progress_percentage: u32,
    pub message: String,
}

/// Result of retranscription
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionResult {
    pub meeting_id: String,
    pub segments_count: usize,
    pub duration_seconds: f64,
    pub language: Option<String>,
}

/// Error during retranscription
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionError {
    pub meeting_id: String,
    pub error: String,
}

/// Check if retranscription is currently in progress
pub fn is_retranscription_in_progress() -> bool {
    RETRANSCRIPTION_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Cancel ongoing retranscription
pub fn cancel_retranscription() {
    RETRANSCRIPTION_CANCELLED.store(true, Ordering::SeqCst);
}

/// Start retranscription of a meeting's audio. Takes an already-resolved
/// event sink and DB pool (the Tauri shell's `start_retranscription` in
/// `retranscription_commands.rs` resolves both from a live `AppHandle`), so
/// this module never depends on Tauri.
pub async fn start_retranscription_with(
    sink: SharedEventSink,
    pool: Option<SqlitePool>,
    meeting_id: String,
    meeting_folder_path: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<RetranscriptionResult> {
    // Acquire guard - ensures flag is cleared even on panic/early return
    let _guard = RetranscriptionGuard::acquire().map_err(|e| anyhow!(e))?;

    // Reset cancellation flag
    RETRANSCRIPTION_CANCELLED.store(false, Ordering::SeqCst);

    let result = run_retranscription(
        &sink,
        pool,
        meeting_id.clone(),
        meeting_folder_path,
        language,
        model,
        provider,
    )
    .await;

    // Unload the engine after the batch job (success, failure, or cancellation)
    super::common::unload_engine_after_batch().await;

    // Guard will automatically clear flag on drop
    // No need for manual: RETRANSCRIPTION_IN_PROGRESS.store(false, Ordering::SeqCst);

    match &result {
        Ok(res) => {
            let _ = sink.emit_event(
                "retranscription-complete",
                &serde_json::json!({
                    "meeting_id": res.meeting_id,
                    "segments_count": res.segments_count,
                    "duration_seconds": res.duration_seconds,
                    "language": res.language
                }),
            );
        }
        Err(e) => {
            let _ = sink.emit_event(
                "retranscription-error",
                &RetranscriptionError {
                    meeting_id: meeting_id.clone(),
                    error: e.to_string(),
                },
            );
        }
    }

    result
}

/// Find audio file in meeting folder
/// Tries common names first, then scans for any file with an audio extension
fn find_audio_file(folder: &Path) -> Result<PathBuf> {
    let candidates = [
        "audio.mp4",
        "audio.m4a",
        "audio.wav",
        "audio.mp3",
        "audio.flac",
        "audio.ogg",
        "recording.mp4",
        "audio.mkv",
        "audio.webm",
        "audio.wma",
    ];

    for name in candidates {
        let path = folder.join(name);
        if path.exists() {
            return Ok(path);
        }
    }

    // Fallback: scan folder for any file with an audio extension
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                let ext = ext.to_string_lossy().to_lowercase();
                if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                    return Ok(path);
                }
            }
        }
    }

    Err(anyhow!("No audio file found in: {}", folder.display()))
}

/// Internal function to run retranscription
async fn run_retranscription(
    sink: &SharedEventSink,
    pool: Option<SqlitePool>,
    meeting_id: String,
    meeting_folder_path: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<RetranscriptionResult> {
    let folder_path = PathBuf::from(&meeting_folder_path);
    let audio_path = find_audio_file(&folder_path)?;

    // `provider` is accepted for backward-compat but only Whisper is supported now.
    let _ = provider;
    info!(
        "Starting retranscription for meeting {} with language {:?}, model {:?}",
        meeting_id, language, model
    );

    // Emit progress: decoding
    emit_progress(sink, &meeting_id, "decoding", 5, "Decoding audio file...");

    // Check for cancellation
    if RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst) {
        return Err(anyhow!("Retranscription cancelled"));
    }

    // Decode the audio file (CPU-intensive, run in blocking task). The
    // progress callback here only exists to honor cancellation mid-decode
    // (same convention as the VAD callback below) — retranscription doesn't
    // surface per-percentage decode progress today.
    let path_for_decode = audio_path.clone();
    let decoded = tokio::task::spawn_blocking(move || {
        let cancel_check: ProgressCallback =
            Box::new(|_progress, _msg| !RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst));
        decode_audio_file_with_progress(&path_for_decode, Some(cancel_check))
    })
    .await
    .map_err(|e| anyhow!("Decode task panicked: {}", e))??;
    let duration_seconds = decoded.duration_seconds;

    info!(
        "Decoded audio: {:.2}s, {}Hz, {} channels",
        duration_seconds, decoded.sample_rate, decoded.channels
    );

    emit_progress(
        sink,
        &meeting_id,
        "decoding",
        15,
        "Converting audio format...",
    );

    // Check for cancellation
    if RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst) {
        return Err(anyhow!("Retranscription cancelled"));
    }

    // Convert to 16kHz mono format (CPU-intensive, run in blocking task).
    // Same cancellation-only callback convention as the decode step above.
    let audio_samples = tokio::task::spawn_blocking(move || {
        let cancel_check: ProgressCallback =
            Box::new(|_progress, _msg| !RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst));
        decoded.to_whisper_format_with_progress(Some(cancel_check))
    })
    .await
    .map_err(|e| anyhow!("Resample task panicked: {}", e))??;
    info!(
        "Converted to 16kHz mono format: {} samples",
        audio_samples.len()
    );

    emit_progress(sink, &meeting_id, "vad", 20, "Detecting speech segments...");

    // Check for cancellation
    if RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst) {
        return Err(anyhow!("Retranscription cancelled"));
    }

    // Use VAD to find natural speech boundaries (same approach as live transcription)
    // IMPORTANT: Run VAD in a blocking task to avoid blocking the async runtime
    // For large files (35+ minutes), VAD processing can take several minutes
    let sink_for_vad = sink.clone();
    let meeting_id_for_vad = meeting_id.clone();

    let speech_segments = tokio::task::spawn_blocking(move || {
        get_speech_chunks_with_progress(
            &audio_samples,
            VAD_REDEMPTION_TIME_MS,
            |vad_progress, segments_found| {
                // Map VAD progress (0-100) to overall progress (20-25)
                let overall_progress = 20 + (vad_progress as f32 * 0.05) as u32;
                emit_progress(
                    &sink_for_vad,
                    &meeting_id_for_vad,
                    "vad",
                    overall_progress,
                    &format!(
                        "Detecting speech segments... {}% ({} found)",
                        vad_progress, segments_found
                    ),
                );

                // Return false to cancel if cancellation requested
                !RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst)
            },
        )
    })
    .await
    .map_err(|e| anyhow!("VAD task panicked: {}", e))?
    .map_err(|e| anyhow!("VAD processing failed: {}", e))?;

    let total_segments = speech_segments.len();
    info!(
        "VAD detected {} speech segments (redemption_time={}ms)",
        total_segments, VAD_REDEMPTION_TIME_MS
    );

    super::common::log_vad_diagnostics(&speech_segments);

    if total_segments == 0 {
        warn!("No speech detected in audio");
        return Err(anyhow!("No speech detected in audio file"));
    }

    emit_progress(
        sink,
        &meeting_id,
        "transcribing",
        25,
        "Loading transcription engine...",
    );

    // Initialize Whisper once (not per-segment)
    let whisper_engine = Some(get_or_init_whisper(pool.as_ref(), model.as_deref()).await?);

    // Build a fresh diarizer for this batch (None if speaker model isn't
    // downloaded). The mixed-audio source means we can't recover mic vs
    // system identity — the diarizer just clusters voices it hears, and
    // matches against stored profiles when available.
    let diarizer = match crate::speaker_diarization::service::build_diarizer(pool.as_ref()).await {
        Ok(d) => d,
        Err(e) => {
            warn!(
                "Speaker diarizer build failed: {} (continuing without speaker labels)",
                e
            );
            None
        }
    };
    if diarizer.is_some() {
        info!("Speaker diarization enabled for retranscription");
    } else {
        info!("Speaker diarization unavailable — transcripts will have no speaker labels");
    }

    // Split-at-silence -> per-segment transcribe -> diarize is shared with
    // audio import (see `common::run_batch_transcription`); only the
    // progress-event shape/percentage range differs here.
    let engine = whisper_engine
        .clone()
        .expect("whisper_engine is always Some at this point");
    let sink_for_progress = sink.clone();
    let meeting_id_for_progress = meeting_id.clone();
    let batch_result = super::common::run_batch_transcription(
        &speech_segments,
        language.clone(),
        engine,
        diarizer.clone(),
        // Retranscription re-processes an already-VAD'd session; it doesn't
        // run its own offline diarization pass (that's import-specific —
        // see `import.rs`), so there's no hint to feed the diarizer here.
        None,
        &RETRANSCRIPTION_CANCELLED,
        move |i, total, segment_duration_sec| {
            // Calculate progress (25% to 80% range for transcription)
            let progress = 25 + ((i as f32 / total as f32) * 55.0) as u32;
            emit_progress(
                &sink_for_progress,
                &meeting_id_for_progress,
                "transcribing",
                progress,
                &format!(
                    "Transcribing segment {} of {} ({:.1}s)...",
                    i + 1,
                    total,
                    segment_duration_sec
                ),
            );
        },
    )
    .await;

    let all_transcripts = match batch_result {
        Ok(transcripts) => transcripts,
        Err(e) => {
            return Err(if RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst) {
                anyhow!("Retranscription cancelled")
            } else {
                e
            });
        }
    };

    emit_progress(sink, &meeting_id, "saving", 80, "Saving transcripts...");

    // Create transcript segments with proper timestamps from VAD
    let segments = create_transcript_segments(&all_transcripts);

    // Never replace a meeting's transcript with nothing. If the batch pass
    // produced no text (VAD found no speech, or Whisper returned only
    // silence/hallucination-filtered output), the existing transcript — often
    // the live one, for auto-refine — is strictly better than an empty one.
    if segments.is_empty() {
        return Err(anyhow!(
            "Retranscription produced no transcript text; the existing transcript was kept"
        ));
    }

    // Save to database
    let pool = pool.ok_or_else(|| anyhow!("App state not available"))?;

    // Full delete+insert replace, in one transaction, to prevent data loss.
    TranscriptsRepository::replace_transcripts_for_meeting(&pool, &meeting_id, &segments)
        .await
        .map_err(|e| anyhow!("Failed to replace transcripts: {}", e))?;

    info!(
        "Updated {} transcripts for meeting {} in transaction",
        segments.len(),
        meeting_id
    );

    // Write updated transcripts.json and metadata.json to the meeting folder
    emit_progress(
        sink,
        &meeting_id,
        "saving",
        90,
        "Writing transcript files...",
    );

    if let Err(e) = write_transcripts_json(&folder_path, &segments) {
        warn!("Failed to write transcripts.json: {}", e);
    }

    // Find audio filename for metadata
    let audio_filename = audio_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.mp4")
        .to_string();

    // Sparse update: only touch the fields retranscription actually knows
    // changed. `common::write_metadata`'s merge behavior leaves everything
    // else (meeting_name, devices, sample_rate, ...) exactly as a prior
    // recording or import wrote it. If no metadata.json exists yet (meeting
    // folder somehow missing one), `fallback` reconstructs a full document.
    let now = chrono::Utc::now().to_rfc3339();
    let updates = super::common::MeetingMetadata {
        status: Some("completed".to_string()),
        transcript_file: Some("transcripts.json".to_string()),
        retranscribed_at: Some(now.clone()),
        origin: Some("retranscription".to_string()),
        ..Default::default()
    };
    let fallback_meeting_id = meeting_id.clone();
    let fallback_audio_filename = audio_filename.clone();
    let fallback_now = now.clone();
    if let Err(e) = super::common::write_metadata(&folder_path, &updates, move || {
        super::common::MeetingMetadata {
            version: Some("1.0".to_string()),
            meeting_id: Some(fallback_meeting_id),
            created_at: Some(fallback_now.clone()),
            completed_at: Some(fallback_now.clone()),
            retranscribed_at: Some(fallback_now),
            duration_seconds: Some(duration_seconds),
            audio_file: Some(fallback_audio_filename),
            transcript_file: Some("transcripts.json".to_string()),
            status: Some("completed".to_string()),
            origin: Some("retranscription".to_string()),
            ..Default::default()
        }
    }) {
        warn!("Failed to update metadata.json: {}", e);
    }

    emit_progress(
        sink,
        &meeting_id,
        "complete",
        100,
        "Retranscription complete",
    );

    // Install the batch diarizer as the process-wide current diarizer so
    // `promote_speaker_to_profile` can reach its embeddings when the user
    // names a "Speaker N" chip on the just-finished meeting. Without this,
    // the diarizer (and its history) would be dropped at function return,
    // forcing every promote to fall back to rename-only.
    //
    // Never while a live recording is running: that session owns the slot
    // (this matters for background auto-refine of the *previous* meeting
    // finishing after the *next* recording has started — swapping the slot
    // would cluster the rest of the live meeting in this batch's history).
    if crate::audio::recording_service::is_recording().await {
        warn!(
            "Recording in progress — not installing retranscription diarizer for meeting {} \
             (promote will degrade to relabel-only)",
            meeting_id
        );
    } else if let Some(d) = diarizer {
        crate::speaker_diarization::set_current_diarizer(Some(d));
        info!(
            "Installed retranscription diarizer as current_diarizer for meeting {}",
            meeting_id
        );
    }

    Ok(RetranscriptionResult {
        meeting_id,
        segments_count: segments.len(),
        duration_seconds,
        language,
    })
}

/// Emit progress event. Takes a `&dyn EventSink` rather than an `AppHandle`
/// since this only ever emits — see `events.rs`.
fn emit_progress(sink: &dyn EventSink, meeting_id: &str, stage: &str, progress: u32, message: &str) {
    let _ = sink.emit_event(
        "retranscription-progress",
        &RetranscriptionProgress {
            meeting_id: meeting_id.to_string(),
            stage: stage.to_string(),
            progress_percentage: progress,
            message: message.to_string(),
        },
    );
}

/// Get or initialize the Whisper engine, auto-loading the model if needed
/// If `requested_model` is provided, ensures that specific model is loaded
async fn get_or_init_whisper(
    pool: Option<&SqlitePool>,
    requested_model: Option<&str>,
) -> Result<Arc<WhisperEngine>> {
    use crate::whisper_engine::models::WHISPER_ENGINE;

    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    };

    match engine {
        Some(e) => {
            // Determine which model to use
            let target_model = match requested_model {
                Some(model) => model.to_string(),
                None => get_configured_whisper_model(pool).await?,
            };

            // Check if the correct model is already loaded
            let current_model = e.get_current_model().await;
            let needs_load = match &current_model {
                Some(loaded) => loaded != &target_model,
                None => true,
            };

            if needs_load {
                // Retranscription (manual, and auto-refine which runs through
                // this same path) is background/user-initiated batch work,
                // not latency sensitive — so if a live recording currently
                // holds the engine lease, wait for it to finish rather than
                // swap the model out from under it (which would silently
                // drop live transcript chunks; see `whisper_engine::lease`).
                if crate::whisper_engine::LIVE_ENGINE_LEASE.is_live_leased() {
                    warn!(
                        "Live recording holds the Whisper engine lease; waiting before loading '{}' for retranscription",
                        target_model
                    );
                    if !crate::whisper_engine::LIVE_ENGINE_LEASE
                        .wait_until_free(std::time::Duration::from_secs(30 * 60))
                        .await
                    {
                        return Err(anyhow!(
                            "Timed out waiting for live recording to release the Whisper engine"
                        ));
                    }
                }

                info!(
                    "Loading Whisper model '{}' (current: {:?})",
                    target_model, current_model
                );

                // Discover available models first (populates the internal cache)
                info!("Discovering available Whisper models...");
                if let Err(discover_err) = e.discover_models().await {
                    warn!(
                        "Error during model discovery (continuing anyway): {}",
                        discover_err
                    );
                }

                match e.load_model(&target_model).await {
                    Ok(_) => {
                        info!("Whisper model '{}' loaded successfully", target_model);
                        Ok(e)
                    }
                    Err(load_err) => {
                        error!(
                            "Failed to load Whisper model '{}': {}",
                            target_model, load_err
                        );
                        Err(anyhow!(
                            "Failed to load Whisper model '{}': {}",
                            target_model,
                            load_err
                        ))
                    }
                }
            } else {
                info!("Whisper model '{}' already loaded", target_model);
                Ok(e)
            }
        }
        None => Err(anyhow!("Whisper engine not initialized")),
    }
}

/// Get the configured Whisper model name from the database
async fn get_configured_whisper_model(pool: Option<&SqlitePool>) -> Result<String> {
    debug!("Getting configured Whisper model from database...");

    let pool = pool.ok_or_else(|| {
        error!("App state not available");
        anyhow!("App state not available")
    })?;

    debug!("Querying transcript_settings table...");

    // Query the transcript settings from the database - get both provider and model
    let result = SettingsRepository::get_transcript_config(pool)
        .await
        .map_err(|e| {
            error!("Failed to query transcript config: {}", e);
            anyhow!("Failed to query transcript config: {}", e)
        })?;

    match result {
        Some(config) => {
            info!(
                "Found transcript config: provider={}, model={}",
                config.provider, config.model
            );

            // Check if provider is Whisper-based
            if config.provider == "localWhisper" || config.provider == "whisper" {
                Ok(config.model)
            } else {
                error!(
                    "Retranscription requires Whisper provider, but configured provider is: {}",
                    config.provider
                );
                Err(anyhow!("Retranscription requires Whisper. Current provider '{}' does not support retranscription with language selection.", config.provider))
            }
        }
        None => {
            // Default to configured Whisper model if no config exists
            warn!(
                "No transcript config found, using default model '{}'",
                DEFAULT_WHISPER_MODEL
            );
            Ok(DEFAULT_WHISPER_MODEL.to_string())
        }
    }
}

// ============================================================================
// POST-MEETING AUTO-REFINE
//
// After a live recording finishes (fast model, low latency), best-effort
// re-run the finalized audio through this same retranscription pipeline
// with a higher-accuracy model in the background, and upgrade the stored
// transcript in place. Triggered by `recording_commands::trigger_post_meeting_refine`
// once the frontend has a `meeting_id` for the just-saved meeting (the Rust
// `stop_recording` command itself never has one — the `meetings` row, and
// its id, is created by the frontend's follow-up `api_save_transcript` call).
// ============================================================================

/// High-accuracy models to refine with, in priority order. large-v3-q5_0 is
/// preferred (near-identical accuracy to large-v3 at roughly a third of the
/// size/decode time); large-v3 is the fallback if only the full-precision
/// file is on disk. Whichever of these is already the live model is treated
/// as "nothing to gain" (see `run_auto_refine`).
const AUTO_REFINE_MODEL_CANDIDATES: &[&str] = &["large-v3-q5_0", "large-v3"];

/// Kick off a best-effort background auto-refine of a just-finished meeting.
/// Never blocks the caller: spawns its own tokio task and returns
/// immediately. Every skip/failure reason is logged; nothing is surfaced to
/// the user as an error since the live transcript is already saved and
/// stays authoritative unless/until this succeeds.
pub fn spawn_auto_refine(
    sink: SharedEventSink,
    pool: Option<SqlitePool>,
    meeting_id: String,
    meeting_folder_path: String,
) {
    tokio::spawn(async move {
        if let Err(e) = run_auto_refine(sink, pool, meeting_id.clone(), meeting_folder_path).await
        {
            info!("Auto-refine not performed for meeting {}: {}", meeting_id, e);
        }
    });
}

/// Decide whether an auto-refine pass is worth running, and if so, run it
/// through the same `start_retranscription` pipeline manual retranscription
/// uses. Returns `Err` for any skip reason (disabled, already-best model,
/// better model not downloaded, collision with an in-flight retranscription)
/// as well as genuine failures — callers only care that it's not `Ok`.
async fn run_auto_refine(
    sink: SharedEventSink,
    pool: Option<SqlitePool>,
    meeting_id: String,
    meeting_folder_path: String,
) -> Result<()> {
    // Preference check.
    let prefs = super::recording_preferences::load_recording_preferences(pool.clone()).await?;
    if !prefs.auto_refine {
        return Err(anyhow!("auto-refine disabled in recording preferences"));
    }

    // Yield to a user-initiated retranscription rather than collide with it.
    // (This is a courtesy early-out; the real race safety is the
    // RetranscriptionGuard acquired inside start_retranscription below.)
    if RETRANSCRIPTION_IN_PROGRESS.load(Ordering::SeqCst) {
        return Err(anyhow!(
            "retranscription already in progress, skipping auto-refine"
        ));
    }

    // Which model was used live? Only worth refining if a strictly
    // higher-accuracy one is available.
    let live_model = get_configured_whisper_model(pool.as_ref()).await?;
    if AUTO_REFINE_MODEL_CANDIDATES.contains(&live_model.as_str()) {
        return Err(anyhow!(
            "live model '{}' is already the high-accuracy tier, nothing to gain",
            live_model
        ));
    }

    // Pick the best candidate that's actually downloaded — never trigger a
    // multi-GB download just to auto-refine.
    let available = crate::whisper_engine::whisper_get_available_models()
        .await
        .map_err(|e| anyhow!(e))?;
    let target_model = AUTO_REFINE_MODEL_CANDIDATES.iter().find_map(|candidate| {
        available
            .iter()
            .find(|m| {
                m.name == *candidate
                    && matches!(m.status, crate::whisper_engine::ModelStatus::Available)
            })
            .map(|_| candidate.to_string())
    });
    let Some(target_model) = target_model else {
        return Err(anyhow!(
            "no higher-accuracy model downloaded (checked {:?}), skipping",
            AUTO_REFINE_MODEL_CANDIDATES
        ));
    };

    info!(
        "✨ Auto-refine starting for meeting {}: live model '{}' -> '{}'",
        meeting_id, live_model, target_model
    );
    let _ = sink.emit_event(
        "meeting-refining",
        &serde_json::json!({ "meeting_id": meeting_id }),
    );

    let result = start_retranscription_with(
        sink.clone(),
        pool,
        meeting_id.clone(),
        meeting_folder_path.clone(),
        None, // language: keep whatever auto-detect the live pass used
        Some(target_model.clone()),
        None,
    )
    .await;

    match &result {
        Ok(res) => {
            let now = chrono::Utc::now().to_rfc3339();
            let updates = super::common::MeetingMetadata {
                auto_refined_at: Some(now),
                ..Default::default()
            };
            if let Err(e) = super::common::write_metadata(
                &PathBuf::from(&meeting_folder_path),
                &updates,
                super::common::MeetingMetadata::default,
            ) {
                warn!(
                    "Failed to record auto_refined_at in metadata.json for meeting {}: {}",
                    meeting_id, e
                );
            }
            info!(
                "✨ Auto-refine complete for meeting {} ({} segments, model '{}')",
                meeting_id, res.segments_count, target_model
            );
            let _ = sink.emit_event(
                "meeting-refined",
                &serde_json::json!({
                    "meeting_id": meeting_id,
                    "segments_count": res.segments_count,
                }),
            );
        }
        Err(e) => {
            // Live transcript is untouched — run_retranscription only
            // mutates the DB/transcripts.json after a fully successful
            // batch, so a failure here leaves the meeting exactly as good
            // as it was, never worse.
            warn!(
                "Auto-refine failed for meeting {} (live transcript preserved): {}",
                meeting_id, e
            );
            let _ = sink.emit_event(
                "meeting-refine-failed",
                &serde_json::json!({ "meeting_id": meeting_id, "error": e.to_string() }),
            );
        }
    }

    result.map(|_| ())
}

/// Response when retranscription is started
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionStarted {
    pub meeting_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::common::BatchTranscript;

    fn bt(text: &str, start_ms: f64, end_ms: f64) -> BatchTranscript {
        BatchTranscript {
            text: text.to_string(),
            start_ms,
            end_ms,
            speaker: None,
            voice_profile_id: None,
            sequence_id: 0,
        }
    }

    #[test]
    fn test_create_transcript_segments_empty() {
        let transcripts: Vec<BatchTranscript> = vec![];
        let segments = create_transcript_segments(&transcripts);
        assert!(segments.is_empty());
    }

    #[test]
    fn test_create_transcript_segments_single() {
        let transcripts = vec![bt("Hello world", 0.0, 1500.0)]; // 0-1.5 seconds
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Hello world");
        assert_eq!(segments[0].audio_start_time, Some(0.0));
        assert_eq!(segments[0].audio_end_time, Some(1.5));
        assert_eq!(segments[0].duration, Some(1.5));
    }

    #[test]
    fn test_create_transcript_segments_multiple() {
        let transcripts = vec![
            bt("First segment", 0.0, 2000.0),
            bt("Second segment", 3000.0, 5000.0),
            bt("Third segment", 6500.0, 8000.0),
        ];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 3);

        assert_eq!(segments[0].text, "First segment");
        assert_eq!(segments[0].audio_start_time, Some(0.0));
        assert_eq!(segments[0].audio_end_time, Some(2.0));
        assert_eq!(segments[0].duration, Some(2.0));

        assert_eq!(segments[1].text, "Second segment");
        assert_eq!(segments[1].audio_start_time, Some(3.0));
        assert_eq!(segments[1].audio_end_time, Some(5.0));
        assert_eq!(segments[1].duration, Some(2.0));

        assert_eq!(segments[2].text, "Third segment");
        assert_eq!(segments[2].audio_start_time, Some(6.5));
        assert_eq!(segments[2].audio_end_time, Some(8.0));
        assert_eq!(segments[2].duration, Some(1.5));
    }

    #[test]
    fn test_create_transcript_segments_trims_whitespace() {
        let transcripts = vec![bt("  Hello with spaces  ", 0.0, 1000.0)];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Hello with spaces");
    }

    #[test]
    fn test_create_transcript_segments_generates_unique_ids() {
        let transcripts = vec![
            bt("Segment one", 0.0, 1000.0),
            bt("Segment two", 1000.0, 2000.0),
        ];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 2);
        assert_ne!(segments[0].id, segments[1].id);
        assert!(segments[0].id.starts_with("transcript-"));
        assert!(segments[1].id.starts_with("transcript-"));
    }

    #[test]
    fn test_create_transcript_segments_carries_speaker_attribution() {
        let transcripts = vec![
            BatchTranscript {
                text: "Hello".into(),
                start_ms: 0.0,
                end_ms: 1000.0,
                speaker: Some("Speaker 1".into()),
                voice_profile_id: None,
                sequence_id: 0,
            },
            BatchTranscript {
                text: "Hi".into(),
                start_ms: 1000.0,
                end_ms: 2000.0,
                speaker: Some("Alice".into()),
                voice_profile_id: Some("profile-abc".into()),
                sequence_id: 1,
            },
        ];
        let segments = create_transcript_segments(&transcripts);
        assert_eq!(segments[0].speaker.as_deref(), Some("Speaker 1"));
        assert!(segments[0].voice_profile_id.is_none());
        assert_eq!(segments[1].speaker.as_deref(), Some("Alice"));
        assert_eq!(segments[1].voice_profile_id.as_deref(), Some("profile-abc"));
    }

    #[test]
    fn test_cancellation_flag() {
        // Reset flag to known state
        RETRANSCRIPTION_CANCELLED.store(false, Ordering::SeqCst);
        RETRANSCRIPTION_IN_PROGRESS.store(false, Ordering::SeqCst);

        assert!(!is_retranscription_in_progress());

        // Test cancellation
        cancel_retranscription();
        assert!(RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst));

        // Reset for other tests
        RETRANSCRIPTION_CANCELLED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn test_vad_redemption_time_constant() {
        // Batch processing uses 800ms — long enough to bridge mid-sentence
        // breaths but short enough to cut at natural meeting pauses.
        assert_eq!(VAD_REDEMPTION_TIME_MS, 800);
    }

    #[test]
    fn test_find_audio_file_common_candidates() {
        let dir = tempfile::tempdir().unwrap();

        // No audio file → error
        assert!(find_audio_file(dir.path()).is_err());

        // Create audio.mp4 — should be found first
        std::fs::write(dir.path().join("audio.mp4"), b"fake").unwrap();
        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "audio.mp4");
    }

    #[test]
    fn test_find_audio_file_non_mp4_extensions() {
        let dir = tempfile::tempdir().unwrap();

        // Create audio.wav (imported as .wav, not .mp4)
        std::fs::write(dir.path().join("audio.wav"), b"fake").unwrap();
        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "audio.wav");
    }

    #[test]
    fn test_find_audio_file_fallback_scan() {
        let dir = tempfile::tempdir().unwrap();

        // Create a file with an audio extension but non-standard name
        std::fs::write(dir.path().join("my_recording.flac"), b"fake").unwrap();
        // Also add a non-audio file that should be ignored
        std::fs::write(dir.path().join("notes.txt"), b"text").unwrap();

        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "my_recording.flac");
    }

    #[test]
    fn test_find_audio_file_priority_order() {
        let dir = tempfile::tempdir().unwrap();

        // Create both audio.m4a and audio.mp4 — mp4 should win (listed first in candidates)
        std::fs::write(dir.path().join("audio.m4a"), b"fake").unwrap();
        std::fs::write(dir.path().join("audio.mp4"), b"fake").unwrap();
        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "audio.mp4");
    }

    #[test]
    fn test_find_audio_file_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_audio_file(dir.path());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No audio file found"));
    }

    #[test]
    fn test_find_audio_file_nonexistent_folder() {
        let result = find_audio_file(Path::new("/nonexistent/path/12345"));
        assert!(result.is_err());
    }

    #[test]
    fn test_audio_extensions_constant() {
        // Verify all expected formats are covered
        assert!(AUDIO_EXTENSIONS.contains(&"mp4"));
        assert!(AUDIO_EXTENSIONS.contains(&"m4a"));
        assert!(AUDIO_EXTENSIONS.contains(&"wav"));
        assert!(AUDIO_EXTENSIONS.contains(&"mp3"));
        assert!(AUDIO_EXTENSIONS.contains(&"flac"));
        assert!(AUDIO_EXTENSIONS.contains(&"ogg"));
        assert!(AUDIO_EXTENSIONS.contains(&"aac"));
        // FFmpeg-backed formats
        assert!(AUDIO_EXTENSIONS.contains(&"mkv"));
        assert!(AUDIO_EXTENSIONS.contains(&"webm"));
        assert!(AUDIO_EXTENSIONS.contains(&"wma"));
        // Non-audio formats
        assert!(!AUDIO_EXTENSIONS.contains(&"txt"));
        assert!(!AUDIO_EXTENSIONS.contains(&"pdf"));
    }
}
