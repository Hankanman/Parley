use anyhow::Result;
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::audio_processing::{audio_to_mono, HighPassFilter, LoudnessNormalizer};
use super::devices::AudioDevice;
use super::recording_state::{AudioChunk, DeviceType, RecordingState};
use super::vad::ContinuousVadProcessor;

/// Per-source bookkeeping for `AudioMixerRingBuffer`.
///
/// `raw_next_pos` is the source's true, contiguous incoming-sample position
/// on the *aligned* (common) timeline — it only ever advances by exactly the
/// length of samples actually received, and is never rewritten by windowing
/// decisions. `buf_start_pos` is the aligned position of `buffer.front()`
/// and is purely a windowing/extraction cursor: it can jump forward when the
/// window cursor gives up waiting on this source (see `forced_gap` in
/// `AudioMixerRingBuffer::take_window`) or when the safety cap drops old
/// samples. Keeping these separate is what lets late data for an
/// already-emitted range be recognized and dropped (via `discard_before`)
/// without corrupting the position of genuinely new data.
#[derive(Default)]
struct SourceState {
    expected: bool,
    seen: bool,
    buffer: VecDeque<f32>,
    buf_start_pos: u64,
    raw_next_pos: u64,
    discard_before: u64,
}

/// Ring buffer for synchronized audio mixing.
///
/// Aligns the asynchronously-arriving mic and system streams by *sample
/// position* on a common timeline, not by arrival order (issue #22). Each
/// source's absolute position is anchored once, the first time it delivers
/// data: the first source to appear anchors at 0, and the second anchors at
/// whatever position the first source had already reached at that moment —
/// capturing the real inter-stream startup skew instead of always treating
/// "first sample from each source" as simultaneous.
///
/// Windows are then extracted at a shared cursor (`window_pos`): window `k`
/// covers common-timeline positions `[k*W, (k+1)*W)` for BOTH sources. If a
/// source has not yet delivered data covering that range we wait, unless the
/// other source has pulled more than `max_lag_samples` ahead — in which case
/// we give up on that range for the lagging source (emit zeros) and advance;
/// any data for that range that arrives later is recognized as late (via
/// `discard_before`) and dropped rather than shifting alignment.
struct AudioMixerRingBuffer {
    mic: SourceState,
    system: SourceState,
    window_size_samples: usize, // Fixed mixing window (e.g., 50ms)
    max_buffer_size: usize,     // Safety limit, sized in absolute time (see `new`)
    max_lag_samples: u64,       // How far one source may lead before we give up waiting
    window_pos: u64,            // Shared aligned-timeline cursor (samples)
    sample_counter: u64,        // Diagnostics only; replaces the old `static mut` counter
}

impl AudioMixerRingBuffer {
    fn new(sample_rate: u32) -> Self {
        // Use 50ms windows for mixing
        let window_ms = 50.0;
        let window_size_samples = (sample_rate as f32 * window_ms / 1000.0) as usize;

        // CRITICAL FIX: Size the safety buffer in absolute time, independent of
        // the mixing window, so shrinking the window (for latency) doesn't also
        // shrink our jitter tolerance. The PipeWire graph delivers mic and
        // system audio as two independently-scheduled streams, so they can
        // drift apart by tens of milliseconds under scheduling pressure
        // before arriving here via channel. Accounts for that jitter plus
        // the processing delay of the mic enhancement chain (HPF + loudness
        // normalization, run AFTER AEC — see `AudioPipeline::run`).
        let max_buffer_ms = 4800.0;
        let max_buffer_size = (sample_rate as f32 * max_buffer_ms / 1000.0) as usize;

        // How far ahead one source may run before we stop waiting on the
        // other and emit a zero gap instead (issue #22).
        let max_lag_ms = 500.0;
        let max_lag_samples = (sample_rate as f32 * max_lag_ms / 1000.0) as u64;

        info!(
            "🔊 Ring buffer initialized: window={}ms ({} samples), max={}ms ({} samples), max_lag={}ms",
            window_ms, window_size_samples, max_buffer_ms, max_buffer_size, max_lag_ms
        );

        Self {
            mic: SourceState {
                expected: true,
                buffer: VecDeque::with_capacity(max_buffer_size),
                ..Default::default()
            },
            system: SourceState {
                expected: true,
                buffer: VecDeque::with_capacity(max_buffer_size),
                ..Default::default()
            },
            window_size_samples,
            max_buffer_size,
            max_lag_samples,
            window_pos: 0,
            sample_counter: 0,
        }
    }

    /// Declare which sources this recording actually has. A source that was
    /// never opened (single-source recording) is treated as permanently
    /// silent rather than something we wait on.
    fn set_expected_sources(&mut self, mic: bool, system: bool) {
        self.mic.expected = mic;
        self.system.expected = system;
    }

    fn add_samples(&mut self, device_type: DeviceType, samples: Vec<f32>) {
        self.sample_counter += 1;
        if self.sample_counter % 200 == 0 {
            debug!(
                "📊 Ring buffer status: mic={} samples (pos={}), sys={} samples (pos={}), window_pos={}",
                self.mic.buffer.len(),
                self.mic.buf_start_pos,
                self.system.buffer.len(),
                self.system.buf_start_pos,
                self.window_pos
            );
        }

        let max_buffer_size = self.max_buffer_size;
        match device_type {
            DeviceType::Microphone => {
                let (other_expected, other_seen, other_raw_next_pos) = (
                    self.system.expected,
                    self.system.seen,
                    self.system.raw_next_pos,
                );
                Self::ingest(
                    &mut self.mic,
                    max_buffer_size,
                    other_expected,
                    other_seen,
                    other_raw_next_pos,
                    samples,
                    "microphone",
                );
            }
            DeviceType::System => {
                let (other_expected, other_seen, other_raw_next_pos) =
                    (self.mic.expected, self.mic.seen, self.mic.raw_next_pos);
                Self::ingest(
                    &mut self.system,
                    max_buffer_size,
                    other_expected,
                    other_seen,
                    other_raw_next_pos,
                    samples,
                    "system",
                );
            }
        }
    }

    /// Absorb newly-arrived samples for one source: establish its alignment
    /// anchor on first arrival, drop any portion that lands before
    /// `discard_before` (data the window cursor already gave up on), and
    /// enforce the absolute-size safety cap.
    fn ingest(
        src: &mut SourceState,
        max_buffer_size: usize,
        other_expected: bool,
        other_seen: bool,
        other_raw_next_pos: u64,
        samples: Vec<f32>,
        label: &str,
    ) {
        if !src.expected || samples.is_empty() {
            return;
        }

        if !src.seen {
            // Anchor: the first source ever seen starts at 0; a source seen
            // later starts wherever the other source's true stream position
            // already is. `.max(discard_before)` guards against the (rare)
            // case where the window cursor already gave up waiting on this
            // still-unseen source before its first chunk showed up.
            let anchor = if other_expected && other_seen {
                other_raw_next_pos
            } else {
                0
            }
            .max(src.discard_before);
            src.seen = true;
            src.buf_start_pos = anchor;
            src.raw_next_pos = anchor;
        }

        let len = samples.len() as u64;
        let incoming_start = src.raw_next_pos;
        src.raw_next_pos = incoming_start + len;

        if incoming_start + len <= src.discard_before {
            // Entirely late — the window cursor already emitted zeros for
            // this whole range and moved on.
            return;
        }

        let drop_prefix = src.discard_before.saturating_sub(incoming_start) as usize;
        if drop_prefix >= samples.len() {
            return;
        }
        let keep = if drop_prefix > 0 {
            samples[drop_prefix..].to_vec()
        } else {
            samples
        };
        if src.buffer.is_empty() {
            src.buf_start_pos = incoming_start + drop_prefix as u64;
        }
        src.buffer.extend(keep);

        if src.buffer.len() > max_buffer_size {
            let excess = src.buffer.len() - max_buffer_size;
            warn!(
                "⚠️ {} buffer overflow: {} > {} samples, dropping oldest {} samples (position preserved)",
                label,
                src.buffer.len(),
                max_buffer_size,
                excess
            );
            for _ in 0..excess {
                src.buffer.pop_front();
            }
            src.buf_start_pos += excess as u64;
        }
    }

    /// How much real data a source can vouch for, on the common timeline —
    /// `u64::MAX`-free stand-in for "never blocks": a source we don't expect
    /// tracks the cursor exactly (always available as silence, never forces
    /// the other side into a gap).
    fn frontier(src: &SourceState, window_pos: u64) -> u64 {
        if !src.expected {
            window_pos
        } else if !src.seen {
            0
        } else {
            src.raw_next_pos
        }
    }

    /// Decide whether the window at the current cursor can be extracted, and
    /// if so, whether either source must be force-filled with zeros because
    /// it has fallen more than `max_lag_samples` behind the other.
    /// Returns `None` when we should keep waiting for more data.
    fn plan_window(&self) -> Option<(bool, bool)> {
        if !self.mic.expected && !self.system.expected {
            return None; // Nothing to mix; avoid spinning forever.
        }

        let wp = self.window_pos;
        let window_end = wp + self.window_size_samples as u64;
        let mic_frontier = Self::frontier(&self.mic, wp);
        let sys_frontier = Self::frontier(&self.system, wp);
        let mic_covered = !self.mic.expected || mic_frontier >= window_end;
        let sys_covered = !self.system.expected || sys_frontier >= window_end;

        if mic_covered && sys_covered {
            return Some((false, false));
        }

        let mut mic_gap = false;
        let mut sys_gap = false;
        if !mic_covered {
            if sys_frontier.saturating_sub(wp) > self.max_lag_samples {
                mic_gap = true;
            } else {
                return None;
            }
        }
        if !sys_covered {
            if mic_frontier.saturating_sub(wp) > self.max_lag_samples {
                sys_gap = true;
            } else {
                return None;
            }
        }
        Some((mic_gap, sys_gap))
    }

    /// Build one source's window at `[wp, wp+w)`, draining and repositioning
    /// its buffer as needed. `forced_gap` means the shared cursor gave up
    /// waiting on this source for this range: emit zeros and mark the range
    /// as late (dropped) if it arrives after all.
    fn take_window(src: &mut SourceState, forced_gap: bool, wp: u64, w: usize) -> Vec<f32> {
        let window_end = wp + w as u64;

        if !src.expected {
            return vec![0.0; w];
        }

        if forced_gap {
            src.discard_before = src.discard_before.max(window_end);
            src.buffer.clear();
            src.buf_start_pos = window_end;
            return vec![0.0; w];
        }

        if !src.seen || src.buf_start_pos >= window_end {
            return vec![0.0; w];
        }

        // Defensive: realign if stale leftover sits before the window start
        // (shouldn't happen given the bookkeeping above, but never emit
        // samples for the wrong position).
        if src.buf_start_pos < wp {
            let stale = (wp - src.buf_start_pos) as usize;
            for _ in 0..stale.min(src.buffer.len()) {
                src.buffer.pop_front();
            }
            src.buf_start_pos = wp;
        }

        let zeros_prefix = (src.buf_start_pos - wp) as usize;
        let mut out = vec![0.0f32; zeros_prefix.min(w)];
        let remaining = w - out.len();
        let take = remaining.min(src.buffer.len());
        for _ in 0..take {
            out.push(src.buffer.pop_front().unwrap_or(0.0));
        }
        out.resize(w, 0.0);
        src.buf_start_pos += take as u64;
        out
    }

    fn extract_window(&mut self) -> Option<(Vec<f32>, Vec<f32>)> {
        let (mic_gap, sys_gap) = self.plan_window()?;
        let wp = self.window_pos;
        let w = self.window_size_samples;
        let mic_window = Self::take_window(&mut self.mic, mic_gap, wp, w);
        let sys_window = Self::take_window(&mut self.system, sys_gap, wp, w);
        self.window_pos += w as u64;
        Some((mic_window, sys_window))
    }

    /// Called once at shutdown: drain every window the normal path would
    /// eventually resolve, then force out any partial remainder
    /// (zero-padded) so the last <1 window of audio isn't silently dropped
    /// (issue #41 part 1).
    fn flush_final(&mut self) -> Vec<(Vec<f32>, Vec<f32>)> {
        let mut windows = Vec::new();
        while let Some(window) = self.extract_window() {
            windows.push(window);
        }

        if !self.mic.buffer.is_empty() || !self.system.buffer.is_empty() {
            let w = self.window_size_samples;
            let mut mic_window: Vec<f32> = self.mic.buffer.drain(..).collect();
            mic_window.resize(w, 0.0);
            let mut sys_window: Vec<f32> = self.system.buffer.drain(..).collect();
            sys_window.resize(w, 0.0);
            self.window_pos += w as u64;
            windows.push((mic_window, sys_window));
        }

        windows
    }
}

/// Captures raw audio from one PipeWire stream (mic or system) and forwards
/// it to the pipeline task.
///
/// PERFORMANCE (issue #28): `process_audio_data` runs directly on PipeWire's
/// real-time data thread (`pw/mod.rs` connects with `RT_PROCESS`), so it is
/// intentionally minimal: one allocation for the mono downmix, no mutexes,
/// no DSP, and no per-call logging. Everything else the old implementation
/// did here — resampling, RNNoise, the high-pass filter, loudness
/// normalization, and the EBU/RMS diagnostic logging — has moved off this
/// thread: PipeWire always negotiates 48 kHz for this app's capture streams
/// (see `pw::CAPTURE_RATE`), so the resampler path was dead code; the mic
/// enhancement chain now runs in `AudioPipeline::run` (see issue #21, on the
/// tokio pipeline task, after AEC).
pub struct AudioCapture {
    state: Arc<RecordingState>,
    sample_rate: u32,
    channels: u16,
    device_type: DeviceType,
    /// Cloned once here, at construction time, instead of locking
    /// `RecordingState`'s sender mutex on every quantum. `RecordingManager`
    /// starts the pipeline (which installs this sender) before it creates
    /// any streams, so this is normally `Some` by the time real audio
    /// arrives; if a stream somehow starts first this is `None` and chunks
    /// are silently dropped, matching the previous "pipeline not ready"
    /// behavior.
    sender: Option<mpsc::UnboundedSender<AudioChunk>>,
    /// Samples sent so far. Used to derive each chunk's timestamp as
    /// `samples_sent / sample_rate` instead of calling
    /// `RecordingState::get_recording_duration()` (a mutex) from the RT
    /// thread.
    samples_sent: u64,
    chunk_counter: u64,
}

impl AudioCapture {
    pub fn new(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        sample_rate: u32,
        channels: u16,
        device_type: DeviceType,
    ) -> Self {
        if sample_rate != 48_000 {
            // PipeWire negotiates the capture format for us (see
            // `pw::CAPTURE_RATE`), so every stream should already be 48 kHz;
            // this is a configuration bug, not something to resample around.
            warn!(
                "[{:?}] Audio device '{}' opened at {} Hz, not the expected 48 kHz",
                device_type, device.name, sample_rate
            );
        }

        let sender = state.cloned_audio_sender();
        if sender.is_none() {
            warn!(
                "[{:?}] AudioCapture for '{}' created before the pipeline sender was ready",
                device_type, device.name
            );
        }

        Self {
            state,
            sample_rate,
            channels,
            device_type,
            sender,
            samples_sent: 0,
            chunk_counter: 0,
        }
    }

    /// Called directly from the PipeWire real-time thread for every
    /// quantum. Keep this on the fast path: no mutexes, no DSP, no
    /// allocation beyond the mono downmix.
    pub fn process_audio_data(&mut self, data: &[f32]) {
        // Both checks are atomics, not mutexes — safe on the RT thread.
        // `is_paused` mirrors the discard-while-paused behavior the old
        // `RecordingState::send_audio_chunk` used to provide.
        if !self.state.is_recording() || self.state.is_paused() {
            return;
        }

        let Some(sender) = &self.sender else {
            return;
        };

        // One allocation per quantum for the mono downmix — unavoidable
        // since ownership of the samples has to move through the channel to
        // the pipeline task.
        let mono_data = if self.channels > 1 {
            audio_to_mono(data, self.channels)
        } else {
            data.to_vec()
        };

        let timestamp = self.samples_sent as f64 / self.sample_rate as f64;
        self.samples_sent += mono_data.len() as u64;

        let chunk_id = self.chunk_counter;
        self.chunk_counter += 1;

        let audio_chunk = AudioChunk {
            data: mono_data,
            sample_rate: self.sample_rate,
            timestamp,
            chunk_id,
            device_type: self.device_type,
        };

        // Best-effort: a closed channel just means the pipeline has shut
        // down (e.g. stop_recording already cleared it). No logging here —
        // this runs on the RT thread — the pipeline shutdown path already
        // logs the transition.
        let _ = sender.send(audio_chunk);
    }
}

/// VAD-driven audio processing pipeline
/// Uses Voice Activity Detection to segment speech in real-time and send only speech to Whisper.
///
/// Source attribution: VAD runs INDEPENDENTLY on the mic and system streams (the un-mixed
/// windows the ring buffer extracts) so each speech segment is tagged with its source
/// (Mic or System). This is the foundation for asymmetric speaker attribution: mic
/// segments are labeled "Me" automatically; system segments feed the diarization layer.
pub struct AudioPipeline {
    receiver: mpsc::UnboundedReceiver<AudioChunk>,
    transcription_sender: mpsc::UnboundedSender<AudioChunk>,
    mic_vad_processor: ContinuousVadProcessor,
    system_vad_processor: ContinuousVadProcessor,
    sample_rate: u32,
    chunk_id_counter: u64,
    // Performance optimization: reduce logging frequency
    last_summary_time: std::time::Instant,
    processed_chunks: u64,
    // Aligns the async mic + system streams into equal-length windows.
    ring_buffer: AudioMixerRingBuffer,
    // Acoustic echo canceller: removes the system audio the mic picks up from
    // the speakers, using the aligned system window as the far-end reference.
    // `None` when AEC couldn't initialize — recording continues without it.
    echo_canceller: Option<super::aec::MicEchoCanceller>,
    // Mic enhancement chain (issue #21): runs AFTER `echo_canceller.cancel`,
    // never before it. AEC3 assumes a linear, time-invariant near-end path;
    // running the high-pass filter and loudness normalizer on the mic
    // upstream of AEC (as the old per-stream `AudioCapture` did) fed AEC a
    // time-varying, gain-boosted, clipped signal and broke its convergence.
    // Mic-only — system audio is left raw.
    mic_high_pass: Option<HighPassFilter>,
    mic_normalizer: Option<LoudnessNormalizer>,
    // Sender for the interleaved stereo recording chunks (mic L, system R).
    recording_sender_for_mixed: Option<mpsc::UnboundedSender<AudioChunk>>,
    // Streaming partials: snapshots of in-progress utterances sent to the
    // partial-decode task. None when streaming partials are disabled.
    partial_sender: Option<mpsc::UnboundedSender<super::recording_state::PartialAudioChunk>>,
    mic_partial: PartialEmitState,
    system_partial: PartialEmitState,
}

/// Per-source bookkeeping that throttles streaming-partial emission.
#[derive(Default)]
struct PartialEmitState {
    /// Monotonic utterance counter, bumped on each silence→speech transition.
    utterance_id: u64,
    /// Whether speech was active on the previous window (edge detection).
    was_active: bool,
    /// Partial-buffer length at the last emitted snapshot, so we only re-emit
    /// after enough new audio has accumulated.
    samples_at_last_emit: usize,
}

// Emit a partial snapshot at most once per ~1.2 s of new speech audio, and
// only once the utterance has at least ~0.8 s of audio (below that whisper has
// little to work with and tends to hallucinate).
const PARTIAL_MIN_SAMPLES: usize = 12_800; // 0.8 s @ 16 kHz
const PARTIAL_EMIT_INTERVAL_SAMPLES: usize = 19_200; // 1.2 s @ 16 kHz

/// Decide whether a streaming-partial snapshot should be emitted for one
/// source, using edge detection (silence→speech bumps the utterance id) and
/// a new-audio interval throttle. Returns `Some(utterance_id)` when a
/// snapshot should be sent, `None` otherwise.
///
/// Deliberately takes only the buffer *length* (not the buffer itself) so
/// callers can run this cheap check before deciding whether the
/// (potentially large) partial buffer is worth cloning — see the call site
/// in [`AudioPipeline::run_vad_for_source`].
fn partial_emit_decision(
    state: &mut PartialEmitState,
    active: bool,
    partial_len: usize,
) -> Option<u64> {
    // Edge: silence → speech starts a new utterance.
    if active && !state.was_active {
        state.utterance_id += 1;
        state.samples_at_last_emit = 0;
    }
    // Edge: speech → silence ends the utterance (the final path takes over).
    if !active && state.was_active {
        state.samples_at_last_emit = 0;
    }
    state.was_active = active;

    if !active || partial_len < PARTIAL_MIN_SAMPLES {
        return None;
    }

    let new_since_emit = partial_len.saturating_sub(state.samples_at_last_emit);
    if new_since_emit < PARTIAL_EMIT_INTERVAL_SAMPLES {
        return None;
    }
    state.samples_at_last_emit = partial_len;

    Some(state.utterance_id)
}

impl AudioPipeline {
    pub fn new(
        receiver: mpsc::UnboundedReceiver<AudioChunk>,
        transcription_sender: mpsc::UnboundedSender<AudioChunk>,
        target_chunk_duration_ms: u32,
        sample_rate: u32,
        mic_device_name: String,
        system_device_name: String,
        mic_present: bool,
        system_present: bool,
    ) -> Self {
        info!(
            "🎛️ AudioPipeline initializing: mic='{}' system='{}'",
            mic_device_name, system_device_name
        );

        let _ = (mic_device_name, system_device_name);

        // Redemption time = the trailing-silence gap that ends a segment.
        // Per-source, because the two streams have different needs:
        //  - Mic (AEC-cleaned, usually just the local user): a looser 400ms gap
        //    bridges natural pauses so one person's speech isn't fragmented.
        //  - System (all the remote participants): a tighter gap so back-to-back
        //    remote speakers split into separate segments instead of merging
        //    into one — otherwise the whole turn gets a single speaker label.
        // Over-splitting is safe (extra pieces re-cluster to the same speaker);
        // merging two speakers into one segment is the error we're avoiding.
        let mic_redemption_time = 400;
        let system_redemption_time = 250;

        // Dual VAD: separate processors per source so segments arrive at the
        // transcription stage tagged with origin (Mic vs System).
        let mic_vad_processor = match ContinuousVadProcessor::new_with_source(
            sample_rate,
            mic_redemption_time,
            DeviceType::Microphone,
        ) {
            Ok(processor) => {
                info!("VAD-driven pipeline: mic VAD ready (source=Microphone)");
                processor
            }
            Err(e) => {
                error!("Failed to create mic VAD processor: {}", e);
                panic!("Mic VAD processor creation failed: {}", e);
            }
        };

        let system_vad_processor = match ContinuousVadProcessor::new_with_source(
            sample_rate,
            system_redemption_time,
            DeviceType::System,
        ) {
            Ok(processor) => {
                info!("VAD-driven pipeline: system VAD ready (source=System)");
                processor
            }
            Err(e) => {
                error!("Failed to create system VAD processor: {}", e);
                panic!("System VAD processor creation failed: {}", e);
            }
        };

        // Ring buffer aligns the asynchronously-arriving mic and system
        // streams into equal-length windows for interleaving. Tell it which
        // sources actually exist for this recording (issue #22): a source
        // that was never opened (mic-only or system-only recording) is
        // permanently silent rather than something to wait on.
        let mut ring_buffer = AudioMixerRingBuffer::new(sample_rate);
        ring_buffer.set_expected_sources(mic_present, system_present);
        // Echo canceller for the mic (uses the system window as reference).
        let echo_canceller = super::aec::MicEchoCanceller::new(sample_rate);

        // Mic enhancement chain (issue #21) — see the field docs on
        // `mic_high_pass` / `mic_normalizer` for why this now lives here,
        // downstream of AEC, instead of in the per-stream `AudioCapture`.
        let mic_high_pass = Some(HighPassFilter::new(sample_rate, 80.0));
        let mic_normalizer = match LoudnessNormalizer::new(1, sample_rate) {
            Ok(normalizer) => {
                info!("✅ EBU R128 normalizer initialized for microphone (target: -23 LUFS, capped +18/-12 dB)");
                Some(normalizer)
            }
            Err(e) => {
                warn!(
                    "⚠️ Failed to create mic loudness normalizer: {}, normalization disabled",
                    e
                );
                None
            }
        };

        // Note: target_chunk_duration_ms is ignored - VAD controls segmentation now
        let _ = target_chunk_duration_ms;

        Self {
            receiver,
            transcription_sender,
            mic_vad_processor,
            system_vad_processor,
            sample_rate,
            chunk_id_counter: 0,
            // Performance optimization: reduce logging frequency
            last_summary_time: std::time::Instant::now(),
            processed_chunks: 0,
            // Ring buffer for aligning mic + system into interleaved windows
            ring_buffer,
            echo_canceller,
            mic_high_pass,
            mic_normalizer,
            recording_sender_for_mixed: None, // Will be set by manager
            partial_sender: None,             // Will be set by manager if enabled
            mic_partial: PartialEmitState::default(),
            system_partial: PartialEmitState::default(),
        }
    }

    /// Run the VAD-driven audio processing pipeline
    pub async fn run(mut self) -> Result<()> {
        info!("VAD-driven audio pipeline started - segments sent in real-time based on speech detection");

        // CRITICAL FIX: Continue processing until channel is closed, not based on recording state
        // This ensures ALL chunks are processed during shutdown, fixing premature meeting completion
        // Previous bug: Loop checked `while self.state.is_recording()` which caused early exit when
        // stop_recording() was called, losing flush signals and remaining chunks in the pipeline
        loop {
            // Receive audio chunks with timeout
            match tokio::time::timeout(
                std::time::Duration::from_millis(50), // Shorter timeout for responsiveness
                self.receiver.recv(),
            )
            .await
            {
                Ok(Some(chunk)) => {
                    // PERFORMANCE: Check for flush signal (special chunk with ID >= u64::MAX - 10)
                    // Multiple flush signals may be sent to ensure processing
                    if chunk.chunk_id >= u64::MAX - 10 {
                        info!(
                            "📥 Received FLUSH signal #{} - flushing VAD processor",
                            u64::MAX - chunk.chunk_id
                        );
                        self.flush_remaining_audio()?;
                        // Continue processing to handle any remaining chunks
                        continue;
                    }

                    // PERFORMANCE OPTIMIZATION: Eliminate per-chunk logging overhead
                    // Logging in hot paths causes severe performance degradation
                    self.processed_chunks += 1;

                    // CRITICAL: Log summary only every 200 chunks OR every 60 seconds (99.5% reduction)
                    // This eliminates I/O overhead in the audio processing hot path
                    // Use performance-optimized debug macro that compiles to nothing in release builds
                    if self.processed_chunks % 200 == 0
                        || self.last_summary_time.elapsed().as_secs() >= 60
                    {
                        // Only computed in debug builds: `perf_debug!` compiles
                        // to nothing in release, and this sums a whole chunk.
                        #[cfg(debug_assertions)]
                        {
                            let avg_level = if chunk.data.is_empty() {
                                0.0
                            } else {
                                chunk.data.iter().map(|&x| x.abs()).sum::<f32>()
                                    / chunk.data.len() as f32
                            };
                            perf_debug!(
                                "Pipeline processed {} chunks, current chunk: {} ({} samples, avg level {:.4})",
                                self.processed_chunks,
                                chunk.chunk_id,
                                chunk.data.len(),
                                avg_level
                            );
                        }
                        self.last_summary_time = std::time::Instant::now();
                    }

                    // STEP 1: Add raw audio to ring buffer for mixing
                    // Microphone audio is already normalized at capture level (AudioCapture)
                    // System audio remains raw
                    self.ring_buffer.add_samples(chunk.device_type, chunk.data);

                    // STEP 2: Process audio in fixed windows when streams have sufficient data.
                    // Each window yields un-mixed mic + system slices used for source-tagged
                    // VAD, plus a mixed slice used only for the recording WAV.
                    while let Some((mic_window, sys_window)) = self.ring_buffer.extract_window() {
                        self.process_window(mic_window, sys_window, chunk.timestamp);
                    }
                }
                Ok(None) => {
                    info!(
                        "Audio pipeline: sender closed after processing {} chunks",
                        self.processed_chunks
                    );
                    break;
                }
                Err(_) => {
                    // Timeout - just continue, VAD handles all segmentation
                    continue;
                }
            }
        }

        // Flush any remaining VAD segments
        self.flush_remaining_audio()?;

        info!("VAD-driven audio pipeline ended");
        Ok(())
    }

    /// Run one aligned mic/system window through AEC, mic enhancement,
    /// source-tagged VAD, and the stereo recording split. Shared by the main
    /// receive loop and `flush_remaining_audio` (issue #22/#41), so the
    /// zero-padded tail window emitted at shutdown gets identical treatment
    /// to every window processed during recording.
    fn process_window(&mut self, mut mic_window: Vec<f32>, sys_window: Vec<f32>, timestamp: f64) {
        // STEP 2.5: Acoustic echo cancellation. Subtract the
        // system audio (played through the user's speakers)
        // from the mic, using the aligned system window as
        // the far-end reference. Runs before VAD and the
        // recording split, so the cleaned mic flows to both
        // transcription and the mic recording channel — a
        // remote speaker no longer bleeds in as a duplicate
        // "Me", and mic-side playback loses its echo.
        if let Some(ref mut aec) = self.echo_canceller {
            aec.cancel(&mut mic_window, &sys_window);
        }

        // STEP 2.7: Mic enhancement (issue #21) — high-pass
        // then loudness normalization, mic only, and
        // deliberately AFTER AEC (see field docs on
        // `mic_high_pass`/`mic_normalizer`). System audio
        // is left untouched.
        if let Some(ref mut hpf) = self.mic_high_pass {
            mic_window = hpf.process(&mic_window);
        }
        if let Some(ref mut normalizer) = self.mic_normalizer {
            mic_window = normalizer.normalize_loudness(&mic_window);
        }

        // STEP 3: Source-tagged VAD on each stream independently
        self.run_vad_for_source(&mic_window, DeviceType::Microphone);
        self.run_vad_for_source(&sys_window, DeviceType::System);

        // STEP 4: Interleave the two sources into a stereo
        // frame for the recording — mic = left, system =
        // right — so playback and downstream processing can
        // keep them apart (source-aware per-segment
        // playback, echo cancellation, …). The mono downmix
        // is derived on demand (the decoder averages
        // channels) rather than stored.
        let frames = mic_window.len().max(sys_window.len());
        let mut stereo = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            stereo.push(mic_window.get(i).copied().unwrap_or(0.0)); // L: mic
            stereo.push(sys_window.get(i).copied().unwrap_or(0.0)); // R: system
        }

        if let Some(ref sender) = self.recording_sender_for_mixed {
            let recording_chunk = AudioChunk {
                data: stereo,
                sample_rate: self.sample_rate,
                timestamp,
                chunk_id: self.chunk_id_counter,
                // device_type is unused by the saver for the
                // stereo recording chunk; left as Microphone.
                device_type: DeviceType::Microphone,
            };
            let _ = sender.send(recording_chunk);
        }
    }

    fn flush_remaining_audio(&mut self) -> Result<()> {
        info!(
            "Flushing remaining audio from pipeline (processed {} chunks)",
            self.processed_chunks
        );

        // Drain any windows the ring buffer still owes us, including a final
        // zero-padded partial window for the last <1 window of audio (issue
        // #41 part 1), before flushing the VAD processors below.
        let sample_rate = self.sample_rate as f64;
        let final_windows = self.ring_buffer.flush_final();
        for (mic_window, sys_window) in final_windows {
            let timestamp = self.ring_buffer.window_pos as f64 / sample_rate;
            self.process_window(mic_window, sys_window, timestamp);
        }

        // Flush both VAD processors so any in-flight speech is emitted with its source tag.
        match self.mic_vad_processor.flush() {
            Ok(final_segments) => self.dispatch_segments(final_segments, "final-mic"),
            Err(e) => warn!("Failed to flush mic VAD processor: {}", e),
        }
        match self.system_vad_processor.flush() {
            Ok(final_segments) => self.dispatch_segments(final_segments, "final-system"),
            Err(e) => warn!("Failed to flush system VAD processor: {}", e),
        }

        Ok(())
    }

    /// Run VAD over a single-source window and dispatch any completed segments.
    fn run_vad_for_source(&mut self, window: &[f32], source: DeviceType) {
        let processor = match source {
            DeviceType::Microphone => &mut self.mic_vad_processor,
            DeviceType::System => &mut self.system_vad_processor,
        };
        let segments = match processor.process_audio(window) {
            Ok(segments) => segments,
            Err(e) => {
                warn!("⚠️ {} VAD error: {}", source_label(source), e);
                return;
            }
        };

        // Streaming partial emission (best-effort, never blocks the final path).
        // Read speech-active + in-progress buffer BEFORE dispatch clears state.
        //
        // The throttle decision is evaluated first, using only the buffer's
        // current *length* — the (potentially large) partial buffer itself is
        // only cloned once we know a snapshot will actually be emitted.
        // Previously the buffer was cloned on every ~50ms window while speech
        // was active and then usually discarded by the throttle below.
        if self.partial_sender.is_some() {
            let active = processor.is_speech_active();
            let partial_len = processor.partial_samples().len();
            let partial_state = match source {
                DeviceType::Microphone => &mut self.mic_partial,
                DeviceType::System => &mut self.system_partial,
            };
            if let Some(utterance_id) = partial_emit_decision(partial_state, active, partial_len) {
                let snapshot = processor.partial_samples().to_vec();
                if let Some(sender) = &self.partial_sender {
                    let _ = sender.send(super::recording_state::PartialAudioChunk {
                        samples: snapshot,
                        source,
                        utterance_id,
                    });
                }
            }
        }

        self.dispatch_segments(segments, source_label(source));
    }

    /// Send VAD segments to the transcription channel, preserving source identity
    /// on each emitted AudioChunk (chunk.device_type carries the speaker source).
    fn dispatch_segments(
        &mut self,
        segments: Vec<super::vad::SpeechSegment>,
        context: &str,
    ) {
        for segment in segments {
            let duration_ms = segment.end_timestamp_ms - segment.start_timestamp_ms;

            // Minimum 50ms at 16kHz — matches Whisper's minimum-input expectation.
            if segment.samples.len() < 800 {
                debug!(
                    "⏭️ Dropping short {} VAD segment: {:.1}ms ({} samples < 800)",
                    context,
                    duration_ms,
                    segment.samples.len()
                );
                continue;
            }

            info!(
                "📤 Sending {} VAD segment: {:.1}ms, {} samples (source={:?})",
                context,
                duration_ms,
                segment.samples.len(),
                segment.source,
            );

            let transcription_chunk = AudioChunk {
                data: segment.samples,
                sample_rate: 16000,
                timestamp: segment.start_timestamp_ms / 1000.0,
                chunk_id: self.chunk_id_counter,
                device_type: segment.source,
            };

            if let Err(e) = self.transcription_sender.send(transcription_chunk) {
                warn!("Failed to send {} VAD segment: {}", context, e);
            } else {
                self.chunk_id_counter += 1;
            }
        }
    }
}

fn source_label(source: DeviceType) -> &'static str {
    match source {
        DeviceType::Microphone => "mic",
        DeviceType::System => "system",
    }
}

/// Simple audio pipeline manager
pub struct AudioPipelineManager {
    pipeline_handle: Option<JoinHandle<Result<()>>>,
    audio_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
}

impl AudioPipelineManager {
    pub fn new() -> Self {
        Self {
            pipeline_handle: None,
            audio_sender: None,
        }
    }

    /// Start the audio pipeline with device information for adaptive buffering
    pub fn start(
        &mut self,
        state: Arc<RecordingState>,
        transcription_sender: mpsc::UnboundedSender<AudioChunk>,
        target_chunk_duration_ms: u32,
        sample_rate: u32,
        recording_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
        partial_sender: Option<mpsc::UnboundedSender<super::recording_state::PartialAudioChunk>>,
        mic_device_name: String,
        system_device_name: String,
        mic_present: bool,
        system_present: bool,
    ) -> Result<()> {
        // Log device information
        info!("🎙️ Starting pipeline with device info:");
        info!("   Microphone: '{}'", mic_device_name);
        info!("   System Audio: '{}'", system_device_name);

        // Create audio processing channel
        let (audio_sender, audio_receiver) = mpsc::unbounded_channel::<AudioChunk>();

        // Set sender in state for audio captures to use
        state.set_audio_sender(audio_sender.clone());

        // Create and start pipeline with device information for adaptive mixing
        let mut pipeline = AudioPipeline::new(
            audio_receiver,
            transcription_sender,
            target_chunk_duration_ms,
            sample_rate,
            mic_device_name,
            system_device_name,
            mic_present,
            system_present,
        );

        // CRITICAL FIX: Connect recording sender to receive pre-mixed audio
        // This ensures both mic AND system audio are captured in recordings
        pipeline.recording_sender_for_mixed = recording_sender;
        // Streaming partials (None when disabled).
        pipeline.partial_sender = partial_sender;

        let handle = tokio::spawn(async move { pipeline.run().await });

        self.pipeline_handle = Some(handle);
        self.audio_sender = Some(audio_sender);

        info!("Audio pipeline manager started with mixed audio recording");
        Ok(())
    }

    /// Stop the audio pipeline
    pub async fn stop(&mut self) -> Result<()> {
        // Drop the sender to close the pipeline
        self.audio_sender = None;

        // Wait for pipeline to finish
        if let Some(handle) = self.pipeline_handle.take() {
            match handle.await {
                Ok(result) => result,
                Err(e) => {
                    error!("Pipeline task failed: {}", e);
                    Ok(())
                }
            }
        } else {
            Ok(())
        }
    }

    /// Force immediate flush of accumulated audio and stop pipeline
    /// PERFORMANCE CRITICAL: Eliminates 30+ second shutdown delays
    pub async fn force_flush_and_stop(&mut self) -> Result<()> {
        info!("🚀 Force flushing pipeline - processing ALL accumulated audio immediately");

        // If we have a sender, send a special flush signal first
        if let Some(sender) = &self.audio_sender {
            // Create a special flush chunk to trigger immediate processing
            let flush_chunk = AudioChunk {
                data: vec![], // Empty data signals flush
                sample_rate: 16000,
                timestamp: 0.0,
                chunk_id: u64::MAX, // Special ID to indicate flush
                device_type: super::recording_state::DeviceType::Microphone,
            };

            if let Err(e) = sender.send(flush_chunk) {
                warn!("Failed to send flush signal: {}", e);
            } else {
                info!("📤 Sent flush signal to pipeline");

                // PERFORMANCE OPTIMIZATION: Reduced wait time from 50ms to 20ms
                // Pipeline should process flush signal very quickly
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

                // Send multiple flush signals to ensure the pipeline catches it
                // This aggressive approach eliminates shutdown delay issues
                for i in 0..3 {
                    let additional_flush = AudioChunk {
                        data: vec![],
                        sample_rate: 16000,
                        timestamp: 0.0,
                        chunk_id: u64::MAX - (i as u64),
                        device_type: super::recording_state::DeviceType::Microphone,
                    };
                    let _ = sender.send(additional_flush);
                }

                info!("📤 Sent additional flush signals for reliability");
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        }

        // Now stop normally
        self.stop().await
    }
}

impl Default for AudioPipelineManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod ring_buffer_tests {
    use super::*;

    const SR: u32 = 48_000;
    // window_size_samples for 48kHz/50ms.
    const W: usize = 2_400;

    fn buf() -> AudioMixerRingBuffer {
        let mut b = AudioMixerRingBuffer::new(SR);
        b.set_expected_sources(true, true);
        b
    }

    fn pattern(start: usize, len: usize) -> Vec<f32> {
        (start..start + len).map(|i| i as f32).collect()
    }

    /// 1) Both sources in lockstep: windows pair identical positions.
    ///
    /// The very first call from each source establishes its anchor — the
    /// first-ever source anchors at 0, the second anchors wherever the
    /// first's position already is. Sending mic-then-system each round means
    /// system's fixed anchor is exactly one window behind mic's; from then
    /// on every subsequent round holds that SAME fixed skew rather than
    /// drifting further — that stability is what this test checks.
    #[test]
    fn lockstep_pairs_matching_positions() {
        let mut b = buf();
        let mic_base = 0;
        let sys_base = 1_000_000;
        for k in 0..4 {
            b.add_samples(DeviceType::Microphone, pattern(mic_base + k * W, W));
            b.add_samples(DeviceType::System, pattern(sys_base + k * W, W));
        }
        for k in 0..4 {
            let (mic, sys) = b.extract_window().expect("window ready");
            assert_eq!(mic, pattern(mic_base + k * W, W), "mic window {k}");
            if k == 0 {
                // System's anchor (established on its first-ever call) is
                // one window behind mic's — window 0 is entirely before it.
                assert!(sys.iter().all(|&s| s == 0.0), "system window 0");
            } else {
                assert_eq!(
                    sys,
                    pattern(sys_base + (k - 1) * W, W),
                    "system window {k}"
                );
            }
        }
        assert!(b.extract_window().is_none());
    }

    /// 2) System starts 120ms late: first windows are zero on the system
    /// side, later windows align by position (not arrival).
    #[test]
    fn late_starting_source_aligns_by_position() {
        let mut b = buf();
        // Mic delivers 120ms (5760 samples) in 480-sample chunks before
        // system ever shows up.
        let late_start = (SR as usize * 120) / 1000; // 5760
        let mut sent = 0;
        while sent < late_start {
            b.add_samples(DeviceType::Microphone, pattern(sent, 480));
            sent += 480;
        }
        assert_eq!(sent, late_start);
        // Now system arrives — its raw position 0 anchors at `late_start`.
        b.add_samples(DeviceType::System, pattern(0, 480));
        // Keep mic flowing well past window 2 (7200 samples) and system
        // flowing too, so nothing here is gated on waiting for more data.
        while sent < 3 * W {
            b.add_samples(DeviceType::Microphone, pattern(sent, 480));
            sent += 480;
        }
        b.add_samples(DeviceType::System, vec![7.0; 4 * 480]); // plenty of system data

        // Window 0 [0, W): mic has real data, system hasn't started yet -> zero.
        let (mic0, sys0) = b.extract_window().expect("window 0 ready");
        assert_eq!(mic0, pattern(0, W));
        assert!(sys0.iter().all(|&s| s == 0.0));

        // Window 1 [W, 2W) = [2400, 4800): still entirely before system's
        // anchor (5760) -> zero.
        let (_, sys1) = b.extract_window().expect("window 1 ready");
        assert!(sys1.iter().all(|&s| s == 0.0));

        // Window 2 [4800, 7200): system's anchor (5760) falls inside this
        // window -> zero prefix, then system's first chunk (pattern(0,480)),
        // then the follow-on 7.0 chunk — all positioned correctly.
        let (_, sys2) = b.extract_window().expect("window 2 ready");
        let zero_prefix = late_start - 2 * W; // 5760 - 4800 = 960
        assert!(sys2[..zero_prefix].iter().all(|&s| s == 0.0));
        assert_eq!(sys2[zero_prefix..zero_prefix + 480], pattern(0, 480)[..]);
        assert_eq!(
            sys2[zero_prefix + 480..],
            vec![7.0; W - zero_prefix - 480][..]
        );
    }

    /// 3) A burst (several windows at once) from one source doesn't shift
    /// pairing relative to the steadily-delivered other source.
    #[test]
    fn burst_does_not_shift_alignment() {
        let mut b = buf();
        let sys_base = 500_000;
        // Mic sends one window first, establishing the shared anchor (mic=0).
        b.add_samples(DeviceType::Microphone, pattern(0, W));
        // System then delivers 3 windows' worth in a single burst call. Its
        // anchor is mic's position at that moment (W), so its burst covers
        // aligned windows 1..4, not 0..3.
        b.add_samples(DeviceType::System, pattern(sys_base, 3 * W));
        // Mic keeps delivering steadily, one window at a time.
        for k in 1..4 {
            b.add_samples(DeviceType::Microphone, pattern(k * W, W));
        }

        for k in 0..4 {
            let (mic, sys) = b.extract_window().expect("window ready");
            assert_eq!(mic, pattern(k * W, W), "mic window {k} shifted");
            if k == 0 {
                assert!(sys.iter().all(|&s| s == 0.0), "system window 0");
            } else {
                // The burst is drained one window at a time regardless of
                // having arrived as a single call — no shift between windows.
                assert_eq!(
                    sys,
                    pattern(sys_base + (k - 1) * W, W),
                    "system window {k} shifted by the burst"
                );
            }
        }
    }

    /// 4) A source lagging more than MAX_LAG produces zero gaps for the
    /// laggard, and its late data (once it finally arrives) is dropped.
    #[test]
    fn lag_beyond_max_produces_gaps_and_drops_late_data() {
        let mut b = buf();
        // Two windows in lockstep to get both sources seen and aligned.
        b.add_samples(DeviceType::Microphone, pattern(0, 2 * W));
        b.add_samples(DeviceType::System, pattern(0, 2 * W));
        let (_, _) = b.extract_window().unwrap();
        let (_, _) = b.extract_window().unwrap();
        assert_eq!(b.window_pos, (2 * W) as u64);

        // Mic races ahead by well over MAX_LAG (500ms = 24_000 samples);
        // system sends nothing more.
        b.add_samples(DeviceType::Microphone, pattern(2 * W, 30_000));

        let mut windows = 0;
        let mut saw_system_gap = false;
        while let Some((_, sys)) = b.extract_window() {
            windows += 1;
            if sys.iter().all(|&s| s == 0.0) {
                saw_system_gap = true;
            }
            if windows > 50 {
                break; // safety net against a runaway loop in a broken impl
            }
        }
        assert!(windows > 0, "expected forced-gap windows to be emitted");
        assert!(saw_system_gap, "lagging system side should be zero-filled");

        // The discard threshold has moved forward; system's real (but
        // stale) continuation data — still starting at its true position,
        // 2*W — must now be dropped rather than accepted.
        let discard_before = b.system.discard_before;
        assert!(discard_before > (2 * W) as u64);
        b.add_samples(DeviceType::System, pattern(2 * W, W));
        assert!(
            b.system.buffer.is_empty(),
            "late system data should have been dropped, not buffered"
        );
    }

    /// 5) Overflow drops from the front but advances the buffer's start
    /// position so alignment is preserved.
    #[test]
    fn overflow_preserves_position() {
        let mut b = AudioMixerRingBuffer::new(SR);
        b.set_expected_sources(true, false); // system not present this recording

        let max = b.max_buffer_size;
        let total = max + 10_000;
        b.add_samples(DeviceType::Microphone, pattern(0, total));

        assert_eq!(b.mic.buffer.len(), max);
        assert_eq!(b.mic.buf_start_pos, (total - max) as u64);
        // The retained tail must still hold the correct (unshifted) values.
        assert_eq!(b.mic.buffer.front().copied(), Some((total - max) as f32));
        assert_eq!(b.mic.raw_next_pos, total as u64);
    }

    /// 6) Shutdown flush emits the zero-padded tail instead of dropping it.
    /// Single-source setup keeps the arithmetic focused on the flush
    /// mechanism itself rather than on cross-source anchor skew (covered by
    /// the other tests).
    #[test]
    fn shutdown_flush_emits_padded_tail() {
        let mut b = AudioMixerRingBuffer::new(SR);
        b.set_expected_sources(true, false);
        let leftover = 600usize;
        b.add_samples(DeviceType::Microphone, pattern(0, W + leftover));

        // Drain the one full window normally.
        let (mic, sys) = b.extract_window().expect("full window ready");
        assert_eq!(mic, pattern(0, W));
        assert!(sys.iter().all(|&s| s == 0.0));

        // Nothing else is a full window yet, and the lag is well under
        // MAX_LAG, so the normal path won't emit it.
        assert!(b.extract_window().is_none());

        let tail = b.flush_final();
        assert_eq!(tail.len(), 1, "exactly one padded tail window expected");
        let (mic_tail, sys_tail) = &tail[0];
        assert_eq!(mic_tail.len(), W);
        assert_eq!(sys_tail.len(), W);
        assert_eq!(mic_tail[..leftover], pattern(W, leftover)[..]);
        assert!(mic_tail[leftover..].iter().all(|&s| s == 0.0));
        assert!(sys_tail.iter().all(|&s| s == 0.0));
    }

    /// A single-source recording (system never opened) treats system as
    /// permanently silent instead of blocking on it.
    #[test]
    fn single_source_recording_never_waits_on_absent_source() {
        let mut b = AudioMixerRingBuffer::new(SR);
        b.set_expected_sources(true, false);
        b.add_samples(DeviceType::Microphone, pattern(0, W));
        let (mic, sys) = b.extract_window().expect("mic-only window ready");
        assert_eq!(mic, pattern(0, W));
        assert!(sys.iter().all(|&s| s == 0.0));
    }
}
