// Audio file import module - allows importing external audio files as new meetings

use crate::audio::common::TranscriptSegment;
use crate::audio::decoder::{decode_audio_file, decode_audio_file_with_progress};
use crate::audio::vad::get_speech_chunks_with_progress;
use crate::config::DEFAULT_WHISPER_MODEL;
use crate::database::repositories::setting::SettingsRepository;
use crate::database::repositories::transcript::TranscriptsRepository;
use crate::events::{EventSink, EventSinkExt, SharedEventSink};
use crate::whisper_engine::WhisperEngine;
use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::audio_processing::create_meeting_folder;
use super::common::{create_transcript_segments, write_transcripts_json};
use super::constants::AUDIO_EXTENSIONS;
use super::recording_preferences::get_default_recordings_folder;

/// Global flag to track if import is in progress
pub static IMPORT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Global flag to signal cancellation
static IMPORT_CANCELLED: AtomicBool = AtomicBool::new(false);

/// RAII guard for IMPORT_IN_PROGRESS flag
/// Ensures flag is cleared even if import panics or returns early
struct ImportGuard;

impl ImportGuard {
    /// Create guard and set flag atomically
    fn acquire() -> Result<Self, String> {
        if IMPORT_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("Import already in progress".to_string());
        }
        Ok(ImportGuard)
    }
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        IMPORT_IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

/// VAD redemption time in milliseconds. See retranscription.rs for the full
/// rationale — 800ms cuts at natural meeting pauses without fragmenting
/// breaths, and gives the diarizer one-speaker-per-segment material to embed.
const VAD_REDEMPTION_TIME_MS: u32 = 800;

/// Maximum file size: 20GB (prevents OOM and excessive processing time)
const MAX_FILE_SIZE_BYTES: u64 = 20 * 1024 * 1024 * 1024; // 20GB

/// Information about a selected audio file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFileInfo {
    pub path: String,
    pub filename: String,
    pub duration_seconds: f64,
    pub size_bytes: u64,
    pub format: String,
}

/// Progress update emitted during import
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportProgress {
    pub stage: String, // "copying", "decoding", "vad", "transcribing", "saving"
    pub progress_percentage: u32,
    pub message: String,
}

/// Result of import
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub meeting_id: String,
    pub title: String,
    pub segments_count: usize,
    pub duration_seconds: f64,
}

/// Error during import
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportError {
    pub error: String,
}

/// Warning emitted during import (non-fatal)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportWarning {
    pub warning: String,
    pub details: Option<String>,
}

/// Response when import is started
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportStarted {
    pub message: String,
}

/// Check if import is currently in progress
pub fn is_import_in_progress() -> bool {
    IMPORT_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Cancel ongoing import
pub fn cancel_import() {
    IMPORT_CANCELLED.store(true, Ordering::SeqCst);
}

/// Validate an audio file and return its info using metadata-only approach
/// Falls back to full decode if metadata is unavailable
pub fn validate_audio_file(path: &Path) -> Result<AudioFileInfo> {
    // Check file exists
    if !path.exists() {
        return Err(anyhow!("File does not exist: {}", path.display()));
    }

    // Check extension
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if !AUDIO_EXTENSIONS.contains(&extension.as_str()) {
        return Err(anyhow!(
            "Unsupported format: .{}. Supported: {}",
            extension,
            AUDIO_EXTENSIONS.join(", ")
        ));
    }

    // Get file size
    let metadata = std::fs::metadata(path).map_err(|e| anyhow!("Cannot read file: {}", e))?;
    let size_bytes = metadata.len();

    // Check file size limit
    if size_bytes > MAX_FILE_SIZE_BYTES {
        return Err(anyhow!(
            "File too large: {:.2}GB. Maximum supported size is {}GB",
            size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            MAX_FILE_SIZE_BYTES / (1024 * 1024 * 1024)
        ));
    }

    // Get filename without extension for title
    let filename = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported Audio")
        .to_string();

    // Try fast metadata-only validation first
    let duration_seconds = match extract_duration_from_metadata(path) {
        Ok(duration) => {
            debug!("Got duration from metadata: {:.2}s (fast path)", duration);
            duration
        }
        Err(e) => {
            // Fallback to full decode if metadata unavailable
            warn!(
                "Metadata extraction failed: {}, falling back to full decode",
                e
            );
            let decoded = decode_audio_file(path)?;
            decoded.duration_seconds
        }
    };

    Ok(AudioFileInfo {
        path: path.to_string_lossy().to_string(),
        filename,
        duration_seconds,
        size_bytes,
        format: extension.to_uppercase(),
    })
}

/// Extract duration from audio file metadata without full decode
/// Returns error if metadata is unavailable, triggering fallback to full decode
fn extract_duration_from_metadata(path: &Path) -> Result<f64> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    // Open the file
    let file =
        std::fs::File::open(path).map_err(|e| anyhow!("Failed to open audio file: {}", e))?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Set up format hint based on file extension
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    // Probe the file format (lightweight operation)
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("Failed to probe audio format: {}", e))?;

    let format = probed.format;

    // Find the first audio track
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("No audio track found in file"))?;

    // Extract duration from metadata
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("Unknown sample rate"))?;

    let n_frames = track
        .codec_params
        .n_frames
        .ok_or_else(|| anyhow!("Frame count not available in metadata"))?;

    let duration_seconds = n_frames as f64 / sample_rate as f64;

    debug!(
        "Extracted metadata: {}Hz, {} frames, {:.2}s",
        sample_rate, n_frames, duration_seconds
    );

    Ok(duration_seconds)
}

/// Start import of an audio file. Takes an already-resolved event sink and
/// DB pool (the Tauri shell's `start_import` in `import_commands.rs`
/// resolves both from a live `AppHandle`), so this module never depends on
/// Tauri.
pub async fn start_import_with(
    sink: SharedEventSink,
    pool: Option<SqlitePool>,
    source_path: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    num_speakers: i32,
) -> Result<ImportResult> {
    // Acquire guard - ensures flag is cleared even on panic/early return
    let _guard = ImportGuard::acquire().map_err(|e| anyhow!(e))?;

    // Reset cancellation flag
    IMPORT_CANCELLED.store(false, Ordering::SeqCst);

    let result = run_import(
        &sink,
        pool,
        source_path,
        title,
        language,
        model,
        provider,
        num_speakers,
    )
    .await;

    // Unload the engine after the batch job (success, failure, or cancellation)
    super::common::unload_engine_after_batch().await;

    // Guard will automatically clear flag on drop
    // No need for manual: IMPORT_IN_PROGRESS.store(false, Ordering::SeqCst);

    match &result {
        Ok(res) => {
            let _ = sink.emit_event(
                "import-complete",
                &serde_json::json!({
                    "meeting_id": res.meeting_id,
                    "title": res.title,
                    "segments_count": res.segments_count,
                    "duration_seconds": res.duration_seconds
                }),
            );
        }
        Err(e) => {
            let _ = sink.emit_event(
                "import-error",
                &ImportError {
                    error: e.to_string(),
                },
            );
        }
    }

    result
}

/// Internal function to run import
async fn run_import(
    sink: &SharedEventSink,
    pool: Option<SqlitePool>,
    source_path: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    num_speakers: i32,
) -> Result<ImportResult> {
    let source = PathBuf::from(&source_path);

    // Validate source file
    if !source.exists() {
        return Err(anyhow!("Source file not found: {}", source.display()));
    }

    // `provider` parameter is accepted for backward compatibility but only
    // Whisper is supported now (Parakeet was removed).
    let _ = provider;
    info!(
        "Starting import for '{}' from {} with language {:?}, model {:?}",
        title, source_path, language, model
    );

    emit_progress(sink, "copying", 5, "Creating meeting folder...");

    // Check for cancellation
    if IMPORT_CANCELLED.load(Ordering::SeqCst) {
        return Err(anyhow!("Import cancelled"));
    }

    // Create meeting folder
    let base_folder = get_default_recordings_folder();
    let meeting_folder = create_meeting_folder(&base_folder, &title, false)?;

    // Copy audio file to meeting folder
    emit_progress(sink, "copying", 10, "Copying audio file...");

    let dest_filename = format!(
        "audio.{}",
        source.extension().and_then(|e| e.to_str()).unwrap_or("mp4")
    );
    let dest_path = meeting_folder.join(&dest_filename);

    let src = source.clone();
    let dst = dest_path.clone();
    tokio::task::spawn_blocking(move || std::fs::copy(&src, &dst))
        .await
        .map_err(|e| anyhow!("Copy task join error: {}", e))?
        .map_err(|e| anyhow!("Failed to copy audio file: {}", e))?;

    info!("Copied audio to: {}", dest_path.display());

    // Check for cancellation
    if IMPORT_CANCELLED.load(Ordering::SeqCst) {
        // Cleanup: remove the meeting folder
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    emit_progress(sink, "decoding", 15, "Decoding audio file...");

    // Decode the audio file with progress updates
    let sink_for_decode = sink.clone();
    let decode_progress = Box::new(move |progress: u32, msg: &str| {
        // Map decode progress: 15% + (progress * 0.05) to go from 15% to 20%
        let overall_progress = 15 + ((progress as f32 * 0.05) as u32);
        emit_progress(&sink_for_decode, "decoding", overall_progress, msg);
        // Returning false aborts decode_audio_file_with_progress immediately,
        // same convention as the VAD progress callback below.
        !IMPORT_CANCELLED.load(Ordering::SeqCst)
    });

    let path_for_decode = dest_path.clone();
    let decoded = tokio::task::spawn_blocking(move || {
        decode_audio_file_with_progress(&path_for_decode, Some(decode_progress))
    })
    .await
    .map_err(|e| anyhow!("Decode task join error: {}", e))??;
    let duration_seconds = decoded.duration_seconds;

    info!(
        "Decoded audio: {:.2}s, {}Hz, {} channels",
        duration_seconds, decoded.sample_rate, decoded.channels
    );

    emit_progress(sink, "resampling", 20, "Converting audio format...");

    // Check for cancellation
    if IMPORT_CANCELLED.load(Ordering::SeqCst) {
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    // Convert to 16kHz mono format with progress updates
    let sink_for_resample = sink.clone();
    let resample_progress = Box::new(move |progress: u32, msg: &str| {
        // Map resample progress: 20% + (progress * 0.05) to go from 20% to 25%
        let overall_progress = 20 + ((progress as f32 * 0.05) as u32);
        emit_progress(&sink_for_resample, "resampling", overall_progress, msg);
        !IMPORT_CANCELLED.load(Ordering::SeqCst)
    });

    let audio_samples = tokio::task::spawn_blocking(move || {
        decoded.to_whisper_format_with_progress(Some(resample_progress))
    })
    .await
    .map_err(|e| anyhow!("Resample task join error: {}", e))??;
    info!(
        "Converted to 16kHz mono format: {} samples",
        audio_samples.len()
    );

    emit_progress(sink, "vad", 25, "Detecting speech segments...");

    // Check for cancellation
    if IMPORT_CANCELLED.load(Ordering::SeqCst) {
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    // Keep a copy of the full 16 kHz buffer for offline diarization — the VAD
    // spawn_blocking below moves `audio_samples` into its closure.
    let audio_for_diar = audio_samples.clone();

    // Use VAD to find speech segments
    let sink_for_vad = sink.clone();

    let speech_segments = tokio::task::spawn_blocking(move || {
        get_speech_chunks_with_progress(
            &audio_samples,
            VAD_REDEMPTION_TIME_MS,
            |vad_progress, segments_found| {
                let overall_progress = 25 + (vad_progress as f32 * 0.05) as u32;
                emit_progress(
                    &sink_for_vad,
                    "vad",
                    overall_progress,
                    &format!(
                        "Detecting speech segments... {}% ({} found)",
                        vad_progress, segments_found
                    ),
                );
                !IMPORT_CANCELLED.load(Ordering::SeqCst)
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

        // Emit warning to frontend
        let _ = sink.emit_event(
            "import-warning",
            &ImportWarning {
                warning: "No speech detected in audio file".to_string(),
                details: Some(
                    "The file was imported successfully, but VAD did not detect any speech. \
                     The meeting was created but contains no transcripts."
                        .to_string(),
                ),
            },
        );
        // Still create the meeting, just with no transcripts
    }

    // Check for cancellation
    if IMPORT_CANCELLED.load(Ordering::SeqCst) {
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    emit_progress(sink, "transcribing", 30, "Loading transcription engine...");

    // Initialize Whisper for the import job (only if there's anything to transcribe).
    let whisper_engine = if total_segments > 0 {
        Some(get_or_init_whisper(pool.as_ref(), model.as_deref()).await?)
    } else {
        None
    };

    // Build a fresh diarizer for the batch (None if speaker model isn't on
    // disk). Imported audio is a single mixed stream — the diarizer just
    // clusters voices and matches stored profiles when available.
    let diarizer = if total_segments > 0 {
        match crate::speaker_diarization::service::build_diarizer(pool.as_ref()).await {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    "Speaker diarizer build failed: {} (continuing without speaker labels)",
                    e
                );
                None
            }
        }
    } else {
        None
    };
    if diarizer.is_some() {
        info!("Speaker diarization enabled for import");
    }

    // Accurate (offline) diarization over the WHOLE file: pyannote
    // segmentation + global clustering, optionally told the exact speaker
    // count. Far more accurate than the per-segment online clusterer for a
    // single clean import source. Rather than bypassing the diarizer built
    // above, its turns become *hints* fed into that same diarizer (see
    // `common::run_batch_transcription` and `Diarizer::process_with_hint`):
    // the embedding is still computed and recorded to history, a stored
    // voice profile still takes precedence, only the "Speaker N" fallback
    // id changes. That keeps promote/rename and the post-import
    // `refine_and_persist` pass working against one diarizer instance
    // instead of a second, disconnected labelling path.
    //
    // Gated behind the `offline_diarization_on_import` preference (default
    // on), only attempted when it's likely to matter (an explicit speaker
    // count, or a file long enough that the online greedy clusterer is more
    // likely to drift), and run off the async runtime — pyannote
    // segmentation over a long file is minutes of CPU.
    let offline_turns: Option<Vec<crate::speaker_diarization::offline::SpeakerTurn>> =
        if total_segments > 0 && diarizer.is_some() {
            let prefs = super::recording_preferences::load_recording_preferences(pool.clone())
                .await
                .unwrap_or_default();
            let worth_it = num_speakers != 0 || duration_seconds > 60.0;

            if prefs.offline_diarization_on_import && worth_it {
                match crate::speaker_diarization::service::ensure_pyannote_segmentation_model()
                    .await
                {
                    Ok(seg_path) => match crate::speaker_diarization::model::default_model_path() {
                        Some(emb_path) if emb_path.exists() => {
                            emit_progress(sink, "diarizing", 28, "Analyzing speakers...");

                            let seg_path = PathBuf::from(seg_path);
                            let samples = audio_for_diar;
                            let threads = crate::audio::hardware_detector::HardwareProfile::detect()
                                .get_whisper_config()
                                .max_threads
                                .unwrap_or(1)
                                .max(1) as i32;

                            match tokio::task::spawn_blocking(move || {
                                crate::speaker_diarization::offline::diarize_offline(
                                    &samples,
                                    &seg_path,
                                    &emb_path,
                                    num_speakers,
                                    threads,
                                )
                            })
                            .await
                            {
                                Ok(Ok(turns)) => {
                                    info!(
                                        "Offline diarization ready: {} turns (num_speakers={})",
                                        turns.len(),
                                        num_speakers
                                    );
                                    Some(turns)
                                }
                                Ok(Err(e)) => {
                                    warn!(
                                        "Offline diarization failed ({e}); falling back to \
                                         the online clusterer"
                                    );
                                    None
                                }
                                Err(e) => {
                                    warn!(
                                        "Offline diarization task panicked ({e}); falling \
                                         back to the online clusterer"
                                    );
                                    None
                                }
                            }
                        }
                        _ => {
                            info!("Speaker embedding model absent; skipping offline diarization");
                            None
                        }
                    },
                    Err(e) => {
                        warn!(
                            "Pyannote segmentation model unavailable ({e}); falling back to \
                             the online clusterer"
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

    // Split-at-silence -> per-segment transcribe -> diarize is shared with
    // retranscription (see `common::run_batch_transcription`); only the
    // progress-event shape/percentage range differs here.
    let batch_result = if total_segments > 0 {
        let engine = whisper_engine
            .clone()
            .expect("whisper_engine is Some when total_segments > 0");
        let sink_for_progress = sink.clone();
        super::common::run_batch_transcription(
            &speech_segments,
            language.clone(),
            engine,
            diarizer.clone(),
            offline_turns.as_deref(),
            &IMPORT_CANCELLED,
            move |i, total, segment_duration_sec| {
                let progress = 30 + ((i as f32 / total.max(1) as f32) * 50.0) as u32;
                emit_progress(
                    &sink_for_progress,
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
        .await
    } else {
        Ok(Vec::new())
    };

    let all_transcripts = match batch_result {
        Ok(transcripts) => transcripts,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&meeting_folder);
            return Err(if IMPORT_CANCELLED.load(Ordering::SeqCst) {
                anyhow!("Import cancelled")
            } else {
                e
            });
        }
    };

    emit_progress(sink, "saving", 85, "Creating meeting...");

    // Create transcript segments
    let segments = create_transcript_segments(&all_transcripts);

    // Save to database
    let pool = pool.ok_or_else(|| anyhow!("App state not available"))?;

    let meeting_id = create_meeting_with_transcripts(
        &pool,
        &title,
        &segments,
        meeting_folder.to_string_lossy().to_string(),
    )
    .await?;

    // Write transcripts.json and metadata.json to the meeting folder
    emit_progress(sink, "saving", 90, "Writing transcript files...");

    if let Err(e) = write_transcripts_json(&meeting_folder, &segments) {
        warn!("Failed to write transcripts.json: {}", e);
    }

    let now = chrono::Utc::now().to_rfc3339();
    let metadata = super::common::MeetingMetadata {
        version: Some("1.0".to_string()),
        meeting_id: Some(meeting_id.clone()),
        meeting_name: Some(title.clone()),
        created_at: Some(now.clone()),
        completed_at: Some(now),
        duration_seconds: Some(duration_seconds),
        audio_file: Some(dest_filename.clone()),
        transcript_file: Some("transcripts.json".to_string()),
        status: Some("completed".to_string()),
        origin: Some("import".to_string()),
        ..Default::default()
    };
    if let Err(e) = super::common::write_metadata(&meeting_folder, &metadata, || metadata.clone()) {
        warn!("Failed to write metadata.json: {}", e);
    }

    emit_progress(sink, "complete", 100, "Import complete");

    // Install the batch diarizer as current so the user can name "Speaker N"
    // on the just-imported meeting and reach this batch's embeddings (see
    // matching note in retranscription.rs for the full rationale).
    //
    // Never while a live recording is running: that session owns the slot,
    // and swapping it out from under the transcription worker would cluster
    // the rest of the live meeting in this batch's history. Promote on the
    // imported meeting then degrades to relabel-only, which is the lesser
    // harm.
    if crate::audio::recording_service::is_recording().await {
        warn!(
            "Recording in progress — not installing import diarizer for meeting {} \
             (promote will degrade to relabel-only)",
            meeting_id
        );
    } else if let Some(d) = diarizer {
        crate::speaker_diarization::set_current_diarizer(Some(d));
        info!(
            "Installed import diarizer as current_diarizer for meeting {}",
            meeting_id
        );

        // Re-cluster offline and persist any improved labels — addresses #9
        // for the online-clustering fallback too (when offline diarization
        // above did run, hinted labels are already accurate and this is a
        // no-op; `refine_and_persist` only rewrites rows that actually
        // change). Backgrounded so import completion isn't held up by it,
        // same as the equivalent post-recording pass in
        // `recording_commands::trigger_post_meeting_refine`. Transcripts are
        // already persisted at this point (see `create_meeting_with_transcripts`
        // above), which `refine_and_persist` requires.
        let sink_for_refine = sink.clone();
        let pool_for_refine = pool.clone();
        let meeting_id_for_refine = meeting_id.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::speaker_diarization::service::refine_and_persist(
                &sink_for_refine,
                &pool_for_refine,
                &meeting_id_for_refine,
            )
            .await
            {
                warn!(
                    "Speaker refinement failed for imported meeting {}: {} \
                     (labels left as recorded)",
                    meeting_id_for_refine, e
                );
            }
        });
    }

    Ok(ImportResult {
        meeting_id,
        title,
        segments_count: segments.len(),
        duration_seconds,
    })
}

/// Emit progress event. Takes a `&dyn EventSink` rather than an `AppHandle`
/// since this only ever emits — see `events.rs`.
fn emit_progress(sink: &dyn EventSink, stage: &str, progress: u32, message: &str) {
    let _ = sink.emit_event(
        "import-progress",
        &ImportProgress {
            stage: stage.to_string(),
            progress_percentage: progress,
            message: message.to_string(),
        },
    );
}

/// Create a new meeting with transcripts in the database
async fn create_meeting_with_transcripts(
    pool: &sqlx::SqlitePool,
    title: &str,
    segments: &[TranscriptSegment],
    folder_path: String,
) -> Result<String> {
    let meeting_id = TranscriptsRepository::save_transcript(pool, title, segments, Some(folder_path))
        .await
        .map_err(|e| anyhow!("Failed to create meeting: {}", e))?;

    info!(
        "Created meeting '{}' with {} transcripts",
        meeting_id,
        segments.len()
    );

    Ok(meeting_id)
}

/// Get or initialize the Whisper engine
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
            let target_model = match requested_model {
                Some(model) => model.to_string(),
                None => get_configured_model(pool).await?,
            };

            let current_model = e.get_current_model().await;
            let needs_load = match &current_model {
                Some(loaded) => loaded != &target_model,
                None => true,
            };

            if needs_load {
                // Import is user-initiated and the user is waiting on it, so
                // prefer not to block on a live recording if we can avoid it.
                // If a live recording holds the engine lease, reuse whatever
                // model it already has loaded instead of swapping (swapping
                // would silently drop live transcript chunks while the load
                // is in flight — see `whisper_engine::lease`); only fall back
                // to waiting for the lease when there's no loaded model to
                // reuse at all (e.g. caught mid-startup).
                if crate::whisper_engine::LIVE_ENGINE_LEASE.is_live_leased() {
                    if let Some(loaded) = &current_model {
                        warn!(
                            "Live recording holds the Whisper engine lease; reusing loaded model '{}' for import instead of switching to requested '{}'",
                            loaded, target_model
                        );
                        return Ok(e);
                    }
                    warn!(
                        "Live recording holds the Whisper engine lease and no model is currently loaded; waiting before loading '{}' for import",
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

                if let Err(e) = e.discover_models().await {
                    warn!("Model discovery error (continuing): {}", e);
                }

                e.load_model(&target_model)
                    .await
                    .map_err(|e| anyhow!("Failed to load model '{}': {}", target_model, e))?;
            }

            Ok(e)
        }
        None => Err(anyhow!("Whisper engine not initialized")),
    }
}

/// Get the configured Whisper model from database, defaulting to
/// [`DEFAULT_WHISPER_MODEL`] if nothing is saved or the saved provider isn't
/// Whisper.
async fn get_configured_model(pool: Option<&SqlitePool>) -> Result<String> {
    let pool = pool.ok_or_else(|| anyhow!("App state not available"))?;

    let config = SettingsRepository::get_transcript_config(pool)
        .await
        .map_err(|e| anyhow!("Failed to query config: {}", e))?;

    match config {
        Some(config) if config.provider == "localWhisper" || config.provider == "whisper" => {
            Ok(config.model)
        }
        _ => Ok(DEFAULT_WHISPER_MODEL.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::{split_segment_at_silence, BatchTranscript};
    use super::*;

    #[test]
    fn test_audio_extensions() {
        assert!(AUDIO_EXTENSIONS.contains(&"mp4"));
        assert!(AUDIO_EXTENSIONS.contains(&"wav"));
        assert!(AUDIO_EXTENSIONS.contains(&"mp3"));
        assert!(!AUDIO_EXTENSIONS.contains(&"txt"));
    }

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
        let transcripts = vec![bt("Hello world", 0.0, 1500.0)];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Hello world");
        assert_eq!(segments[0].audio_start_time, Some(0.0));
        assert_eq!(segments[0].audio_end_time, Some(1.5));
    }

    #[test]
    fn test_cancellation_flag() {
        IMPORT_CANCELLED.store(false, Ordering::SeqCst);
        IMPORT_IN_PROGRESS.store(false, Ordering::SeqCst);

        assert!(!is_import_in_progress());

        cancel_import();
        assert!(IMPORT_CANCELLED.load(Ordering::SeqCst));

        // Reset
        IMPORT_CANCELLED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn test_extract_duration_from_metadata_wav() {
        // Test with sample WAV file if available
        let test_path = Path::new("../../backend/whisper.cpp/samples/jfk.wav");
        if test_path.exists() {
            let result = extract_duration_from_metadata(test_path);
            // Should succeed and return a reasonable duration
            assert!(result.is_ok());
            let duration = result.unwrap();
            assert!(
                duration > 0.0 && duration < 60.0,
                "Duration {} seems unreasonable",
                duration
            );
        }
    }

    #[test]
    fn test_extract_duration_from_metadata_mp3() {
        // Test with sample MP3 file if available
        let test_path = Path::new("../../backend/whisper.cpp/samples/jfk.mp3");
        if test_path.exists() {
            let result = extract_duration_from_metadata(test_path);
            // MP3 files may not have n_frames metadata, so fallback is expected
            // We just verify it doesn't panic
            let _ = result;
        }
    }

    #[test]
    fn test_validate_audio_file_with_metadata() {
        // Test validation with actual audio file
        let test_path = Path::new("../../backend/whisper.cpp/samples/jfk.wav");
        if test_path.exists() {
            let result = validate_audio_file(test_path);
            assert!(result.is_ok());
            let info = result.unwrap();
            assert_eq!(info.format, "WAV");
            assert!(info.duration_seconds > 0.0);
            assert!(info.size_bytes > 0);
        }
    }

    #[test]
    fn test_validate_audio_file_nonexistent() {
        let result = validate_audio_file(Path::new("/nonexistent/file.mp4"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_validate_audio_file_wrong_extension() {
        // Create a temporary file with wrong extension
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("test_audio.txt");
        let _ = std::fs::write(&temp_file, b"dummy content");

        let result = validate_audio_file(&temp_file);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported format"));

        // Cleanup
        let _ = std::fs::remove_file(temp_file);
    }

    #[test]
    fn test_split_segment_at_silence_short_segment() {
        // Segment shorter than max — returned as-is
        let segment = crate::audio::vad::SpeechSegment {
            samples: vec![0.1; 16000], // 1 second
            start_timestamp_ms: 0.0,
            end_timestamp_ms: 1000.0,
            confidence: 0.9,
            source: crate::audio::recording_state::DeviceType::Microphone,
        };
        let result = split_segment_at_silence(&segment, 25 * 16000);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].samples.len(), 16000);
    }

    #[test]
    fn test_split_segment_at_silence_splits_long_segment() {
        // 60-second segment of low-level noise with a silent gap at ~25s
        let mut samples = vec![0.01f32; 60 * 16000];
        // Insert silence at 25 seconds (sample 400000)
        for i in (25 * 16000)..(25 * 16000 + 3200) {
            samples[i] = 0.0;
        }
        let segment = crate::audio::vad::SpeechSegment {
            samples,
            start_timestamp_ms: 0.0,
            end_timestamp_ms: 60_000.0,
            confidence: 0.9,
            source: crate::audio::recording_state::DeviceType::Microphone,
        };

        let result = split_segment_at_silence(&segment, 25 * 16000);
        assert!(
            result.len() >= 2,
            "Should split into at least 2 segments, got {}",
            result.len()
        );

        // All sub-segments should have samples
        for (i, seg) in result.iter().enumerate() {
            assert!(!seg.samples.is_empty(), "Segment {} is empty", i);
            assert!(
                seg.start_timestamp_ms < seg.end_timestamp_ms,
                "Segment {} has invalid timestamps: {} >= {}",
                i,
                seg.start_timestamp_ms,
                seg.end_timestamp_ms
            );
        }
    }

    #[test]
    fn test_split_segment_at_silence_no_silence_uses_overlap() {
        // Continuous speech (constant energy) — should still split with overlap
        let segment = crate::audio::vad::SpeechSegment {
            samples: vec![0.5f32; 60 * 16000], // 60 seconds of "speech"
            start_timestamp_ms: 0.0,
            end_timestamp_ms: 60_000.0,
            confidence: 0.9,
            source: crate::audio::recording_state::DeviceType::Microphone,
        };

        let result = split_segment_at_silence(&segment, 25 * 16000);
        assert!(result.len() >= 2);

        // Total samples should exceed input due to overlap
        let total_samples: usize = result.iter().map(|s| s.samples.len()).sum();
        assert!(
            total_samples >= 60 * 16000,
            "Overlap should not lose samples"
        );
    }

    #[test]
    fn test_write_transcripts_json() {
        let dir = tempfile::tempdir().unwrap();
        let segments = vec![
            TranscriptSegment {
                id: "t-1".to_string(),
                text: "Hello world".to_string(),
                timestamp: Some("2024-01-01T00:00:00Z".to_string()),
                audio_start_time: Some(0.0),
                audio_end_time: Some(1.5),
                duration: Some(1.5),
                display_time: None,
                confidence: None,
                sequence_id: None,
                speaker: None,
                voice_profile_id: None,
                source: None,
            },
            TranscriptSegment {
                id: "t-2".to_string(),
                text: "Second segment".to_string(),
                timestamp: Some("2024-01-01T00:00:01Z".to_string()),
                audio_start_time: Some(2.0),
                audio_end_time: Some(3.5),
                duration: Some(1.5),
                display_time: None,
                confidence: None,
                sequence_id: None,
                speaker: None,
                voice_profile_id: None,
                source: None,
            },
        ];

        let result = write_transcripts_json(dir.path(), &segments);
        assert!(
            result.is_ok(),
            "write_transcripts_json failed: {:?}",
            result
        );

        // Verify file exists and is valid JSON
        let path = dir.path().join("transcripts.json");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["total_segments"], 2);
        assert_eq!(parsed["version"], "1.0");
        assert_eq!(parsed["segments"][0]["text"], "Hello world");
        assert_eq!(parsed["segments"][1]["text"], "Second segment");
        assert_eq!(parsed["segments"][0]["sequence_id"], 0);
        assert_eq!(parsed["segments"][1]["sequence_id"], 1);

        // Verify temp file was cleaned up
        assert!(!dir.path().join(".transcripts.json.tmp").exists());
    }

    #[test]
    fn test_write_import_metadata() {
        let dir = tempfile::tempdir().unwrap();

        let metadata = super::super::common::MeetingMetadata {
            version: Some("1.0".to_string()),
            meeting_id: Some("meeting-123".to_string()),
            meeting_name: Some("Test Meeting".to_string()),
            created_at: Some("2024-01-01T00:00:00Z".to_string()),
            completed_at: Some("2024-01-01T00:00:00Z".to_string()),
            duration_seconds: Some(1800.0),
            audio_file: Some("audio.mp4".to_string()),
            transcript_file: Some("transcripts.json".to_string()),
            status: Some("completed".to_string()),
            origin: Some("import".to_string()),
            ..Default::default()
        };
        let result =
            super::super::common::write_metadata(dir.path(), &metadata, || metadata.clone());
        assert!(result.is_ok(), "write_metadata failed: {:?}", result);

        let path = dir.path().join("metadata.json");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["version"], "1.0");
        assert_eq!(parsed["meeting_id"], "meeting-123");
        assert_eq!(parsed["meeting_name"], "Test Meeting");
        assert_eq!(parsed["duration_seconds"], 1800.0);
        assert_eq!(parsed["audio_file"], "audio.mp4");
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["origin"], "import");
    }

    /// Integration test that decodes a real audio file and runs VAD.
    /// Run with: TEST_AUDIO_PATH=/path/to/audio.mp4 cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_import_pipeline_decode_vad() {
        let audio_path = std::env::var("TEST_AUDIO_PATH")
            .expect("Set TEST_AUDIO_PATH to run this integration test");

        let path = Path::new(&audio_path);
        assert!(path.exists(), "Audio file not found: {}", audio_path);

        // Step 1: Decode
        println!("Decoding {}...", audio_path);
        let decoded =
            crate::audio::decoder::decode_audio_file(path).expect("Failed to decode audio file");
        println!(
            "Decoded: {:.2}s, {}Hz, {} channels, {} samples",
            decoded.duration_seconds,
            decoded.sample_rate,
            decoded.channels,
            decoded.samples.len()
        );

        // Step 2: Resample to 16kHz mono
        println!("Resampling to 16kHz mono...");
        let samples = decoded.to_whisper_format();
        println!(
            "Resampled: {} samples ({:.2}s at 16kHz)",
            samples.len(),
            samples.len() as f64 / 16000.0
        );

        // Step 3: Run VAD with both redemption times and compare
        for redemption_ms in [400u32, 2000] {
            println!("\n--- VAD with redemption_time={}ms ---", redemption_ms);
            let segments = crate::audio::vad::get_speech_chunks_with_progress(
                &samples,
                redemption_ms,
                |progress, count| {
                    if progress % 20 == 0 {
                        println!("  VAD progress: {}% ({} segments)", progress, count);
                    }
                    true
                },
            )
            .expect("VAD failed");

            let total_segments = segments.len();
            println!("Found {} segments", total_segments);

            if !segments.is_empty() {
                let durations: Vec<f64> = segments
                    .iter()
                    .map(|s| s.end_timestamp_ms - s.start_timestamp_ms)
                    .collect();
                let total_speech: f64 = durations.iter().sum();
                let avg = total_speech / durations.len() as f64;
                let min = durations.iter().cloned().fold(f64::INFINITY, f64::min);
                let max = durations.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

                println!(
                    "Stats: avg={:.0}ms, min={:.0}ms, max={:.0}ms, total_speech={:.1}s/{:.1}s ({:.0}%)",
                    avg, min, max,
                    total_speech / 1000.0,
                    decoded.duration_seconds,
                    (total_speech / 1000.0 / decoded.duration_seconds) * 100.0
                );

                // Segments over 25s that would be split
                let oversized = durations.iter().filter(|d| **d > 25_000.0).count();
                println!("Segments >25s (would be split): {}", oversized);

                // Basic sanity checks
                assert!(total_speech > 0.0, "No speech detected");
                for (i, seg) in segments.iter().enumerate() {
                    assert!(!seg.samples.is_empty(), "Segment {} has no samples", i);
                    assert!(
                        seg.end_timestamp_ms > seg.start_timestamp_ms,
                        "Segment {} has invalid timestamps",
                        i
                    );
                }
            }
        }
    }
}
