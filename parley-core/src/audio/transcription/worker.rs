// audio/transcription/worker.rs
//
// The transcription task: consumes VAD speech segments, splits them at
// speaker-turn boundaries, transcribes, diarizes, and emits transcript
// updates.
//
// Processing is deliberately SERIAL — one segment at a time, in arrival
// order. Ordered emission is what keeps the live transcript chronological,
// and the cross-source echo dedup (echo_dedup.rs) depends on segments being
// checked one at a time against previously-accepted ones.

use super::echo_dedup::{EchoDecision, EchoDedup};
use super::engine::TranscriptionEngine;
use crate::audio::recording_state::DeviceType;
use crate::audio::AudioChunk;
use crate::events::{EventSinkExt, SharedEventSink};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Granular error types for transcription operations.
#[derive(Debug, Clone)]
pub enum TranscriptionError {
    ModelNotLoaded,
    AudioTooShort { samples: usize, minimum: usize },
    EngineFailed(String),
    UnsupportedLanguage(String),
}

impl std::fmt::Display for TranscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelNotLoaded => write!(f, "No transcription model is loaded"),
            Self::AudioTooShort { samples, minimum } => write!(
                f,
                "Audio too short: {} samples (minimum {})",
                samples, minimum
            ),
            Self::EngineFailed(msg) => write!(f, "Transcription engine failed: {}", msg),
            Self::UnsupportedLanguage(lang) => {
                write!(f, "Language '{}' is not supported by this provider", lang)
            }
        }
    }
}

impl std::error::Error for TranscriptionError {}

// Sequence counter for transcript updates
static SEQUENCE_COUNTER: AtomicU64 = AtomicU64::new(0);

// Speech detection flag - reset per recording session
static SPEECH_DETECTED_EMITTED: AtomicBool = AtomicBool::new(false);

/// Reset the speech detected flag for a new recording session
pub fn reset_speech_detected_flag() {
    SPEECH_DETECTED_EMITTED.store(false, Ordering::SeqCst);
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TranscriptUpdate {
    pub text: String,
    pub timestamp: String, // Wall-clock time for reference (e.g., "14:30:05")
    /// Audio stream tag: "mic" or "system". Distinct from `speaker` (the
    /// human-readable label) — `source` is the raw stream identity.
    pub source: String,
    pub sequence_id: u64,
    pub chunk_start_time: f64, // Legacy field, kept for compatibility
    pub is_partial: bool,
    pub confidence: f32,
    // Recording-relative timestamps for playback sync
    pub audio_start_time: f64, // Seconds from recording start (e.g., 125.3)
    pub audio_end_time: f64,   // Seconds from recording start (e.g., 128.6)
    pub duration: f64,         // Segment duration in seconds (e.g., 3.3)
    /// Speaker label shown in the UI: a stored profile name or clustered
    /// "Speaker N" when a diarizer is loaded, else the source placeholder
    /// ("Me" for mic, "Speaker" for system).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    /// Foreign key to a stored voice profile when the embedding matched one.
    /// `None` for in-session-only clusters or unmatched audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_profile_id: Option<String>,
}

/// Resolve a default (source, speaker) pair from the audio stream's device type.
/// Used only when no diarizer is loaded — both mic and system streams pass
/// through the diarizer otherwise so multi-voice mic captures (open-mic +
/// speakers setups) get clustered correctly instead of all being labelled
/// as the local user.
pub fn default_speaker_for_source(source: DeviceType) -> (&'static str, &'static str) {
    match source {
        DeviceType::Microphone => ("mic", "Me"),
        DeviceType::System => ("system", "Speaker"),
    }
}

/// A speaker-turn sub-chunk queued for transcription, carrying the speaker
/// embedding the turn-splitter already computed for its samples (when it
/// did) so diarization doesn't embed the same audio twice.
struct PendingChunk {
    chunk: AudioChunk,
    embedding: Option<Vec<f32>>,
}

/// Split a VAD segment into speaker-turn sub-chunks so back-to-back speakers
/// (with no silence gap for VAD to cut on) get separate segments and labels.
///
/// Both streams are split: the system stream carries the remote participants
/// trading turns, and the mic stream carries every voice in the room in a
/// speakers-not-headphones setup — the same reason both streams are
/// diarized at all. Short segments, and the no-diarizer case, pass through
/// unchanged. The embedding runs off the async runtime (CPU-bound ONNX),
/// matching how the main diarization step is scheduled.
async fn split_chunk_by_speaker(chunk: AudioChunk) -> Vec<PendingChunk> {
    let Some(diarizer) = crate::speaker_diarization::current_diarizer() else {
        return vec![PendingChunk {
            chunk,
            embedding: None,
        }];
    };

    let samples = chunk.data.clone();
    let turns = match tokio::task::spawn_blocking(move || diarizer.speaker_turns(&samples)).await {
        Ok(turns) => turns,
        Err(_) => {
            return vec![PendingChunk {
                chunk,
                embedding: None,
            }]
        }
    };
    if turns.len() <= 1 {
        // Single speaker throughout (or too short to analyze) — keep the
        // original chunk, but reuse the whole-segment embedding if the
        // analysis produced one.
        let embedding = turns.into_iter().next().and_then(|t| t.embedding);
        return vec![PendingChunk { chunk, embedding }];
    }

    let sample_rate = chunk.sample_rate as f64;
    turns
        .into_iter()
        .map(|turn| PendingChunk {
            chunk: AudioChunk {
                data: chunk.data[turn.start..turn.end].to_vec(),
                sample_rate: chunk.sample_rate,
                // Offset the recording-relative timestamp into the segment.
                timestamp: chunk.timestamp + turn.start as f64 / sample_rate,
                chunk_id: chunk.chunk_id,
                device_type: chunk.device_type,
            },
            embedding: turn.embedding,
        })
        .collect()
}

/// Serial transcription task: drains the pipeline's segment channel until it
/// closes, processing every chunk in order. The channel-closed return of
/// `recv()` doubles as the completion signal — by then every queued chunk
/// has been pulled and processed, so nothing is ever lost.
pub fn start_transcription_task(
    sink: SharedEventSink,
    pool: Option<sqlx::SqlitePool>,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("🚀 Starting transcription task (serial, ordered emission)");

        // Hold the live-transcription lease for the entire lifetime of this
        // task, so no batch job (import / retranscription / auto-refine) can
        // swap or unload the shared Whisper engine's model out from under
        // this live recording. Released automatically when this async block
        // exits (loop break below, or an early return on init failure), i.e.
        // once the receive loop has fully drained. See
        // `whisper_engine::lease` for the full rationale.
        let _live_engine_lease = crate::whisper_engine::LIVE_ENGINE_LEASE.acquire_live();

        let engine = match super::engine::get_or_init_transcription_engine(pool.as_ref()).await {
            Ok(engine) => engine,
            Err(e) => {
                error!("Failed to initialize transcription engine: {}", e);
                let _ = sink.emit_event("transcription-error", &serde_json::json!({
                    "error": e,
                    "userMessage": "Recording failed: Unable to initialize speech recognition. Please check your model settings.",
                    "actionable": true
                }));
                return;
            }
        };

        if engine.is_model_loaded().await {
            let model = engine
                .get_current_model()
                .await
                .unwrap_or_else(|| "unknown".to_string());
            info!(
                "✅ Transcription task: {} model '{}' is loaded and ready",
                engine.provider_name(),
                model
            );
        } else {
            warn!(
                "⚠️ Transcription task: {} model not loaded - chunks may be skipped",
                engine.provider_name()
            );
        }

        // NOTE: we deliberately do NOT feed a rolling transcript tail to
        // whisper as an initial prompt on the live path. Live VAD chunks are
        // short, and whisper.cpp readily regurgitates a growing prompt back
        // into its output on short audio — producing duplicated,
        // ever-lengthening lines littered with "..." continuation markers
        // (it thinks it's mid-sentence). Each chunk is transcribed
        // independently instead.

        // Cross-source echo dedup state (mic bleed of system audio, or vice
        // versa, in speaker-not-headphones setups). See echo_dedup.rs for
        // the full rationale and policy. Lives per recording session.
        let mut echo_dedup = EchoDedup::new();

        // Speaker-turn sub-chunks of the segment currently being processed,
        // drained before pulling the next VAD segment off the channel.
        // Splitting here (rather than inside process_chunk) lets every turn
        // flow through the normal transcribe -> diarize -> emit path with
        // its own sequence id.
        let mut pending: VecDeque<PendingChunk> = VecDeque::new();
        let mut processed: u64 = 0;

        loop {
            let item = match pending.pop_front() {
                Some(item) => item,
                None => match receiver.recv().await {
                    Some(segment) => {
                        // Segment left the transcription queue — see
                        // transcription::queue for the accounting this feeds
                        // (issue #26: queue-depth visibility).
                        super::queue::dequeued();
                        pending.extend(split_chunk_by_speaker(segment).await);
                        match pending.pop_front() {
                            Some(item) => item,
                            None => continue,
                        }
                    }
                    // Channel closed and fully drained: the pipeline dropped
                    // its sender after flushing, and every queued chunk has
                    // been processed.
                    None => break,
                },
            };

            process_chunk(&engine, item, &sink, &mut echo_dedup).await;
            processed += 1;
        }

        info!(
            "✅ Transcription task completed - all {} chunks processed, ready for model unload",
            processed
        );
    })
}

/// Transcribe, filter, diarize, and emit a single (sub-)chunk.
async fn process_chunk(
    engine: &TranscriptionEngine,
    item: PendingChunk,
    sink: &dyn crate::events::EventSink,
    echo_dedup: &mut EchoDedup,
) {
    let PendingChunk { chunk, embedding } = item;

    // Reduce logging in the hot path: only log every 10th chunk.
    let should_log_this_chunk = chunk.chunk_id % 10 == 0;
    if should_log_this_chunk {
        info!(
            "👷 Processing chunk {} with {} samples",
            chunk.chunk_id,
            chunk.data.len()
        );
    }

    if !engine.is_model_loaded().await {
        warn!(
            "⚠️ Model unloaded, skipping chunk {}",
            chunk.chunk_id
        );
        return;
    }

    let chunk_timestamp = chunk.timestamp;
    let chunk_duration = chunk.data.len() as f64 / chunk.sample_rate as f64;
    // Capture source identity before chunk is moved into the transcription call.
    let chunk_source = chunk.device_type;
    // Samples for the diarizer to embed after transcription completes — only
    // needed when the turn-splitter didn't already hand us an embedding for
    // exactly these samples. Routed for *both* mic and system sources: in
    // any setup where the user is on speakers (instead of headphones), the
    // mic captures everyone in the room, and an asymmetric "mic = Me always"
    // placeholder would mis-attribute them all to the local user.
    //
    // The "Me" fallback in `default_speaker_for_source` still applies for
    // users who haven't downloaded the speaker model (no diarizer loaded).
    let diarization_samples = if embedding.is_none()
        && crate::speaker_diarization::current_diarizer().is_some()
    {
        Some(chunk.data.clone())
    } else {
        None
    };

    let (transcript, confidence_opt, is_partial) =
        match transcribe_chunk_with_provider(engine, chunk, sink, None).await {
            Ok(result) => result,
            Err(e) => {
                match e {
                    TranscriptionError::AudioTooShort { .. } => {
                        // Expected for very short chunks; skip silently.
                        info!("{}", e);
                    }
                    TranscriptionError::ModelNotLoaded => {
                        warn!("Model unloaded during transcription");
                    }
                    _ => {
                        warn!("Transcription failed: {}", e);
                        let _ = sink.emit_event("transcription-warning", &e.to_string());
                    }
                }
                return;
            }
        };

    // Provider-aware confidence threshold
    let confidence_threshold = match engine {
        TranscriptionEngine::Whisper(_) => 0.3,
    };
    let confidence_str = match confidence_opt {
        Some(c) => format!("{:.2}", c),
        None => "N/A".to_string(),
    };

    // Check confidence threshold (or accept if no confidence provided)
    let meets_threshold = confidence_opt.map_or(true, |c| c >= confidence_threshold);

    // Whisper's classic hallucinations on marginal audio: stock phrases and
    // bracketed sound markers with mediocre decoder confidence.
    let hallucinated = is_likely_hallucination(&transcript, confidence_opt.unwrap_or(1.0));
    if hallucinated {
        info!(
            "Dropping likely hallucination: '{}' (confidence: {})",
            transcript, confidence_str
        );
    }

    // Recording-relative timestamps, shared by the echo-dedup check and the
    // emitted TranscriptUpdate.
    let audio_start_time = chunk_timestamp;
    let audio_end_time = chunk_timestamp + chunk_duration;

    // Cross-source echo suppression: only worth running for segments that
    // would otherwise be accepted — everything else is already headed for
    // the drop path, so skip the wasted check and don't pollute the dedup
    // buffer with it.
    let is_echo = !transcript.trim().is_empty()
        && meets_threshold
        && !hallucinated
        && echo_dedup.check(chunk_source, &transcript, audio_start_time, audio_end_time)
            == EchoDecision::DropAsMicEcho;

    if transcript.trim().is_empty() || !meets_threshold || hallucinated || is_echo {
        // Echo drops are already logged inside echo_dedup.check().
        if !is_echo && !transcript.trim().is_empty() && should_log_this_chunk {
            if let Some(c) = confidence_opt {
                info!(
                    "Low-confidence transcription (confidence: {:.2}), skipping",
                    c
                );
            }
        }
        return;
    }

    info!(
        "✅ Transcribed: {} (confidence: {}, partial: {})",
        transcript, confidence_str, is_partial
    );

    // Emit speech-detected once per session for frontend UX.
    if !SPEECH_DETECTED_EMITTED.swap(true, Ordering::SeqCst) {
        match sink.emit_event(
            "speech-detected",
            &serde_json::json!({ "message": "Speech activity detected" }),
        ) {
            Ok(_) => info!("🎤 First speech detected - emitted speech-detected event"),
            Err(e) => error!("🎤 Failed to emit speech-detected event: {}", e),
        }
    }

    let sequence_id = SEQUENCE_COUNTER.fetch_add(1, Ordering::SeqCst);

    // Diarize: either replay the turn-splitter's precomputed embedding or
    // embed the snapshot. The result is a stored profile name (embedding
    // matched a saved voice profile above threshold) or "Speaker N" from
    // in-session clustering. When no diarizer is loaded, fall back to the
    // source placeholder ("Me" for mic, "Speaker" for system).
    //
    // The local user reads as "Me" here too, without a branch: if they've
    // enrolled their voice in settings, their profile is named "Me" and
    // matches like any other stored speaker. See
    // `speaker_diarization::enrollment`.
    let (source_tag, default_speaker) = default_speaker_for_source(chunk_source);
    let diarization_result = match (
        crate::speaker_diarization::current_diarizer(),
        embedding.is_some() || diarization_samples.is_some(),
    ) {
        (Some(diarizer), true) => {
            // Speaker embedding is CPU-bound ONNX inference; run it off the
            // async runtime so it can't stall other tasks (same reasoning
            // as whisper inference).
            let samples = diarization_samples.unwrap_or_default();
            tokio::task::spawn_blocking(move || diarizer.process(sequence_id, &samples, embedding))
                .await
                .ok()
                .and_then(|res| match res {
                    Ok(result) => Some(result),
                    Err(e) => {
                        log::debug!("Diarization fallback for chunk {}: {}", sequence_id, e);
                        None
                    }
                })
        }
        _ => None,
    };
    let diarized = diarization_result.is_some();
    let (speaker_label, voice_profile_id) = match diarization_result {
        Some(r) => (r.label, r.voice_profile_id),
        None => (default_speaker.to_string(), None),
    };
    // Per-segment attribution trace. Debug-level so it's silent by default;
    // enable with RUST_LOG=app_lib::audio::transcription::worker=debug to
    // see whether live labels come from the diarizer (matched profile /
    // "Speaker N") or the source fallback ("Me" / "Speaker").
    log::debug!(
        "Attribution: source={:?} speaker='{}' profile={:?} diarized={}",
        chunk_source,
        speaker_label,
        voice_profile_id,
        diarized
    );

    let update = TranscriptUpdate {
        text: transcript,
        timestamp: format_current_timestamp(), // Wall-clock for reference
        source: source_tag.to_string(),
        sequence_id,
        chunk_start_time: chunk_timestamp, // Legacy compatibility
        is_partial,
        confidence: confidence_opt.unwrap_or(0.85), // Default for providers without confidence
        audio_start_time,
        audio_end_time,
        duration: chunk_duration,
        speaker: Some(speaker_label),
        voice_profile_id,
    };

    // Record for future cross-source echo checks. Only accepted (emitted)
    // segments are recorded — a suppressed echo should never itself become
    // a match target.
    echo_dedup.record(chunk_source, &update.text, audio_start_time, audio_end_time);

    // Persistence first (in-process, synchronous), then the UI.
    crate::audio::transcript_bus::publish(&update);
    if let Err(e) = sink.emit_event("transcript-update", &update) {
        error!("Failed to emit transcript update: {}", e);
    }
}

/// Stock phrases whisper reliably hallucinates on marginal/near-silent
/// audio (trained-in subtitle credits and filler), plus bracketed sound
/// markers like "[Music]" / "(applause)". Only applied below a confidence
/// bar so genuine short utterances with a confident decode survive.
pub fn is_likely_hallucination(text: &str, confidence: f32) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false; // empty is handled separately
    }

    // Bracket-only output is a sound-event marker, never meeting speech.
    let bracket_only = (trimmed.starts_with('[') && trimmed.ends_with(']'))
        || (trimmed.starts_with('(') && trimmed.ends_with(')'))
        || (trimmed.starts_with('♪') && trimmed.ends_with('♪'));
    if bracket_only {
        return true;
    }

    // Confident decodes are trusted even if the text matches a stock phrase.
    if confidence >= 0.5 {
        return false;
    }

    const STOCK_PHRASES: &[&str] = &[
        "you",
        "bye",
        "thank you",
        "thanks for watching",
        "thank you for watching",
        "thank you very much",
        "please subscribe",
        "subtitles by the amara.org community",
        "www.mooji.org",
    ];
    let normalized: String = trimmed
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(|w| w.trim_matches('.'))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    STOCK_PHRASES.contains(&normalized.as_str())
}

/// Transcribe audio chunk using the configured engine.
/// Returns: (text, confidence Option, is_partial)
async fn transcribe_chunk_with_provider(
    engine: &TranscriptionEngine,
    chunk: AudioChunk,
    sink: &dyn crate::events::EventSink,
    context_prompt: Option<String>,
) -> std::result::Result<(String, Option<f32>, bool), TranscriptionError> {
    // Convert to 16kHz mono for transcription
    let transcription_data = if chunk.sample_rate != 16000 {
        crate::audio::audio_processing::resample_audio(&chunk.data, chunk.sample_rate, 16000)
            .map_err(|e| {
                TranscriptionError::EngineFailed(format!(
                    "Resampling chunk {} from {}Hz to 16000Hz failed: {}",
                    chunk.chunk_id, chunk.sample_rate, e
                ))
            })?
    } else {
        chunk.data
    };

    // Skip VAD processing here since the pipeline already extracted speech using VAD
    let speech_samples = transcription_data;

    // Check for empty samples - improved error handling
    if speech_samples.is_empty() {
        warn!(
            "Audio chunk {} is empty, skipping transcription",
            chunk.chunk_id
        );
        return Err(TranscriptionError::AudioTooShort {
            samples: 0,
            minimum: 1600, // 100ms at 16kHz
        });
    }

    // Calculate energy for logging + the system-audio silence gate below.
    let energy: f32 =
        speech_samples.iter().map(|&x| x * x).sum::<f32>() / speech_samples.len() as f32;
    info!(
        "Processing speech audio chunk {} with {} samples (energy: {:.6})",
        chunk.chunk_id,
        speech_samples.len(),
        energy
    );

    // Drop near-silent system-audio chunks. When nothing is actually playing
    // through the speakers, the system loopback still picks up faint mic
    // bleed (typically ~0.0001 RMS² range). VAD sees enough variation to
    // call it speech; Whisper hallucinates words from the near-silence.
    // Real system audio (a remote participant on a call) lands ~10-100x
    // higher in energy than this floor.
    //
    // Threshold picked empirically: observed mic speech ~0.005, mic-bleed
    // system ~0.0001; 0.0005 sits comfortably between.
    const SYSTEM_AUDIO_SILENCE_THRESHOLD: f32 = 0.0005;
    if chunk.device_type == DeviceType::System && energy < SYSTEM_AUDIO_SILENCE_THRESHOLD {
        info!(
            "Dropping near-silent system audio chunk {} (energy {:.6} < threshold {:.6})",
            chunk.chunk_id, energy, SYSTEM_AUDIO_SILENCE_THRESHOLD
        );
        // Return empty text; downstream callers already treat empty as "no
        // segment to emit", same path as Whisper returning "".
        return Ok((String::new(), Some(1.0), false));
    }

    // Transcribe using the appropriate engine (with improved error handling)
    match engine {
        TranscriptionEngine::Whisper(whisper_engine) => {
            // Get language preference from global state
            let language = crate::utils::get_language_preference_internal();

            match whisper_engine
                .transcribe_audio_with_confidence(speech_samples, language, context_prompt)
                .await
            {
                Ok((text, confidence, is_partial)) => {
                    let cleaned_text = text.trim().to_string();
                    if cleaned_text.is_empty() {
                        return Ok((String::new(), Some(confidence), is_partial));
                    }

                    info!(
                        "Whisper transcription complete for chunk {}: '{}' (confidence: {:.2}, partial: {})",
                        chunk.chunk_id, cleaned_text, confidence, is_partial
                    );

                    Ok((cleaned_text, Some(confidence), is_partial))
                }
                Err(e) => {
                    error!(
                        "Whisper transcription failed for chunk {}: {}",
                        chunk.chunk_id, e
                    );

                    // A missing model (batch job swapped it, or shutdown
                    // unloaded it) is the quiet-skip case the caller handles
                    // as `ModelNotLoaded`; anything else is a real engine
                    // failure worth a toast.
                    let transcription_error = if e.to_string().contains("No model loaded") {
                        TranscriptionError::ModelNotLoaded
                    } else {
                        TranscriptionError::EngineFailed(e.to_string())
                    };
                    if matches!(transcription_error, TranscriptionError::ModelNotLoaded) {
                        return Err(transcription_error);
                    }
                    let _ = sink.emit_event(
                        "transcription-error",
                        &serde_json::json!({
                            "error": transcription_error.to_string(),
                            "userMessage": format!("Transcription failed: {}", transcription_error),
                            "actionable": false
                        }),
                    );

                    Err(transcription_error)
                }
            }
        }
    }
}

/// Format the current wall-clock time in the user's local timezone
/// (`HH:MM:SS`). Shown next to each transcript line, so it must match the
/// clock the user is looking at, not UTC.
fn format_current_timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::is_likely_hallucination;

    #[test]
    fn stock_phrases_dropped_at_low_confidence() {
        assert!(is_likely_hallucination("you", 0.2));
        assert!(is_likely_hallucination(" Thank you. ", 0.35));
        assert!(is_likely_hallucination("Thanks for watching!", 0.4));
    }

    #[test]
    fn confident_short_utterances_survive() {
        assert!(!is_likely_hallucination("you", 0.8));
        assert!(!is_likely_hallucination("Thank you.", 0.72));
    }

    #[test]
    fn real_speech_survives_at_any_confidence() {
        assert!(!is_likely_hallucination("I'm getting sick", 0.27));
        assert!(!is_likely_hallucination(
            "Sounds good, let's do that.",
            0.31
        ));
    }

    #[test]
    fn sound_markers_always_dropped() {
        assert!(is_likely_hallucination("[Music]", 0.9));
        assert!(is_likely_hallucination("(applause)", 0.95));
        assert!(is_likely_hallucination("♪ la la la ♪", 0.9));
    }

    #[test]
    fn empty_is_not_flagged() {
        assert!(!is_likely_hallucination("   ", 0.1));
    }
}
