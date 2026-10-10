use anyhow::Result;
use chrono::Utc;
use log::debug;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::path::PathBuf;

/// Sanitize a filename to be safe for filesystem use
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Create a meeting folder with timestamp and return the path
/// Creates structure: base_path/MeetingName_YYYY-MM-DD_HH-MM-SS/
///                    ├── .checkpoints/  (for incremental saves, optional)
///
/// # Arguments
/// * `base_path` - Base directory for meetings
/// * `meeting_name` - Name of the meeting
/// * `create_checkpoints_dir` - Whether to create .checkpoints/ subdirectory (only needed when auto_save is true)
pub fn create_meeting_folder(
    base_path: &PathBuf,
    meeting_name: &str,
    create_checkpoints_dir: bool,
) -> Result<PathBuf> {
    let timestamp = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let sanitized_name = sanitize_filename(meeting_name);
    let base_folder_name = format!("{}_{}", sanitized_name, timestamp);

    // Guard against name collisions: two meetings with the same (sanitized)
    // title created within the same second would otherwise resolve to the
    // same path, and create_dir_all() on an existing folder succeeds
    // silently — the second meeting's files would then overwrite the
    // first's. Append a numeric suffix until we land on a free path.
    let mut meeting_folder = base_path.join(&base_folder_name);
    let mut suffix = 1u32;
    while meeting_folder.exists() {
        meeting_folder = base_path.join(format!("{}_{}", base_folder_name, suffix));
        suffix += 1;
    }

    // Create main meeting folder
    std::fs::create_dir_all(&meeting_folder)?;

    // Only create .checkpoints subdirectory if requested (when auto_save is true)
    if create_checkpoints_dir {
        let checkpoints_dir = meeting_folder.join(".checkpoints");
        std::fs::create_dir_all(&checkpoints_dir)?;
        log::info!(
            "Created meeting folder with checkpoints: {}",
            meeting_folder.display()
        );
    } else {
        log::info!(
            "Created meeting folder without checkpoints: {}",
            meeting_folder.display()
        );
    }

    Ok(meeting_folder)
}

pub fn audio_to_mono(audio: &[f32], channels: u16) -> Vec<f32> {
    let mut mono_samples = Vec::with_capacity(audio.len() / channels as usize);

    // For microphone arrays (> 2 channels), only use first 2 channels
    // Many microphone arrays have auxiliary channels for beam-forming/noise cancellation
    // that can contain anti-phase signals. Averaging all channels can cause destructive
    // interference resulting in near-zero output.
    let effective_channels = if channels > 2 { 2 } else { channels };

    // Iterate over the audio slice in chunks, each containing `channels` samples
    for chunk in audio.chunks(channels as usize) {
        // Sum only the first effective_channels (typically 1-2 for mic arrays)
        let sum: f32 = chunk.iter().take(effective_channels as usize).sum();

        // Calculate the average mono sample using effective channel count
        let mono_sample = sum / effective_channels as f32;

        // Store the computed mono sample
        mono_samples.push(mono_sample);
    }

    mono_samples
}

/// High-quality audio resampling with adaptive parameters based on sample rate ratio
///
/// This function automatically selects the best resampling parameters based on:
/// - Sample rate ratio (upsampling vs downsampling)
/// - Quality requirements (integer ratios get optimized paths)
/// - Anti-aliasing needs
///
/// Supports all common sample rates: 8kHz, 16kHz, 24kHz, 44.1kHz, 48kHz, etc.
pub fn resample(input: &[f32], from_sample_rate: u32, to_sample_rate: u32) -> Result<Vec<f32>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }

    // Fast path: No resampling needed
    if from_sample_rate == to_sample_rate {
        return Ok(input.to_vec());
    }

    let ratio = to_sample_rate as f64 / from_sample_rate as f64;

    // Adaptive parameters based on sample rate ratio
    let (sinc_len, interpolation_type, oversampling) = if ratio >= 2.0 {
        // Large upsampling (e.g., 8kHz → 16kHz, 16kHz → 48kHz, 24kHz → 48kHz)
        // Needs high quality to avoid artifacts
        debug!(
            "High-quality upsampling: {}Hz → {}Hz (ratio: {:.2}x)",
            from_sample_rate, to_sample_rate, ratio
        );
        (
            512,                          // Longer sinc for smoother interpolation
            SincInterpolationType::Cubic, // Cubic for best quality
            512,                          // Higher oversampling
        )
    } else if ratio >= 1.5 {
        // Moderate upsampling (e.g., 32kHz → 48kHz)
        debug!(
            "Moderate upsampling: {}Hz → {}Hz (ratio: {:.2}x)",
            from_sample_rate, to_sample_rate, ratio
        );
        (384, SincInterpolationType::Cubic, 384)
    } else if ratio > 1.0 {
        // Small upsampling (e.g., 44.1kHz → 48kHz)
        debug!(
            "Small upsampling: {}Hz → {}Hz (ratio: {:.2}x)",
            from_sample_rate, to_sample_rate, ratio
        );
        (256, SincInterpolationType::Linear, 256)
    } else if ratio <= 0.5 {
        // Large downsampling (e.g., 48kHz → 16kHz, 48kHz → 8kHz)
        // Needs strong anti-aliasing
        debug!(
            "Anti-aliased downsampling: {}Hz → {}Hz (ratio: {:.2}x)",
            from_sample_rate, to_sample_rate, ratio
        );
        (
            512,                          // Longer sinc for anti-aliasing
            SincInterpolationType::Cubic, // Cubic for quality
            512,
        )
    } else {
        // Moderate downsampling (e.g., 48kHz → 24kHz, 48kHz → 32kHz)
        debug!(
            "Moderate downsampling: {}Hz → {}Hz (ratio: {:.2}x)",
            from_sample_rate, to_sample_rate, ratio
        );
        (384, SincInterpolationType::Linear, 384)
    };

    let params = SincInterpolationParameters {
        sinc_len,
        f_cutoff: 0.95, // Preserve most of the frequency content
        interpolation: interpolation_type,
        oversampling_factor: oversampling,
        window: WindowFunction::BlackmanHarris2, // Best window for audio
    };

    let mut resampler = SincFixedIn::<f32>::new(
        ratio,
        2.0, // Maximum relative deviation
        params,
        input.len(),
        1, // Mono
    )?;

    // SincFixedIn has a fixed group delay of `sinc_len / 2` input frames
    // (reported here, already converted to output frames, by
    // `output_delay()`) that is never flushed by a single `process()` call:
    // the trailing ~`delay` output samples that correspond to the tail of
    // `input` are still "owed" by the filter and only come out if we feed it
    // more (zero-padded) input. Without draining this, output is shifted
    // early by `delay` samples and the last few milliseconds of audio are
    // silently lost.
    let delay = resampler.output_delay();

    let waves_in = vec![input.to_vec()];
    let mut waves_out = resampler.process(&waves_in, None)?;

    // Drain the remaining group delay: feeding `None` zero-pads the input
    // with the samples needed to complete the filter response for the tail
    // of `input`.
    let flush = resampler.process_partial::<Vec<f32>>(None, None)?;
    waves_out[0].extend(flush.into_iter().next().unwrap_or_default());

    let mut out = waves_out.into_iter().next().unwrap();

    // Trim the leading `delay` samples (sinc filter warm-up) so the output
    // is time-aligned with the input instead of shifted late by the group
    // delay.
    if delay >= out.len() {
        out.clear();
    } else {
        out.drain(0..delay);
    }

    // The drain above can leave a few samples more than the ideal
    // input.len() * ratio (the flush call rounds up to a full output
    // block); trim to the expected length so callers get output whose
    // duration matches the input's, not the resampler's internal block size.
    let target_len = ((input.len() as f64) * ratio).round() as usize;
    out.truncate(target_len);

    debug!(
        "Resampling complete: {} samples → {} samples (delay {} trimmed)",
        input.len(),
        out.len(),
        delay
    );

    Ok(out)
}

// Alias for compatibility with existing code.
//
// Returns `Result` (rather than silently falling back to the original,
// wrong-sample-rate audio) because a caller that gets back samples at the
// wrong rate has no way to detect it — e.g. audio meant for 16kHz Whisper
// coming back still at 48kHz plays out ~3x slowed and produces garbage
// transcriptions with no error surfaced anywhere. Callers must propagate or
// explicitly log-and-fail instead of continuing with the input untouched.
pub fn resample_audio(
    input: &[f32],
    from_sample_rate: u32,
    to_sample_rate: u32,
) -> Result<Vec<f32>> {
    resample(input, from_sample_rate, to_sample_rate)
}

#[cfg(test)]
mod resample_tests {
    //! Issue #38: SincFixedIn's group delay must be flushed (not just
    //! truncated at the input's nominal length) so the last few
    //! milliseconds of audio survive resampling and the output is time-
    //! aligned with the input.
    use super::*;

    #[test]
    fn resample_1khz_tone_48k_to_16k_is_aligned_and_complete() {
        const FROM_RATE: u32 = 48_000;
        const TO_RATE: u32 = 16_000;
        let duration_secs = 1.0f32;
        let freq = 1000.0f32;
        let n = (FROM_RATE as f32 * duration_secs) as usize;
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / FROM_RATE as f32).sin())
            .collect();

        let output = resample(&input, FROM_RATE, TO_RATE).expect("resample should succeed");

        // Length should be ~= input.len() * ratio (16000 samples for 1s @ 16kHz).
        let expected_len = 16_000usize;
        let len_diff = (output.len() as i64 - expected_len as i64).unsigned_abs();
        assert!(
            len_diff <= 2,
            "expected ~{} samples, got {}",
            expected_len,
            output.len()
        );

        // RMS of a resampled full-cycle sine tone should closely match the
        // RMS of the original (both sines of the same amplitude): within 3%.
        let input_rms = (input.iter().map(|s| s * s).sum::<f32>() / input.len() as f32).sqrt();
        let output_rms = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
        let rel_diff = (output_rms - input_rms).abs() / input_rms;
        assert!(
            rel_diff < 0.03,
            "RMS mismatch too large: input_rms={} output_rms={} rel_diff={}",
            input_rms,
            output_rms,
            rel_diff
        );

        // The last 5ms (80 samples at 16kHz) must be non-zero — proof the
        // group delay was flushed rather than silently truncating the tail.
        let last_5ms = TO_RATE as usize * 5 / 1000;
        let tail = &output[output.len() - last_5ms..];
        assert!(
            tail.iter().any(|s| s.abs() > 1e-4),
            "expected non-zero samples in the last 5ms of output, got {:?}",
            tail
        );
    }

    #[test]
    fn resample_audio_propagates_result() {
        let input = vec![0.1f32; 4800];
        let result = resample_audio(&input, 48_000, 16_000);
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }
}
