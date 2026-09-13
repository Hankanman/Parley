use anyhow::Result;
use chrono::Utc;
use log::{debug, info, warn};
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

/// Soft-clipping "limiter" (issue #21).
///
/// The previous `TruePeakLimiter` here wasn't actually a limiter: it fed
/// each sample through a 10ms delay line and hard-clipped it if it exceeded
/// the threshold, with no knee — a delayed hard clip, not true-peak
/// limiting. Replaced with a `tanh` soft clip: samples below `threshold`
/// pass through completely unchanged (so normal-level speech is untouched),
/// and samples above it are compressed smoothly toward ±1.0 instead of
/// being clipped flat. That trades a little harmonic distortion on rare
/// over-threshold peaks for no lookahead buffer (zero added latency) and no
/// audible "wall" at the threshold — the right trade for a live mic path
/// feeding VAD/Whisper, not a mastering chain.
struct SoftClipper {
    threshold: f32,
}

impl SoftClipper {
    fn new(threshold: f32) -> Self {
        Self {
            threshold: threshold.clamp(0.0, 0.999),
        }
    }

    fn process(&self, sample: f32) -> f32 {
        let abs = sample.abs();
        if abs <= self.threshold {
            return sample;
        }
        let headroom = (1.0 - self.threshold).max(1e-6);
        let over = (abs - self.threshold) / headroom;
        sample.signum() * (self.threshold + headroom * over.tanh())
    }
}

/// Gain the mic normalizer is allowed to request, in dB (issue #21).
///
/// Unbounded gain was the bug: a very quiet (e.g. -60 LUFS) mic could ask
/// for +37 dB, which also amplifies whatever echo AEC didn't fully cancel
/// back up toward the target loudness. Capping keeps the normalizer doing
/// comfort gain, not undoing AEC's work.
const GAIN_CEILING_DB: f32 = 18.0;
const GAIN_FLOOR_DB: f32 = -12.0;

/// Professional loudness normalizer using EBU R128 standard
/// This is a STATEFUL normalizer that tracks cumulative loudness over time
///
/// EBU R128 is the broadcast industry standard for loudness normalization:
/// - Target: -23 LUFS (Loudness Units relative to Full Scale)
/// - Used by: Netflix, YouTube, Spotify, all professional broadcast
/// - Perceptually accurate (not just simple RMS)
///
/// Gain is capped to [GAIN_FLOOR_DB, GAIN_CEILING_DB] and smoothed with a
/// one-pole follower (issue #21) so it never steps abruptly every 512-sample
/// analysis window, and never demands more boost than a downstream AEC path
/// can tolerate. This normalizer must run AFTER acoustic echo cancellation
/// (see `AudioPipeline::run` in pipeline.rs) — applying it before AEC feeds
/// AEC a time-varying near-end signal and breaks its convergence.
pub struct LoudnessNormalizer {
    ebur128: ebur128::EbuR128,
    limiter: SoftClipper,
    /// Current, smoothed linear gain actually applied to samples.
    gain_linear: f32,
    /// Most recent gain measurement (capped), which `gain_linear` chases.
    target_gain_linear: f32,
    /// One-pole smoothing coefficient — see `new` for the time constant.
    smoothing_alpha: f32,
    loudness_buffer: Vec<f32>,
}

impl LoudnessNormalizer {
    /// Create a new EBU R128 loudness normalizer
    ///
    /// # Arguments
    /// * `channels` - Number of audio channels (1 for mono, 2 for stereo)
    /// * `sample_rate` - Sample rate in Hz (e.g., 48000)
    pub fn new(channels: u32, sample_rate: u32) -> Result<Self> {
        const ANALYZE_CHUNK_SIZE: usize = 512;
        // -1 dBFS soft-clip threshold — matches the previous true-peak target.
        const SOFT_CLIP_THRESHOLD_DB: f32 = -1.0;
        // 50ms one-pole time constant for the gain follower: fast enough to
        // track real level changes, slow enough that no single 512-sample
        // measurement update is audible as a step.
        const SMOOTHING_TAU_SECS: f32 = 0.05;

        let ebur128 = ebur128::EbuR128::new(channels, sample_rate, ebur128::Mode::I)
            .map_err(|e| anyhow::anyhow!("Failed to create EBU R128 normalizer: {}", e))?;

        let soft_clip_threshold = 10_f32.powf(SOFT_CLIP_THRESHOLD_DB / 20.0);
        let smoothing_alpha = 1.0 - (-1.0 / (SMOOTHING_TAU_SECS * sample_rate as f32)).exp();

        Ok(Self {
            ebur128,
            limiter: SoftClipper::new(soft_clip_threshold),
            gain_linear: 1.0,
            target_gain_linear: 1.0,
            smoothing_alpha,
            loudness_buffer: Vec::with_capacity(ANALYZE_CHUNK_SIZE),
        })
    }

    /// Normalize loudness using EBU R128 standard with a capped, smoothed
    /// gain and a soft clip.
    ///
    /// This maintains cumulative loudness measurements across all processed
    /// audio, resulting in consistent normalization that sounds natural.
    /// Target: -23 LUFS (professional broadcast standard for speech/dialog).
    /// Gain is capped to [-12dB, +18dB] and smoothed per-sample toward the
    /// latest measurement (issue #21) rather than jumping to it.
    pub fn normalize_loudness(&mut self, samples: &[f32]) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }

        const TARGET_LUFS: f64 = -23.0;
        const ANALYZE_CHUNK_SIZE: usize = 512;

        let mut normalized_samples = Vec::with_capacity(samples.len());

        for &sample in samples {
            // Accumulate samples for loudness analysis
            self.loudness_buffer.push(sample);

            // Analyze loudness every 512 samples
            if self.loudness_buffer.len() >= ANALYZE_CHUNK_SIZE {
                if let Err(e) = self.ebur128.add_frames_f32(&self.loudness_buffer) {
                    warn!("Failed to add frames to EBU R128: {}", e);
                } else {
                    // Update the gain target based on cumulative loudness.
                    // `gain_linear` itself is smoothed toward this below,
                    // one sample at a time, rather than snapping here.
                    if let Ok(current_lufs) = self.ebur128.loudness_global() {
                        if current_lufs.is_finite() && current_lufs < 0.0 {
                            let gain_db = (TARGET_LUFS - current_lufs) as f32;
                            let capped_db = gain_db.clamp(GAIN_FLOOR_DB, GAIN_CEILING_DB);
                            self.target_gain_linear = 10_f32.powf(capped_db / 20.0);
                        }
                    }
                }
                self.loudness_buffer.clear();
            }

            // Chase the target gain smoothly instead of stepping to it.
            self.gain_linear +=
                (self.target_gain_linear - self.gain_linear) * self.smoothing_alpha;

            let amplified = sample * self.gain_linear;
            let limited = self.limiter.process(amplified);

            normalized_samples.push(limited);
        }

        normalized_samples
    }
}

/// High-pass filter to remove low-frequency rumble and noise
/// Removes frequencies below cutoff_hz (typically 80-100 Hz for speech)
pub struct HighPassFilter {
    #[allow(dead_code)]
    sample_rate: f32,
    #[allow(dead_code)]
    cutoff_hz: f32,
    // First-order IIR filter coefficients
    alpha: f32,
    prev_input: f32,
    prev_output: f32,
}

impl HighPassFilter {
    /// Create a new high-pass filter
    ///
    /// # Arguments
    /// * `sample_rate` - Audio sample rate in Hz
    /// * `cutoff_hz` - Cutoff frequency in Hz (typical: 80-100 Hz for speech)
    pub fn new(sample_rate: u32, cutoff_hz: f32) -> Self {
        let sample_rate_f = sample_rate as f32;
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
        let dt = 1.0 / sample_rate_f;
        let alpha = rc / (rc + dt);

        info!(
            "Initializing high-pass filter: cutoff={}Hz @ {}Hz",
            cutoff_hz, sample_rate
        );

        Self {
            sample_rate: sample_rate_f,
            cutoff_hz,
            alpha,
            prev_input: 0.0,
            prev_output: 0.0,
        }
    }

    /// Apply high-pass filter to audio samples
    /// Uses first-order IIR (Infinite Impulse Response) filter
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity(samples.len());

        for &sample in samples {
            // First-order high-pass IIR filter formula:
            // y[n] = alpha * (y[n-1] + x[n] - x[n-1])
            let filtered = self.alpha * (self.prev_output + sample - self.prev_input);

            self.prev_input = sample;
            self.prev_output = filtered;

            output.push(filtered);
        }

        output
    }

    /// Reset filter state (call when starting new recording)
    pub fn reset(&mut self) {
        self.prev_input = 0.0;
        self.prev_output = 0.0;
    }
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
mod mic_enhancement_tests {
    //! Pure-function tests for issue #21: the normalizer's gain cap/smoothing
    //! and the soft clip. No audio devices involved.
    use super::*;

    fn gain_db_to_linear(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    // ---- SoftClipper -------------------------------------------------

    #[test]
    fn soft_clip_passes_samples_below_threshold_unchanged() {
        let clipper = SoftClipper::new(0.9);
        assert!((clipper.process(0.5) - 0.5).abs() < 1e-6);
        assert!((clipper.process(-0.5) - (-0.5)).abs() < 1e-6);
        assert!((clipper.process(0.9) - 0.9).abs() < 1e-6);
    }

    #[test]
    fn soft_clip_bounds_samples_above_threshold() {
        let clipper = SoftClipper::new(0.9);
        // Moderately over threshold (not so far that f32 tanh saturates to
        // exactly 1.0): stays bounded strictly below full scale, above the
        // threshold (soft knee, not a hard clip down to the threshold).
        let pos = clipper.process(1.2);
        let neg = clipper.process(-1.2);
        assert!(pos < 1.0 && pos > 0.9, "pos={pos}");
        assert!(neg > -1.0 && neg < -0.9, "neg={neg}");

        // Far-over-threshold samples still never exceed full scale, even
        // once f32 precision saturates tanh to 1.0.
        let extreme = clipper.process(50.0);
        assert!(extreme <= 1.0, "extreme={extreme}");
    }

    #[test]
    fn soft_clip_is_monotonic_and_antisymmetric() {
        let clipper = SoftClipper::new(0.8);
        let mut prev_out = f32::NEG_INFINITY;
        let mut x = -3.0f32;
        while x <= 3.0 {
            let out = clipper.process(x);
            assert!(
                out >= prev_out,
                "soft clip must be monotonic: x={x} out={out} prev_out={prev_out}"
            );
            // Odd function: f(-x) == -f(x).
            let out_neg = clipper.process(-x);
            assert!(
                (out_neg + out).abs() < 1e-5,
                "soft clip should be antisymmetric: x={x} out={out} out(-x)={out_neg}"
            );
            prev_out = out;
            x += 0.1;
        }
    }

    // ---- LoudnessNormalizer gain cap + smoothing ----------------------

    #[test]
    fn normalizer_gain_is_capped_for_very_quiet_audio() {
        let mut norm = LoudnessNormalizer::new(1, 48_000).unwrap();
        // Far below -23 LUFS target; an uncapped normalizer would demand
        // tens of dB of boost (issue #21). Feed several seconds so the
        // EBU R128 integrated measurement settles.
        let quiet: Vec<f32> = (0..48_000 * 3)
            .map(|i| 0.0003 * (i as f32 * 0.05).sin())
            .collect();
        for chunk in quiet.chunks(4800) {
            norm.normalize_loudness(chunk);
        }
        let ceiling_linear = gain_db_to_linear(GAIN_CEILING_DB);
        assert!(
            norm.target_gain_linear <= ceiling_linear + 1e-3,
            "target gain {} exceeded +{}dB ceiling {}",
            norm.target_gain_linear,
            GAIN_CEILING_DB,
            ceiling_linear
        );
        assert!(
            norm.gain_linear <= ceiling_linear + 1e-3,
            "smoothed gain {} exceeded +{}dB ceiling {}",
            norm.gain_linear,
            GAIN_CEILING_DB,
            ceiling_linear
        );
    }

    #[test]
    fn normalizer_gain_is_floored_for_very_loud_audio() {
        let mut norm = LoudnessNormalizer::new(1, 48_000).unwrap();
        // Well above -23 LUFS; an uncapped normalizer would ask for large
        // negative gain, undoing AEC's work on residual echo (issue #21).
        let loud: Vec<f32> = (0..48_000 * 3)
            .map(|i| 0.8 * (i as f32 * 0.05).sin())
            .collect();
        for chunk in loud.chunks(4800) {
            norm.normalize_loudness(chunk);
        }
        let floor_linear = gain_db_to_linear(GAIN_FLOOR_DB);
        assert!(
            norm.target_gain_linear >= floor_linear - 1e-3,
            "target gain {} went below {}dB floor {}",
            norm.target_gain_linear,
            GAIN_FLOOR_DB,
            floor_linear
        );
    }

    #[test]
    fn normalizer_gain_changes_smoothly_not_in_steps() {
        let mut norm = LoudnessNormalizer::new(1, 48_000).unwrap();

        // EBU R128's integrated-loudness measurement needs hundreds of ms
        // to converge, which would make this test slow and indirect. Stage
        // a large target jump directly instead, to isolate the one-pole
        // smoothing math (the thing issue #21 asks for) from the
        // measurement pipeline.
        norm.target_gain_linear = gain_db_to_linear(GAIN_CEILING_DB);
        assert!((norm.gain_linear - 1.0).abs() < 1e-6);

        // Silence doesn't yield a finite loudness measurement (guarded by
        // `current_lufs.is_finite()` in `normalize_loudness`), so
        // `target_gain_linear` stays exactly what we staged above — only
        // the smoothing follower moves.
        let silence = vec![0.0f32; 512];
        norm.normalize_loudness(&silence);

        assert!(
            norm.gain_linear < norm.target_gain_linear - 1e-4,
            "gain should not jump instantly to target: gain={} target={}",
            norm.gain_linear,
            norm.target_gain_linear
        );

        // Feeding enough further silence lets the smoothed gain converge
        // toward the (fixed) target.
        for _ in 0..50 {
            norm.normalize_loudness(&silence);
        }
        assert!(
            (norm.gain_linear - norm.target_gain_linear).abs() < 0.05,
            "gain should have converged close to target after many blocks: gain={} target={}",
            norm.gain_linear,
            norm.target_gain_linear
        );
    }
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
