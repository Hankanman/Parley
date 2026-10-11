//! Shared whisper.cpp transcription logic: `FullParams` construction and
//! decoder-confidence scoring.
//!
//! Ported out of the in-process engine (now
//! `parley-core/src/whisper_engine/whisper_engine.rs`)
//! (`transcribe_audio_with_confidence_opts`) for the issue #56 sidecar spike.
//! The in-process engine is left unmodified for this spike (see
//! `docs/transcription-backends.md` for the follow-up that would switch it
//! to call this crate too, removing the duplication); this crate mirrors its
//! parameter choices and confidence formula exactly so the two paths behave
//! identically once wired together.
//!
//! Deliberately has no knowledge of Tokio, the UI shell, or hardware detection —
//! callers (the in-process engine, or `whisper-helper`) supply thread count
//! and beam size via [`DecodeConfig`] rather than this crate reaching for
//! `audio::HardwareProfile` itself, so it stays usable from a plain sidecar
//! binary with no async runtime.

use anyhow::{anyhow, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext};

/// Whisper decodes nothing useful under 1s of audio ("input is too short")
/// and silently returns zero segments instead of erroring — callers must pad
/// short chunks before calling [`transcribe_pcm16k`].
pub const WHISPER_MIN_INPUT_SAMPLES: usize = 16_000; // 1s @ 16kHz

/// Zero-pad `audio` up to [`WHISPER_MIN_INPUT_SAMPLES`] if it's shorter.
pub fn pad_to_min_whisper_input(mut audio: Vec<f32>) -> Vec<f32> {
    if audio.len() < WHISPER_MIN_INPUT_SAMPLES {
        audio.resize(WHISPER_MIN_INPUT_SAMPLES, 0.0);
    }
    audio
}

/// Hardware-adaptive decode knobs. The in-process engine derives these from
/// `audio::HardwareProfile::detect().get_whisper_config()`; a sidecar with no
/// access to that module can supply its own estimate (e.g. `num_cpus` or a
/// fixed conservative default) via [`DecodeConfig::default`].
#[derive(Debug, Clone, Copy)]
pub struct DecodeConfig {
    pub beam_size: u32,
    pub max_threads: i32,
    pub temperature: f32,
}

impl Default for DecodeConfig {
    fn default() -> Self {
        // Matches HardwareProfile's conservative fallback tier.
        Self {
            beam_size: 5,
            max_threads: 4,
            temperature: 0.0,
        }
    }
}

/// Per-call overrides — mirrors `whisper_engine::TranscribeOptions`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TranscribeOptions {
    pub max_threads: Option<i32>,
    pub greedy: bool,
}

/// Cap `default` at `requested`, if given, flooring at 1. Identical to
/// `whisper_engine::effective_threads` — kept in sync manually since this
/// crate doesn't depend on the app crate.
fn effective_threads(default: i32, requested: Option<i32>) -> i32 {
    let capped = match requested {
        Some(requested) => default.min(requested),
        None => default,
    };
    capped.max(1)
}

#[derive(Debug, Clone)]
pub struct SegmentOutcome {
    pub text: String,
    pub confidence: f32,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone)]
pub struct TranscriptionOutcome {
    pub text: String,
    pub confidence: f32,
    pub is_partial: bool,
    pub segments: Vec<SegmentOutcome>,
}

/// Transcribe one chunk of 16kHz mono f32 PCM against an already-loaded
/// `WhisperContext`. Synchronous and CPU-bound — callers on an async runtime
/// should run this via `spawn_blocking`, exactly as
/// `WhisperEngine::transcribe_audio_with_confidence_opts` does.
///
/// Confidence formula matches the in-process engine: mean token probability
/// per segment (specials excluded), discounted by the segment's no-speech
/// probability, weighted by token count when averaging across segments.
pub fn transcribe_pcm16k(
    ctx: &WhisperContext,
    audio_data: Vec<f32>,
    language: Option<&str>,
    context_prompt: Option<&str>,
    decode_config: DecodeConfig,
    options: TranscribeOptions,
) -> Result<TranscriptionOutcome> {
    let duration_seconds = audio_data.len() as f64 / 16000.0;
    let is_partial = duration_seconds < 15.0;

    let mut params = if options.greedy {
        FullParams::new(SamplingStrategy::Greedy { best_of: 1 })
    } else {
        FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: decode_config.beam_size as i32,
            patience: 1.0,
        })
    };

    params.set_n_threads(effective_threads(
        decode_config.max_threads,
        options.max_threads,
    ));

    let (language_code, should_translate) = match language {
        Some("auto") | None => (None, false),
        Some("auto-translate") => (None, true),
        Some(lang) => (Some(lang), false),
    };
    params.set_language(language_code);
    params.set_translate(should_translate);

    params.set_no_timestamps(false); // sidecar reports per-segment timestamps
    params.set_token_timestamps(true);

    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_temperature(decode_config.temperature);
    params.set_max_initial_ts(1.0);
    params.set_entropy_thold(2.4);
    params.set_logprob_thold(-1.0);
    params.set_no_speech_thold(0.55);
    params.set_max_len(200);
    params.set_single_segment(false);

    let sanitized_prompt = context_prompt
        .map(|p| p.replace('\0', " "))
        .filter(|p| !p.trim().is_empty());
    if let Some(ref prompt) = sanitized_prompt {
        params.set_initial_prompt(prompt);
    }

    let audio_data = pad_to_min_whisper_input(audio_data);

    let mut state = ctx
        .create_state()
        .map_err(|e| anyhow!("failed to create whisper state: {e}"))?;
    state
        .full(params, &audio_data)
        .map_err(|e| anyhow!("whisper full() failed: {e}"))?;
    let num_segments = state.full_n_segments();

    let mut segments = Vec::with_capacity(num_segments.max(0) as usize);
    let mut result = String::new();
    let mut weighted_confidence = 0.0f32;
    let mut total_tokens = 0u32;

    for i in 0..num_segments {
        let Some(segment) = state.get_segment(i) else {
            continue;
        };
        let segment_text = match segment.to_str_lossy() {
            Ok(text) => text.into_owned(),
            Err(_) => continue,
        };

        let mut prob_sum = 0.0f32;
        let mut token_count = 0u32;
        for t in 0..segment.n_tokens() {
            let Some(token) = segment.get_token(t) else {
                continue;
            };
            let is_special = token
                .to_str()
                .map(|s| s.starts_with("<|") || s.starts_with("[_"))
                .unwrap_or(true);
            if is_special {
                continue;
            }
            prob_sum += token.token_probability();
            token_count += 1;
        }

        let no_speech = segment.no_speech_probability().clamp(0.0, 1.0);
        let segment_confidence = if token_count > 0 {
            let avg_p = prob_sum / token_count as f32;
            let c = avg_p * (1.0 - no_speech);
            weighted_confidence += c * token_count as f32;
            total_tokens += token_count;
            c
        } else {
            0.0
        };

        let cleaned_text = segment_text.trim();
        if !cleaned_text.is_empty() {
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(cleaned_text);
        }

        segments.push(SegmentOutcome {
            text: cleaned_text.to_string(),
            confidence: segment_confidence.clamp(0.0, 1.0),
            start_ms: segment.start_timestamp() * 10,
            end_ms: segment.end_timestamp() * 10,
        });
    }

    let avg_confidence = if total_tokens > 0 {
        (weighted_confidence / total_tokens as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };

    Ok(TranscriptionOutcome {
        text: result.trim().to_string(),
        confidence: avg_confidence,
        is_partial,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_short_input_to_floor() {
        let out = pad_to_min_whisper_input(vec![0.5; 4_000]);
        assert_eq!(out.len(), WHISPER_MIN_INPUT_SAMPLES);
        assert_eq!(out[3_999], 0.5);
        assert_eq!(out[4_000], 0.0);
    }

    #[test]
    fn leaves_long_input_untouched() {
        let input = vec![0.1; 32_000];
        let out = pad_to_min_whisper_input(input.clone());
        assert_eq!(out, input);
    }

    #[test]
    fn effective_threads_caps_and_floors() {
        assert_eq!(effective_threads(8, Some(2)), 2);
        assert_eq!(effective_threads(8, None), 8);
        assert_eq!(effective_threads(8, Some(0)), 1);
        assert_eq!(effective_threads(1, Some(99)), 1);
    }
}
