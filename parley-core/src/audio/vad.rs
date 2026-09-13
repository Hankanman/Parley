//! Voice activity detection — wraps sherpa-onnx's `VoiceActivityDetector`.
//!
//! Previously used `silero-rs` (which depends on the `ort` crate). That worked
//! fine in isolation, but having `ort` in the same binary as `sherpa-onnx-sys`
//! (used for speaker embedding) caused glibc `free(): invalid pointer` aborts
//! at sherpa-onnx's first model load on Linux/CUDA — two static onnxruntime
//! copies in one process don't coexist on every platform. Switching to
//! sherpa-onnx's bundled VAD eliminates the second runtime entirely.
//!
//! Behaviour we preserve:
//! - Same `SpeechSegment` shape (samples + start/end timestamps + source tag).
//! - Same `ContinuousVadProcessor` API (`new`, `new_with_source`, `process_audio`, `flush`).
//! - Same `extract_speech_16k`, `get_speech_chunks`, `get_speech_chunks_with_progress` helpers.
//! - 16 kHz mono input; resampling from any input rate happens here via a
//!   stateful windowed-sinc downsampler (`StreamingDownsampler`).
//!
//! What changed under the hood:
//! - The "redemption_time_ms" parameter now maps to sherpa's
//!   `min_silence_duration` (the trailing silence gap that terminates a
//!   speech segment). Same intent, slightly different name.
//! - Audio is handed to sherpa one 512-sample silero window per call:
//!   sherpa updates its speech/silence state once per `accept_waveform`
//!   call, so larger calls blur or lose segment boundaries (see `feed_16k`).

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};

use super::recording_state::DeviceType;

/// Represents a complete speech segment detected by VAD.
#[derive(Debug, Clone)]
pub struct SpeechSegment {
    pub samples: Vec<f32>,
    pub start_timestamp_ms: f64,
    pub end_timestamp_ms: f64,
    pub confidence: f32,
    /// Which audio stream this segment came from (Mic or System).
    /// Set by the VAD processor based on its construction-time source tag.
    pub source: DeviceType,
}

/// Streaming VAD processor that emits complete speech segments.
pub struct ContinuousVadProcessor {
    detector: VoiceActivityDetector,
    /// Stateful anti-aliased downsampler to 16 kHz. `None` when the input is
    /// already 16 kHz. The samples it produces are what sherpa hands back in
    /// each `SpeechSegment`, i.e. exactly what Whisper decodes.
    downsampler: Option<StreamingDownsampler>,
    /// Total 16 kHz samples consumed so far. Used to convert sherpa's
    /// segment-relative sample indices into absolute timestamps.
    processed_samples_16k: u64,
    /// Source tag stamped onto every emitted segment (Mic or System).
    source: DeviceType,
    /// 16 kHz audio accumulated since the last completed segment — i.e. the
    /// current in-progress utterance. Used only for streaming partial preview
    /// decodes; reset whenever a segment finalizes (the authoritative final
    /// path consumes the completed segment separately). Capped to bound memory
    /// and partial-decode cost.
    current_utterance_16k: Vec<f32>,
}

/// Cap on the in-progress partial buffer (~30 s at 16 kHz). Matches the VAD's
/// own max_speech_duration so a single utterance can't grow the partial buffer
/// unboundedly if silence never arrives.
const MAX_PARTIAL_SAMPLES: usize = 30 * 16_000;

const VAD_SAMPLE_RATE: i32 = 16_000;

/// silero-vad's window: 512 samples = 32 ms at 16 kHz. Also the most audio
/// handed to sherpa per `accept_waveform` call (see `feed_16k`).
const VAD_WINDOW_SIZE: usize = 512;

impl ContinuousVadProcessor {
    /// Create a VAD processor with no specific source tag (defaults to Mic for
    /// backward compatibility with helpers that operate on a single mixed stream).
    pub fn new(input_sample_rate: u32, redemption_time_ms: u32) -> Result<Self> {
        Self::new_with_source(input_sample_rate, redemption_time_ms, DeviceType::Microphone)
    }

    /// Create a VAD processor that tags every emitted segment with `source`.
    /// Used for the dual-VAD pipeline where mic and system streams are
    /// processed independently to preserve source identity.
    pub fn new_with_source(
        input_sample_rate: u32,
        redemption_time_ms: u32,
        source: DeviceType,
    ) -> Result<Self> {
        // Force-cut after 12s. Real meeting utterances rarely run this long
        // unbroken, and longer segments hurt diarization (multiple speakers get
        // embedded as one label) and Whisper accuracy. Over-cutting is cheap:
        // extra pieces of one speaker just re-cluster to the same "Speaker N".
        Self::new_configured(input_sample_rate, redemption_time_ms, source, 12.0, 18.0)
    }

    /// A processor tuned for long, single-speaker capture (voice enrollment)
    /// rather than a meeting. It raises `max_speech_duration` far above the
    /// capture length so continuous reading is *not* force-cut: with the
    /// meeting default of 20s, someone reading a passage steadily (no 400ms
    /// pauses) hits the force-cut boundary and sherpa emits only a tiny tail
    /// segment, so almost the entire recording is discarded as "not speech".
    /// Here the only cuts are natural pauses.
    pub fn new_for_enrollment(input_sample_rate: u32) -> Result<Self> {
        // 60s force-cut / 65s buffer. Enrollment is capped well under this, so
        // the force-cut never fires; the large value exists only to satisfy
        // sherpa's "buffer >= max_speech_duration" invariant.
        Self::new_configured(input_sample_rate, 400, DeviceType::Microphone, 60.0, 65.0)
    }

    /// Shared constructor. `max_speech_duration` force-cuts an unbroken speech
    /// run; `buffer_secs` sizes sherpa's internal ring buffer and must be
    /// `>= max_speech_duration` so it can look back when force-cutting.
    fn new_configured(
        input_sample_rate: u32,
        redemption_time_ms: u32,
        source: DeviceType,
        max_speech_duration: f32,
        buffer_secs: f32,
    ) -> Result<Self> {
        let model_path = crate::speaker_diarization::model::silero_vad_path()
            .ok_or_else(|| anyhow!("silero-vad model dir not configured"))?;
        if !crate::speaker_diarization::model::model_is_ready(&model_path) {
            return Err(anyhow!(
                "silero-vad model not present at {}; download it on app startup",
                model_path.display()
            ));
        }

        // Map silero-rs's "redemption_time" (how long after losing speech to
        // wait before declaring segment end) to sherpa's `min_silence_duration`.
        let min_silence_duration = (redemption_time_ms as f32) / 1000.0;

        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(model_path.to_string_lossy().into_owned()),
                // 0.45 (vs silero default 0.50) makes brief lulls in continuous
                // speech register as silence — needed for meeting audio that
                // has continuous low-level room noise.
                threshold: 0.45,
                min_silence_duration,           // From caller (typically 400ms live, 800ms batch).
                min_speech_duration: 0.25,      // Reject segments shorter than 250ms.
                window_size: VAD_WINDOW_SIZE as i32, // 32 ms at 16 kHz, silero-vad's expected window.
                max_speech_duration,
            },
            ten_vad: Default::default(),
            sample_rate: VAD_SAMPLE_RATE,
            num_threads: 1,
            provider: Some("cpu".to_string()),
            debug: false,
        };

        let detector = VoiceActivityDetector::create(&config, buffer_secs)
            .ok_or_else(|| anyhow!("Failed to create sherpa VoiceActivityDetector"))?;

        info!(
            "VAD processor created (sherpa-onnx silero): input={}Hz, vad=16000Hz, \
             min_silence={}ms, max_speech={}s, source={:?}",
            input_sample_rate, redemption_time_ms, max_speech_duration, source
        );

        let downsampler = if input_sample_rate == VAD_SAMPLE_RATE as u32 {
            None
        } else {
            Some(StreamingDownsampler::new(
                input_sample_rate,
                VAD_SAMPLE_RATE as u32,
            )?)
        };

        Ok(Self {
            detector,
            downsampler,
            processed_samples_16k: 0,
            source,
            current_utterance_16k: Vec::new(),
        })
    }

    /// Whether sherpa currently considers speech to be active (inside an
    /// in-progress utterance). Used to gate streaming partial decodes.
    pub fn is_speech_active(&self) -> bool {
        self.detector.detected()
    }

    /// 16 kHz mono samples of the current in-progress utterance (audio since
    /// the last finalized segment). Empty between utterances.
    pub fn partial_samples(&self) -> &[f32] {
        &self.current_utterance_16k
    }

    /// Process incoming audio samples and return any complete speech segments.
    /// Handles resampling from input sample rate to 16 kHz.
    pub fn process_audio(&mut self, samples: &[f32]) -> Result<Vec<SpeechSegment>> {
        let resampled: std::borrow::Cow<[f32]> = match self.downsampler.as_mut() {
            None => samples.into(),
            Some(ds) => ds.process(samples)?.into(),
        };

        self.feed_16k(resampled.as_ref());

        let segments = self.drain_segments();
        // A finalized segment ends the current utterance — the authoritative
        // final path takes over from here, so clear the partial accumulator
        // and let the next utterance start fresh.
        if !segments.is_empty() {
            self.current_utterance_16k.clear();
        }
        Ok(segments)
    }

    /// Flush any remaining buffered audio and return final speech segments.
    pub fn flush(&mut self) -> Result<Vec<SpeechSegment>> {
        debug!(
            "VAD flush: processed {} samples ({}s), draining trailing segments",
            self.processed_samples_16k,
            self.processed_samples_16k as f64 / 16_000.0
        );
        // Push the downsampler's buffered remainder + filter delay through so
        // the last few milliseconds of speech reach the detector.
        if let Some(ds) = self.downsampler.as_mut() {
            let tail = ds.flush()?;
            if !tail.is_empty() {
                self.feed_16k(&tail);
            }
        }
        self.detector.flush();
        Ok(self.drain_segments())
    }

    /// Hand 16 kHz samples to sherpa and to the partial-preview accumulator.
    fn feed_16k(&mut self, samples_16k: &[f32]) {
        if samples_16k.is_empty() {
            return;
        }
        // Feed sherpa one model window at a time. Its `AcceptWaveform` runs
        // silero on every window in the call but updates the speech/silence
        // state machine only once per call (OR of all windows) — so a large
        // call collapses to one decision: a whole-file batch call yields a
        // single bogus segment, 1 s calls merge utterances across pauses,
        // 10 s calls drop entire utterances. sherpa keeps any sub-window
        // remainder internally, so arbitrary slice boundaries are fine.
        for window in samples_16k.chunks(VAD_WINDOW_SIZE) {
            self.detector.accept_waveform(window);
        }
        self.processed_samples_16k += samples_16k.len() as u64;

        // Accumulate the in-progress utterance for streaming partial decodes,
        // bounded by MAX_PARTIAL_SAMPLES.
        if self.current_utterance_16k.len() < MAX_PARTIAL_SAMPLES {
            self.current_utterance_16k.extend_from_slice(samples_16k);
        }
    }

    /// Pop every queued segment from sherpa's detector, converting each to
    /// our `SpeechSegment` shape (with timestamps and source tag).
    fn drain_segments(&mut self) -> Vec<SpeechSegment> {
        let mut out = Vec::new();
        while !self.detector.is_empty() {
            let Some(seg) = self.detector.front() else { break };
            let start_sample = seg.start();
            let samples_slice = seg.samples();
            let n_samples = samples_slice.len();
            let samples = samples_slice.to_vec();

            let start_ms = (start_sample as f64 / 16_000.0) * 1000.0;
            let end_ms = ((start_sample as i64 + n_samples as i64) as f64 / 16_000.0) * 1000.0;

            info!(
                "VAD: speech segment {:.0}ms-{:.0}ms ({} samples, source={:?})",
                start_ms, end_ms, n_samples, self.source
            );

            out.push(SpeechSegment {
                samples,
                start_timestamp_ms: start_ms,
                end_timestamp_ms: end_ms,
                confidence: 0.9,
                source: self.source,
            });

            // `front()` borrows the segment; drop before pop to release the
            // sherpa-side buffer.
            drop(seg);
            self.detector.pop();
        }
        out
    }

}

/// Stateful, anti-aliased sample-rate converter used to bring pipeline audio
/// (48 kHz) down to the 16 kHz sherpa/Whisper expect.
///
/// A windowed-sinc resampler (rubato `SincFixedIn`) with its state carried
/// across calls: there are no per-window edge effects and the transition band
/// sits at the *output* Nyquist, so content above 8 kHz is rejected instead of
/// folding into the speech band. (The previous implementation was a 5-tap
/// moving average plus linear interpolation, whose first null was near
/// 9.6 kHz — sibilants and broadband noise between 8 and 24 kHz aliased
/// straight into what Whisper decoded.)
///
/// Input is accumulated into fixed 10 ms blocks, so arbitrary chunk sizes
/// are accepted; the live pipeline's 50 ms windows resolve to exactly five
/// blocks (2400 in → 800 out at 48 → 16 kHz). Group delay is
/// `sinc_len / 2` input frames (~1.3 ms), flushed by [`Self::flush`].
pub(crate) struct StreamingDownsampler {
    resampler: SincFixedIn<f32>,
    /// Fixed input block the resampler consumes per call.
    block_in: usize,
    /// Input samples waiting for a full block.
    pending: Vec<f32>,
}

impl StreamingDownsampler {
    pub(crate) fn new(input_rate: u32, output_rate: u32) -> Result<Self> {
        if input_rate == 0 || output_rate == 0 {
            return Err(anyhow!(
                "invalid sample rates for downsampler: {} -> {}",
                input_rate,
                output_rate
            ));
        }
        // 10 ms blocks: small enough that leftovers are negligible for any
        // caller, and an exact divisor of the pipeline's 50 ms windows.
        let block_in = (input_rate / 100).max(1) as usize;
        let ratio = output_rate as f64 / input_rate as f64;
        let params = SincInterpolationParameters {
            sinc_len: 128,
            // Relative to the lower Nyquist (the 16 kHz side): keep the
            // passband to ~7.4 kHz so the stopband is well established by 8 kHz.
            f_cutoff: 0.92,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler = SincFixedIn::<f32>::new(ratio, 1.0, params, block_in, 1)
            .map_err(|e| anyhow!("failed to create {}→{} Hz downsampler: {}", input_rate, output_rate, e))?;
        Ok(Self {
            resampler,
            block_in,
            pending: Vec::with_capacity(block_in * 6),
        })
    }

    /// Convert `samples`; returns whatever whole blocks are now available
    /// (the remainder is buffered for the next call).
    pub(crate) fn process(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        self.pending.extend_from_slice(samples);
        let blocks = self.pending.len() / self.block_in;
        let mut out = Vec::with_capacity(blocks * self.resampler.output_frames_max());
        for _ in 0..blocks {
            let block = [self.pending.drain(..self.block_in).collect::<Vec<f32>>()];
            let mut converted = self
                .resampler
                .process(&block[..], None)
                .map_err(|e| anyhow!("downsampler process failed: {}", e))?;
            if let Some(channel) = converted.pop() {
                out.extend_from_slice(&channel);
            }
        }
        Ok(out)
    }

    /// Emit the buffered partial block plus the filter's group-delay tail.
    /// The resampler stays usable afterwards (its delay line is zero-filled).
    pub(crate) fn flush(&mut self) -> Result<Vec<f32>> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let partial = [self.pending.drain(..).collect::<Vec<f32>>()];
            let mut converted = self
                .resampler
                .process_partial(Some(&partial[..]), None)
                .map_err(|e| anyhow!("downsampler partial process failed: {}", e))?;
            if let Some(channel) = converted.pop() {
                out.extend_from_slice(&channel);
            }
        }
        let empty: Option<&[Vec<f32>]> = None;
        let mut converted = self
            .resampler
            .process_partial(empty, None)
            .map_err(|e| anyhow!("downsampler flush failed: {}", e))?;
        if let Some(channel) = converted.pop() {
            out.extend_from_slice(&channel);
        }
        Ok(out)
    }
}

/// Trim a voice-enrollment recording (16 kHz mono) down to its voiced parts.
///
/// Unlike [`extract_speech_16k`], this uses [`ContinuousVadProcessor::new_for_enrollment`]
/// so a person reading a passage continuously isn't force-cut and discarded.
/// The audio is fed in slices, mirroring how the live pipeline drives the VAD.
/// Returns the concatenated speech samples (empty if the VAD found none — the
/// caller decides whether to fall back to the raw buffer).
pub fn extract_enrollment_speech_16k(samples_mono_16k: &[f32]) -> Result<Vec<f32>> {
    let mut processor = ContinuousVadProcessor::new_for_enrollment(16_000)?;

    // 10s slices — cheap, and closer to how audio actually arrives than one
    // giant push.
    const CHUNK_SIZE: usize = 160_000;
    let mut all_segments = Vec::new();
    for chunk in samples_mono_16k.chunks(CHUNK_SIZE) {
        all_segments.extend(processor.process_audio(chunk)?);
    }
    all_segments.extend(processor.flush()?);

    let mut result = Vec::new();
    for segment in &all_segments {
        result.extend_from_slice(&segment.samples);
    }
    debug!(
        "VAD (enrollment): {} samples → {} speech samples from {} segments",
        samples_mono_16k.len(),
        result.len(),
        all_segments.len()
    );
    Ok(result)
}

/// Convenience: get all speech chunks from a 16 kHz mono buffer.
pub fn get_speech_chunks(
    samples_mono_16k: &[f32],
    redemption_time_ms: u32,
) -> Result<Vec<SpeechSegment>> {
    get_speech_chunks_with_progress(samples_mono_16k, redemption_time_ms, |_, _| true)
}

/// Get speech chunks with a progress callback and cancellation support.
/// The callback receives `(progress_percent, segments_found_so_far)` and
/// returning `false` cancels processing.
pub fn get_speech_chunks_with_progress<F>(
    samples_mono_16k: &[f32],
    redemption_time_ms: u32,
    mut progress_callback: F,
) -> Result<Vec<SpeechSegment>>
where
    F: FnMut(u32, usize) -> bool,
{
    let mut processor = ContinuousVadProcessor::new(16_000, redemption_time_ms)?;

    const LARGE_FILE_THRESHOLD: usize = 960_000; // ~60s at 16kHz
    const CHUNK_SIZE: usize = 160_000; // 10s slices for progress granularity

    let total_samples = samples_mono_16k.len();
    let mut all_segments = Vec::new();

    if total_samples > LARGE_FILE_THRESHOLD {
        info!(
            "VAD: processing large file ({} samples = {:.1}s)",
            total_samples,
            total_samples as f64 / 16_000.0
        );

        let mut processed = 0usize;
        let mut last_progress = 0u32;
        let mut chunk_count = 0usize;
        let total_chunks = (total_samples + CHUNK_SIZE - 1) / CHUNK_SIZE;

        for chunk in samples_mono_16k.chunks(CHUNK_SIZE) {
            chunk_count += 1;
            let start_time = std::time::Instant::now();
            let segments = processor.process_audio(chunk)?;
            let elapsed = start_time.elapsed();

            debug!(
                "VAD chunk {}/{} processed in {:?}, found {} segments",
                chunk_count,
                total_chunks,
                elapsed,
                segments.len()
            );
            if elapsed.as_secs() > 1 {
                warn!(
                    "VAD chunk {} took {:?} — possible performance issue",
                    chunk_count, elapsed
                );
            }
            all_segments.extend(segments);

            processed += chunk.len();
            let progress = ((processed * 100) / total_samples) as u32;
            if progress >= last_progress + 5 {
                debug!(
                    "VAD progress {}% ({} segments so far)",
                    progress,
                    all_segments.len()
                );
                if !progress_callback(progress, all_segments.len()) {
                    info!("VAD cancelled by callback at {}%", progress);
                    return Err(anyhow!("VAD processing cancelled"));
                }
                last_progress = progress;
            }
        }

        all_segments.extend(processor.flush()?);
        info!("VAD complete: {} speech segments", all_segments.len());
    } else {
        all_segments = processor.process_audio(samples_mono_16k)?;
        all_segments.extend(processor.flush()?);
    }

    Ok(all_segments)
}

#[cfg(test)]
mod downsampler_tests {
    use super::StreamingDownsampler;

    fn tone(freq: f32, rate: u32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn live_window_yields_800_samples_after_warmup() {
        let mut ds = StreamingDownsampler::new(48_000, 16_000).unwrap();
        // rubato absorbs the filter's group delay (sinc_len / 2 input frames,
        // ≈ 21 output samples) in the very first call; every window after
        // that maps 2400 → 800 exactly, which keeps VAD timestamps aligned.
        let first = ds.process(&vec![0.1; 2400]).unwrap();
        assert!((760..=800).contains(&first.len()), "first window: {}", first.len());
        for _ in 0..20 {
            let out = ds.process(&vec![0.1; 2400]).unwrap();
            assert_eq!(out.len(), 800);
        }
    }

    #[test]
    fn arbitrary_chunking_is_conserved() {
        let mut ds = StreamingDownsampler::new(48_000, 16_000).unwrap();
        let input = tone(440.0, 48_000, 1.0);
        let mut total = 0usize;
        for chunk in input.chunks(517) {
            total += ds.process(chunk).unwrap().len();
        }
        total += ds.flush().unwrap().len();
        // 48 000 in → 16 000 out, minus the group delay absorbed up front,
        // plus the zero block rubato pushes through on flush (≤ one 10 ms
        // block, i.e. ≤ 160 output samples of trailing silence).
        assert!(
            (total as i64 - 16_000).abs() <= 200,
            "got {} samples",
            total
        );
    }

    #[test]
    fn passband_preserved_and_aliases_rejected() {
        // 1 kHz must pass ~unchanged; 11 kHz (above the 8 kHz output Nyquist)
        // must be strongly attenuated instead of folding down to 5 kHz.
        let mut pass = StreamingDownsampler::new(48_000, 16_000).unwrap();
        let mut stop = StreamingDownsampler::new(48_000, 16_000).unwrap();
        let in_pass = tone(1_000.0, 48_000, 1.0);
        let in_stop = tone(11_000.0, 48_000, 1.0);
        let out_pass = pass.process(&in_pass).unwrap();
        let out_stop = stop.process(&in_stop).unwrap();
        // Skip the warm-up region.
        let p = rms(&out_pass[4_000..]);
        let s = rms(&out_stop[4_000..]);
        let in_rms = rms(&in_pass);
        assert!((p - in_rms).abs() / in_rms < 0.05, "passband rms {} vs {}", p, in_rms);
        assert!(s < in_rms * 0.01, "stopband leak rms {} (input {})", s, in_rms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silero_vad_available() -> bool {
        crate::speaker_diarization::model::silero_vad_path()
            .map(|p| crate::speaker_diarization::model::model_is_ready(&p))
            .unwrap_or(false)
    }

    /// Generate synthetic speech-like audio with alternating speech/silence.
    fn generate_test_audio_with_speech(duration_seconds: f32, sample_rate: u32) -> Vec<f32> {
        let total_samples = (duration_seconds * sample_rate as f32) as usize;
        let mut samples = vec![0.0f32; total_samples];

        let speech_interval = 10.0;
        let speech_duration = 5.0;

        for i in 0..total_samples {
            let time = i as f32 / sample_rate as f32;
            let cycle_time = time % speech_interval;
            if cycle_time < speech_duration {
                let freq1 = 200.0 + (time * 50.0).sin() * 100.0;
                let freq2 = freq1 * 2.0;
                let freq3 = freq1 * 3.0;
                let amplitude = 0.3 + 0.1 * (time * 5.0).sin();
                samples[i] = amplitude
                    * (0.5 * (2.0 * std::f32::consts::PI * freq1 * time).sin()
                        + 0.3 * (2.0 * std::f32::consts::PI * freq2 * time).sin()
                        + 0.2 * (2.0 * std::f32::consts::PI * freq3 * time).sin());
            }
        }
        samples
    }

    #[test]
    fn test_vad_chunked_vs_single_processing() {
        if !silero_vad_available() {
            eprintln!("skipping: silero_vad.onnx not present");
            return;
        }
        let audio = generate_test_audio_with_speech(60.0, 16_000);
        let segments_single = get_speech_chunks(&audio, 2000).expect("single failed");
        let segments_chunked =
            get_speech_chunks_with_progress(&audio, 2000, |_, _| true).expect("chunked failed");
        let diff = (segments_single.len() as i32 - segments_chunked.len() as i32).abs();
        assert!(
            diff <= 1,
            "single vs chunked segment counts differ too much: {} vs {}",
            segments_single.len(),
            segments_chunked.len(),
        );
    }

    #[test]
    fn test_vad_large_file_progress() {
        if !silero_vad_available() {
            eprintln!("skipping: silero_vad.onnx not present");
            return;
        }
        let audio = generate_test_audio_with_speech(120.0, 16_000);
        let mut progress_updates = Vec::new();
        let segments = get_speech_chunks_with_progress(&audio, 2000, |progress, segments| {
            progress_updates.push((progress, segments));
            true
        })
        .expect("processing failed");
        assert!(!progress_updates.is_empty());
        // Synthetic audio doesn't always trigger silero, so don't assert
        // segment count — just confirm we made it through without panicking.
        let _ = segments;
    }

    #[test]
    fn test_vad_cancellation() {
        if !silero_vad_available() {
            eprintln!("skipping: silero_vad.onnx not present");
            return;
        }
        let audio = generate_test_audio_with_speech(120.0, 16_000);
        let result = get_speech_chunks_with_progress(&audio, 2000, |progress, _| progress < 50);
        assert!(result.is_err(), "expected cancellation error");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }

    #[test]
    fn test_vad_continuous_processor_state_across_chunks() {
        if !silero_vad_available() {
            eprintln!("skipping: silero_vad.onnx not present");
            return;
        }
        let mut processor =
            ContinuousVadProcessor::new(16_000, 2000).expect("Failed to create processor");
        let audio = generate_test_audio_with_speech(30.0, 16_000);

        let mut all_segments = Vec::new();
        for chunk in audio.chunks(160_000) {
            let segments = processor.process_audio(chunk).expect("process failed");
            all_segments.extend(segments);
        }
        all_segments.extend(processor.flush().expect("flush failed"));
        // Synthetic harmonic stack isn't always speech-like enough to trigger
        // silero VAD; just confirm the processor doesn't panic across chunks.
        let _ = all_segments;
    }
}
