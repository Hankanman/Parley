//! "Record my voice" — enrollment of the local user's own voice profile.
//!
//! ## The problem this solves
//!
//! When no speaker model is downloaded, mic-source segments fall back to the
//! "Me" placeholder (see `audio::transcription::worker::default_speaker_for_source`)
//! and everything reads fine. The moment a diarizer *is* loaded, that
//! placeholder stops applying: the mic stream is clustered like any other,
//! because in a speakers-in-a-room setup the mic picks up everyone. Correct
//! for the room, wrong for the user — their own voice becomes "Speaker 3".
//!
//! Enrollment fixes it the same way the app already recognizes any returning
//! speaker: with a stored voice profile. The user records ~20s of their own
//! speech outside a meeting, we embed it, and save the centroid as a profile
//! flagged `is_self` and named [`SELF_SPEAKER_LABEL`] ("Me"). From then on the
//! ordinary profile matcher recognizes them — no special-casing in the
//! diarizer, no per-recording state, and it works on both mic and system
//! sources (a user dialled in through the meeting's speakers still matches).
//!
//! Enrollment is entirely optional: with no self profile, behaviour is exactly
//! as it is today.
//!
//! ## Why capture happens here in Rust, not in the webview
//!
//! The obvious alternative is `MediaRecorder`/`getUserMedia` in the frontend,
//! handing a blob to Rust to decode. We capture natively instead because:
//!
//! - **Same audio path as a real meeting.** Enrollment embeddings must live in
//!   the same space as meeting embeddings or matching degrades. Capturing via
//!   [`PwCaptureStream`] means the exact device, format negotiation, and
//!   resample chain the recording pipeline uses — not WebKitGTK's mixer with
//!   its own AGC/AEC and codec.
//! - **Same device selection.** The user already picked a mic in Recording
//!   settings; enrollment takes that node id. The webview only knows about
//!   browser device ids.
//! - **No permission surface, no decode step.** getUserMedia under WebKitGTK
//!   needs its own permission plumbing, and a blob would need decoding back to
//!   f32 anyway.
//!
//! The flow is a small state machine over one PipeWire stream:
//! [`start_self_voice_enrollment`] opens it and accumulates mono 48 kHz
//! samples while emitting `self-voice-enrollment-progress` for the level meter
//! and countdown; [`finish_self_voice_enrollment`] stops it and converts the
//! buffer into a profile; [`cancel_self_voice_enrollment`] throws it away.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::audio::audio_processing::{audio_to_mono, resample_audio};
use crate::audio::pw::{PwCaptureStream, PwStreamEvent, CAPTURE_CHANNELS, CAPTURE_RATE};
use crate::audio::recording_state::DeviceType;
use crate::audio::stream::capture_target_for;
use crate::audio::vad::extract_enrollment_speech_16k;
use crate::database::repositories::voice_profile::VoiceProfilesRepository;
use crate::events::{EventSinkExt, SharedEventSink};
use crate::speaker_diarization::embedding_math::average_and_normalize;
use crate::speaker_diarization::{
    default_model_path, model::model_is_ready, SpeakerEmbedder, SELF_SPEAKER_LABEL,
};

/// Sample rate the embedder consumes.
const EMBED_RATE: u32 = 16_000;

/// How long the UI asks the user to speak for. The frontend counts down from
/// this; the backend doesn't stop at it (the user may keep talking), it only
/// reports it so both ends agree on the target.
const TARGET_CAPTURE_SECS: f32 = 20.0;

/// Hard cap on buffered audio. Guards against a "Record" that's never stopped
/// (~5.5 MB of f32 at 48 kHz mono). Capture continues past this, we just stop
/// growing the buffer — 30s is already well past the point of diminishing
/// returns for a centroid.
const MAX_CAPTURE_SECS: f32 = 30.0;

/// Minimum wall-clock audio before we'll build a profile.
const MIN_CAPTURE_SECS: f32 = 10.0;

/// Minimum *voiced* audio (post-VAD) required. A 20s recording of someone
/// silently staring at the button passes the duration check but produces a
/// meaningless centroid, so gate on speech, not on time.
const MIN_SPEECH_SECS: f32 = 5.0;

/// Window the enrollment audio is diced into before embedding. Roughly matches
/// the length of a typical VAD-emitted meeting segment, keeping enrollment
/// embeddings in the same regime as the ones they'll be matched against.
const WINDOW_SECS: f32 = 4.0;

/// A trailing remainder shorter than this is dropped rather than embedded —
/// sherpa needs a meaningful chunk to produce a stable embedding.
const MIN_WINDOW_SECS: f32 = 2.0;

/// RMS floor for "this is audio, not a dead mic". Deliberately low: the VAD
/// speech check above is the real gate, this only catches a totally silent
/// device so the user gets "we heard nothing" instead of a garbage profile.
const MIN_RMS: f32 = 0.005;

/// Peak amplitude above which, if the VAD trimmed the recording below
/// [`MIN_SPEECH_SECS`], we still enroll on the raw buffer rather than reject
/// it. Distinguishes "clearly talking but the VAD was over-eager" (fall back)
/// from "genuinely too quiet" (report it). Well below normal speech peaks
/// (~0.2–0.8) yet safely above idle-mic noise.
const ENROLL_FALLBACK_PEAK: f32 = 0.03;

/// Progress tick for the enrollment UI (level meter + elapsed).
///
/// Deserialize is for the GPUI shell, which re-decodes this from the
/// `self-voice-enrollment-progress` event's JSON payload (see
/// `core_events::CoreEvent::decode`); the Tauri frontend only ever needs
/// the Serialize side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentProgress {
    pub rms_level: f32,
    pub peak_level: f32,
    /// Audio captured so far, in seconds.
    pub captured_secs: f32,
    /// How long we're asking them to speak for.
    pub target_secs: f32,
    /// True once `captured_secs` passes the minimum, i.e. "Save" can be
    /// enabled. Sent so the frontend doesn't duplicate the threshold.
    pub can_save: bool,
}

/// Current state of the local user's enrollment, for the settings screen.
#[derive(Debug, Clone, Serialize)]
pub struct SelfVoiceStatus {
    pub enrolled: bool,
    pub profile_id: Option<String>,
    pub name: Option<String>,
    /// Number of embedding windows behind the stored centroid.
    pub sample_count: Option<i64>,
    pub updated_at: Option<String>,
    /// Whether the speaker embedding model is on disk. Without it we can
    /// neither enroll nor match, so the UI explains that instead of offering a
    /// Record button that would fail.
    pub model_ready: bool,
}

struct EnrollmentSession {
    stream: PwCaptureStream,
    /// Mono 48 kHz samples accumulated by the capture callback.
    buffer: Arc<Mutex<Vec<f32>>>,
}

/// Bumped on every start/stop; invalidates the progress-emit task of any
/// previous session (same approach as `audio::simple_level_monitor`).
static GENERATION: AtomicU64 = AtomicU64::new(0);

static SESSION: OnceLock<Mutex<Option<EnrollmentSession>>> = OnceLock::new();

fn session_slot() -> &'static Mutex<Option<EnrollmentSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/// Stop the capture stream and return whatever it buffered.
fn take_current_session() -> Option<Vec<f32>> {
    let session = session_slot().lock().ok()?.take()?;
    session.stream.stop();
    let samples = session.buffer.lock().map(|b| b.clone()).unwrap_or_default();
    Some(samples)
}

/// Begin capturing the user's voice from `mic_device` (a PipeWire node id, or
/// `None`/`"default"` for the system default source).
///
/// Emits `self-voice-enrollment-progress` every 100ms until finished or
/// cancelled. Starting twice is safe: the previous session is discarded.
///
/// Tauri-free core — opens the capture stream and spawns the
/// progress-emitting task against `sink` instead of an `AppHandle`. See
/// `speaker_diarization::enrollment_commands` for the `#[tauri::command]`
/// wrapper.
pub async fn start_self_voice_enrollment_with_sink(
    sink: SharedEventSink,
    mic_device: Option<String>,
) -> Result<(), String> {
    // The mic is capturable by more than one PipeWire client at once, so this
    // would technically "work" mid-meeting — but the user would be enrolling
    // over the top of a live conversation, which is neither what they mean nor
    // clean audio.
    if crate::audio::recording_service::is_recording().await {
        return Err("Stop the current recording before enrolling your voice".into());
    }

    // Fail fast rather than letting the user talk for 20s into a capture we
    // can't turn into a profile. The model is loaded at finish, not here.
    if !default_model_path()
        .map(|p| model_is_ready(&p))
        .unwrap_or(false)
    {
        return Err(
            "The speaker model isn't downloaded yet — install it before enrolling your voice"
                .into(),
        );
    }

    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    stop_current_session();

    let device = mic_device.unwrap_or_else(|| "default".to_string());
    let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let levels: Arc<Mutex<(f32, f32)>> = Arc::new(Mutex::new((0.0, 0.0)));
    let max_samples = (MAX_CAPTURE_SECS * CAPTURE_RATE as f32) as usize;

    let buffer_for_cb = buffer.clone();
    let levels_for_cb = levels.clone();
    let device_for_open = device.clone();

    // Opening the stream blocks on a PipeWire roundtrip — keep it off the
    // async runtime.
    let stream = tokio::task::spawn_blocking(move || {
        let target = capture_target_for(&device_for_open, DeviceType::Microphone);
        PwCaptureStream::open(
            target,
            Box::new(move |samples: &[f32]| {
                if samples.is_empty() {
                    return;
                }
                let rms =
                    (samples.iter().map(|&x| x * x).sum::<f32>() / samples.len() as f32).sqrt();
                let peak = samples.iter().map(|&x| x.abs()).fold(0.0_f32, f32::max);
                if let Ok(mut l) = levels_for_cb.lock() {
                    *l = (rms.min(1.0), peak.min(1.0));
                }

                // Downmix in the callback: the centroid only ever needs mono,
                // and halving the buffer here keeps the capped allocation small.
                let mono = audio_to_mono(samples, CAPTURE_CHANNELS as u16);
                if let Ok(mut b) = buffer_for_cb.lock() {
                    let room = max_samples.saturating_sub(b.len());
                    if room > 0 {
                        b.extend_from_slice(&mono[..room.min(mono.len())]);
                    }
                }
            }),
            Box::new(move |event| {
                if !matches!(event, PwStreamEvent::Ended) {
                    log::warn!("Voice enrollment: capture stream event: {:?}", event);
                }
            }),
        )
    })
    .await
    .map_err(|e| format!("Enrollment capture task failed: {}", e))?
    .map_err(|e| format!("Could not open microphone '{}': {}", device, e))?;

    if let Ok(mut guard) = session_slot().lock() {
        *guard = Some(EnrollmentSession {
            stream,
            buffer: buffer.clone(),
        });
    }

    log::info!("Voice enrollment: capture started on '{}'", device);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        let min_samples = (MIN_CAPTURE_SECS * CAPTURE_RATE as f32) as usize;
        while GENERATION.load(Ordering::SeqCst) == generation {
            interval.tick().await;
            let captured = buffer.lock().map(|b| b.len()).unwrap_or(0);
            let (rms, peak) = levels.lock().map(|l| *l).unwrap_or((0.0, 0.0));
            let progress = EnrollmentProgress {
                rms_level: rms,
                peak_level: peak,
                captured_secs: captured as f32 / CAPTURE_RATE as f32,
                target_secs: TARGET_CAPTURE_SECS,
                can_save: captured >= min_samples,
            };
            if sink
                .emit_event("self-voice-enrollment-progress", &progress)
                .is_err()
            {
                break;
            }
        }
        log::debug!(
            "Voice enrollment: progress task exiting (generation {})",
            generation
        );
    });

    Ok(())
}

/// Discard an in-progress enrollment capture. Safe to call when nothing is
/// running.
pub async fn cancel_self_voice_enrollment() -> Result<(), String> {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    tokio::task::spawn_blocking(stop_current_session)
        .await
        .map_err(|e| format!("Failed to stop enrollment capture: {}", e))?;
    log::info!("Voice enrollment: cancelled");
    Ok(())
}

/// Stop capturing and turn the recording into the self voice profile,
/// replacing any previous one. Returns the resulting status.
pub async fn finish_self_voice_enrollment_with_pool(
    pool: &SqlitePool,
    name: Option<String>,
) -> Result<SelfVoiceStatus, String> {
    GENERATION.fetch_add(1, Ordering::SeqCst);

    let samples = tokio::task::spawn_blocking(take_current_session)
        .await
        .map_err(|e| format!("Failed to stop enrollment capture: {}", e))?
        .ok_or_else(|| "No enrollment recording in progress".to_string())?;

    let model_path = default_model_path()
        .filter(|p| model_is_ready(p))
        .ok_or_else(|| {
            "The speaker model isn't downloaded yet — install it before enrolling your voice"
                .to_string()
        })?;

    // Resample + VAD + ONNX inference are all CPU-bound; keep them off the
    // async runtime (same reasoning as the transcription worker's embedding
    // call).
    let (centroid, window_count) =
        tokio::task::spawn_blocking(move || build_centroid(&samples, &model_path))
            .await
            .map_err(|e| format!("Enrollment processing task failed: {}", e))?
            .map_err(|e| e.to_string())?;

    let label = self_label_or_default(name.as_deref());
    let profile_id = VoiceProfilesRepository::upsert_self(
        pool,
        &label,
        &centroid,
        window_count as i64,
    )
    .await
    .map_err(|e| format!("Failed to save your voice profile: {}", e))?;

    log::info!(
        "Voice enrollment: saved self profile {} from {} windows (dim={})",
        profile_id,
        window_count,
        centroid.len()
    );

    // The profile matcher is built once per recording session, so an enrollment
    // done while a diarizer is loaded takes effect on the next recording. That
    // is fine — enrollment is blocked during recording anyway.
    self_voice_status_with_pool(pool).await
}

/// The label the user's own voice shows up as. Trims the supplied name and
/// falls back to [`SELF_SPEAKER_LABEL`] ("Me") when it's blank, so there is
/// always a sensible label. Capped so a pathological name can't bloat every
/// transcript line.
fn self_label_or_default(name: Option<&str>) -> String {
    let trimmed = name.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        SELF_SPEAKER_LABEL.to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}

/// Rename the enrolled self profile without re-recording. A blank name resets
/// the label to [`SELF_SPEAKER_LABEL`] ("Me"). Returns the updated status.
pub async fn rename_self_voice_profile_with_pool(
    pool: &SqlitePool,
    name: String,
) -> Result<SelfVoiceStatus, String> {
    let profile = VoiceProfilesRepository::get_self(pool)
        .await
        .map_err(|e| format!("Failed to load your voice profile: {}", e))?
        .ok_or_else(|| "You haven't enrolled your voice yet".to_string())?;

    let label = self_label_or_default(Some(&name));
    VoiceProfilesRepository::update_profile(pool, &profile.id, &label, profile.email.as_deref())
        .await
        .map_err(|e| format!("Failed to rename your voice profile: {}", e))?;

    log::info!(
        "Voice enrollment: renamed self profile {} to '{}'",
        profile.id,
        label
    );

    self_voice_status_with_pool(pool).await
}

/// Whether the user has enrolled their voice, plus enough detail for the
/// settings screen to describe it.
pub async fn self_voice_status_with_pool(pool: &SqlitePool) -> Result<SelfVoiceStatus, String> {
    let model_ready = default_model_path()
        .map(|p| model_is_ready(&p))
        .unwrap_or(false);

    let profile = VoiceProfilesRepository::get_self(pool)
        .await
        .map_err(|e| format!("Failed to load your voice profile: {}", e))?;

    Ok(match profile {
        Some(p) => SelfVoiceStatus {
            enrolled: true,
            profile_id: Some(p.id),
            name: Some(p.name),
            sample_count: Some(p.sample_count),
            updated_at: Some(p.updated_at),
            model_ready,
        },
        None => SelfVoiceStatus {
            enrolled: false,
            profile_id: None,
            name: None,
            sample_count: None,
            updated_at: None,
            model_ready,
        },
    })
}

/// Delete the enrolled self profile. Past transcripts keep their "Me" labels
/// (the text is still true) but stop being linked to the profile; future
/// meetings fall back to clustering the user as "Speaker N" again.
pub async fn delete_self_voice_profile_with_pool(pool: &SqlitePool) -> Result<bool, String> {
    let Some(profile) = VoiceProfilesRepository::get_self(pool)
        .await
        .map_err(|e| format!("Failed to load your voice profile: {}", e))?
    else {
        return Ok(false);
    };

    let deleted = VoiceProfilesRepository::delete(pool, &profile.id)
        .await
        .map_err(|e| format!("Failed to delete your voice profile: {}", e))?;
    log::info!("Voice enrollment: removed self profile {}", profile.id);
    Ok(deleted)
}

fn stop_current_session() {
    if let Ok(mut guard) = session_slot().lock() {
        if let Some(session) = guard.take() {
            session.stream.stop();
        }
    }
}

/// Turn a mono 48 kHz enrollment recording into an L2-normalized centroid.
/// Returns `(centroid, window_count)`.
fn build_centroid(
    samples_48k_mono: &[f32],
    model_path: &std::path::Path,
) -> Result<(Vec<f32>, usize)> {
    let captured_secs = samples_48k_mono.len() as f32 / CAPTURE_RATE as f32;
    if captured_secs < MIN_CAPTURE_SECS {
        return Err(anyhow!(
            "Only {:.0}s recorded — keep talking for at least {:.0}s so we can learn your voice",
            captured_secs,
            MIN_CAPTURE_SECS
        ));
    }

    let rms = (samples_48k_mono.iter().map(|&x| x * x).sum::<f32>()
        / samples_48k_mono.len() as f32)
        .sqrt();
    if rms < MIN_RMS {
        return Err(anyhow!(
            "That recording is silent — check your microphone is selected and unmuted"
        ));
    }

    let samples_16k = resample_audio(samples_48k_mono, CAPTURE_RATE, EMBED_RATE).map_err(|e| {
        anyhow!(
            "Resampling enrollment audio to {}Hz failed: {}",
            EMBED_RATE,
            e
        )
    })?;

    // Trim to speech. The buffer already passed the RMS gate above, so it
    // demonstrably contains audio; if the VAD keeps too little (or is
    // unavailable) we enroll on the raw buffer rather than reject the user's
    // recording outright. A slightly noisier centroid beats a baffling "no
    // speech detected" when they were clearly talking, and the per-window
    // embedding below still skips pure-silence windows.
    let peak = samples_16k.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
    let speech = match extract_enrollment_speech_16k(&samples_16k) {
        Ok(s) if (s.len() as f32 / EMBED_RATE as f32) >= MIN_SPEECH_SECS => s,
        Ok(s) if peak >= ENROLL_FALLBACK_PEAK => {
            log::warn!(
                "Voice enrollment: VAD kept only {:.1}s of {:.1}s (peak {:.3}); \
                 enrolling on the full recording",
                s.len() as f32 / EMBED_RATE as f32,
                captured_secs,
                peak
            );
            samples_16k.clone()
        }
        Ok(s) => {
            // Little speech *and* a weak signal — genuinely too quiet.
            return Err(anyhow!(
                "Only {:.0}s of speech detected in {:.0}s of audio — move closer to the mic \
                 and speak normally the whole time",
                s.len() as f32 / EMBED_RATE as f32,
                captured_secs
            ));
        }
        Err(e) => {
            log::warn!(
                "Voice enrollment: VAD unavailable ({}), using the full recording",
                e
            );
            samples_16k.clone()
        }
    };

    let speech_secs = speech.len() as f32 / EMBED_RATE as f32;

    let window = (WINDOW_SECS * EMBED_RATE as f32) as usize;
    let min_window = (MIN_WINDOW_SECS * EMBED_RATE as f32) as usize;
    let embedder = SpeakerEmbedder::from_path(model_path, 1)?;

    let mut embeddings: Vec<Vec<f32>> = Vec::new();
    for chunk in speech.chunks(window) {
        if chunk.len() < min_window {
            continue; // trailing scrap — too short for a stable embedding
        }
        match embedder.embed(chunk) {
            Ok(e) => embeddings.push(e),
            Err(e) => log::warn!("Voice enrollment: skipping a window ({})", e),
        }
    }

    if embeddings.is_empty() {
        return Err(anyhow!(
            "Couldn't extract a voice signature from that recording — please try again"
        ));
    }

    log::info!(
        "Voice enrollment: {:.1}s captured, {:.1}s speech, {} windows embedded",
        captured_secs,
        speech_secs,
        embeddings.len()
    );

    let count = embeddings.len();
    Ok((average_and_normalize(&embeddings), count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_recording_is_rejected_before_model_load() {
        // 2s at 48 kHz — under MIN_CAPTURE_SECS, so this must fail on duration
        // rather than trying (and failing) to load a model.
        let samples = vec![0.5f32; 2 * CAPTURE_RATE as usize];
        let err = build_centroid(&samples, std::path::Path::new("/nonexistent/model.onnx"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least"), "unexpected error: {}", err);
    }

    #[test]
    fn self_label_falls_back_to_me_when_blank() {
        assert_eq!(self_label_or_default(None), SELF_SPEAKER_LABEL);
        assert_eq!(self_label_or_default(Some("")), SELF_SPEAKER_LABEL);
        assert_eq!(self_label_or_default(Some("   ")), SELF_SPEAKER_LABEL);
    }

    #[test]
    fn self_label_trims_and_caps() {
        assert_eq!(self_label_or_default(Some("  Seb  ")), "Seb");
        let long: String = "a".repeat(100);
        assert_eq!(self_label_or_default(Some(&long)).chars().count(), 60);
    }

    #[test]
    fn silent_recording_is_rejected() {
        let samples = vec![0.0f32; 15 * CAPTURE_RATE as usize];
        let err = build_centroid(&samples, std::path::Path::new("/nonexistent/model.onnx"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("silent"), "unexpected error: {}", err);
    }
}
