//! Speakerphone voice chain for the microphone (WebRTC APM).
//!
//! Parley assumes calls are taken on speakerphone: the far-end audio comes out
//! of the speakers and the (desk condenser) mic hears it plus the room. The
//! system-audio capture gives us the exact far-end reference — the textbook
//! AEC setup — so the mic runs through the full WebRTC voice chain before VAD,
//! transcription and the left recording channel:
//!
//! 1. high-pass filter (full band) — rumble/DC, also required by AEC3
//! 2. AEC3 echo cancellation, internal delay estimation (stream delay unset,
//!    so it tracks the speaker→mic acoustic delay)
//! 3. noise suppression (High)
//! 4. GainController2 adaptive digital (replaces the old EBU R128
//!    normaliser): brings speech to a usable level but, via
//!    `max_output_noise_level_dbfs`, never lifts the room noise floor
//!
//! The system (right) channel is not touched.
//!
//! WebRTC's audio processing module works on fixed 10 ms frames (sample_rate /
//! 100 samples per channel). The pipeline runs at 48 kHz with 50 ms windows, so
//! each window is exactly five 480-sample frames — no cross-window buffering.
//! The chain adds no algorithmic delay beyond one frame, so the mic/system
//! alignment in `AudioMixerRingBuffer` is unaffected.

use log::{info, warn};
use webrtc_audio_processing::config::{
    AdaptiveDigital, EchoCanceller, FixedDigital, GainController, GainController2, HighPassFilter,
    NoiseSuppression, NoiseSuppressionLevel,
};
use webrtc_audio_processing::{Config, Processor};

/// Pipeline sample rate the chain is wired for. WebRTC APM accepts
/// 8/16/32/48 kHz; the capture layer resamples everything to 48 kHz.
const SAMPLE_RATE: u32 = 48_000;
/// 10 ms @ 48 kHz — WebRTC APM's fixed per-channel frame size.
const FRAME: usize = (SAMPLE_RATE / 100) as usize;

/// The APM configuration used for the mic.
pub(crate) fn speakerphone_config() -> Config {
    Config {
        high_pass_filter: Some(HighPassFilter {
            apply_in_full_band: true,
        }),
        // Full AEC3 with the internal delay estimator (stream delay unset),
        // so it adapts to the mic<->speaker acoustic delay. The crate exposes
        // no further AEC3 tuning (suppression levels etc.) for loudspeakers.
        echo_canceller: Some(EchoCanceller::Full {
            stream_delay_ms: None,
        }),
        noise_suppression: Some(NoiseSuppression {
            // VeryHigh: on the synthetic -35 dBFS noise scene it takes the
            // floor down 13.6 dB (High managed 7.2) with near-end speech level
            // unchanged. The output feeds Whisper, not a listener, so a
            // slightly more processed voice is the right trade for less
            // noise to hallucinate on.
            level: NoiseSuppressionLevel::VeryHigh,
            // Analysing AEC3's linear output needs it exported, which this
            // crate's config can't turn on; measured no difference either way.
            analyze_linear_aec_output: false,
        }),
        gain_controller: Some(GainController::GainController2(GainController2 {
            input_volume_controller_enabled: false,
            adaptive_digital: Some(AdaptiveDigital {
                headroom_db: 5.0,
                max_gain_db: 30.0,
                initial_gain_db: 10.0,
                max_gain_change_db_per_second: 3.0,
                // Never amplify the noise floor: gain is held back when the
                // estimated noise level at the output would exceed this.
                max_output_noise_level_dbfs: -50.0,
            }),
            fixed_digital: FixedDigital { gain_db: 0.0 },
        })),
        ..Default::default()
    }
}

/// Wraps a WebRTC processor configured as a mono speakerphone voice chain.
pub struct MicProcessor {
    processor: Processor,
}

impl MicProcessor {
    /// Create the chain for the given pipeline sample rate. Returns `None`
    /// (mic left unprocessed, recording still works) if the rate isn't the
    /// expected 48 kHz or the processor can't be created.
    pub fn new(sample_rate: u32) -> Option<Self> {
        if sample_rate != SAMPLE_RATE {
            warn!(
                "Mic processing disabled: pipeline sample rate {} Hz != {} Hz",
                sample_rate, SAMPLE_RATE
            );
            return None;
        }
        match Processor::new(sample_rate) {
            Ok(processor) => {
                processor.set_config(speakerphone_config());
                info!("✅ Mic speakerphone chain enabled (WebRTC: HPF + AEC3 + NS High + AGC2)");
                Some(Self { processor })
            }
            Err(e) => {
                warn!("Mic processor init failed ({e:?}); continuing without AEC/NS/AGC");
                None
            }
        }
    }

    /// Run the voice chain on `mic` in place, using `reference` (the system
    /// audio played to the speakers) as the AEC far-end reference. Processes
    /// as many whole 10 ms frames as both slices share; any short trailing
    /// remainder is left untouched (windows are always whole frames, so this
    /// is only a defensive guard).
    pub fn process(&mut self, mic: &mut [f32], reference: &[f32]) {
        for (mic_frame, ref_frame) in mic.chunks_mut(FRAME).zip(reference.chunks(FRAME)) {
            if mic_frame.len() != FRAME || ref_frame.len() != FRAME {
                break;
            }
            // Feed the far-end reference first (what was played), then run
            // the capture chain on the near-end mic frame in place.
            let _ = self.processor.analyze_render_frame([ref_frame]);
            let _ = self.processor.process_capture_frame([mic_frame]);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Offline measurement on a synthetic speakerphone scene. Run with
    //! `cargo test -p parley-core mic_processor -- --ignored --nocapture`.
    use super::*;

    const SR: usize = SAMPLE_RATE as usize;

    struct Lcg(u64);
    impl Lcg {
        /// Uniform in [-1, 1).
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u64 << 23) as f32) - 1.0
        }
    }

    /// Speech-like signal: gliding-pitch harmonic stack with a ~4 Hz syllable
    /// envelope and a little aspiration noise, RMS-normalised to `rms`.
    fn speech(seconds: usize, rms: f32, seed: u64) -> Vec<f32> {
        let mut rng = Lcg(seed);
        let n = seconds * SR;
        let mut phase = 0.0f32;
        let mut f0 = 120.0f32;
        let mut f0_target = 140.0f32;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            if i % (SR / 8) == 0 {
                f0_target = 90.0 + 110.0 * (rng.next() * 0.5 + 0.5);
            }
            f0 += (f0_target - f0) * 0.0004;
            phase += 2.0 * std::f32::consts::PI * f0 / SR as f32;
            phase %= 2.0 * std::f32::consts::PI;
            let t = i as f32 / SR as f32;
            let env =
                (0.5 + 0.5 * (2.0 * std::f32::consts::PI * 4.0 * t + seed as f32).sin()).powf(1.5);
            let mut v = 0.0;
            for h in 1..=20 {
                // Crude formant tilt: energy near 500 / 1500 Hz.
                let fh = f0 * h as f32;
                let g = (-((fh - 500.0) / 400.0).powi(2)).exp()
                    + 0.6 * (-((fh - 1500.0) / 600.0).powi(2)).exp()
                    + 0.05;
                v += g * (phase * h as f32).sin() / h as f32;
            }
            out.push(env * (v + 0.05 * rng.next()));
        }
        let cur = rms_of(&out).max(1e-9);
        for s in out.iter_mut() {
            *s *= rms / cur;
        }
        out
    }

    fn rms_of(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn db(x: f32) -> f32 {
        20.0 * x.max(1e-9).log10()
    }

    fn seg(x: &[f32], from_s: f32, to_s: f32) -> &[f32] {
        &x[(from_s * SR as f32) as usize..(to_s * SR as f32) as usize]
    }

    #[test]
    #[ignore = "offline measurement; run with --ignored --nocapture"]
    fn speakerphone_chain_reduces_noise_floor_and_echo() {
        const SECS: usize = 30;
        // Timeline (s): 0-4 silence | 4-12 far-end only | 12-17 silence |
        // 17-22 near-end only | 22-30 far-end only.
        let far_active = |t: f32| (4.0..12.0).contains(&t) || (22.0..30.0).contains(&t);
        let near_active = |t: f32| (17.0..22.0).contains(&t);

        let far_full = speech(SECS, 0.1, 7); // -20 dBFS far end
        let near_full = speech(SECS, 0.1, 99);
        let mut rng = Lcg(12345);
        // White noise at -35 dBFS RMS (uniform has rms = 1/sqrt(3)).
        let noise_amp = 10f32.powf(-35.0 / 20.0) * 3f32.sqrt();

        let n = SECS * SR;
        let far: Vec<f32> = (0..n)
            .map(|i| {
                if far_active(i as f32 / SR as f32) {
                    far_full[i]
                } else {
                    0.0
                }
            })
            .collect();
        let delay = SR * 45 / 1000;
        let reflect = SR * 90 / 1000;
        let mic: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / SR as f32;
                let echo = 0.35 * if i >= delay { far[i - delay] } else { 0.0 }
                    + 0.15 * if i >= reflect { far[i - reflect] } else { 0.0 };
                let near = if near_active(t) { near_full[i] } else { 0.0 };
                noise_amp * rng.next() + echo + near
            })
            .collect();

        let mut processed = mic.clone();
        let mut proc = MicProcessor::new(SAMPLE_RATE).expect("APM init");
        for (m, r) in processed.chunks_mut(SR / 20).zip(far.chunks(SR / 20)) {
            proc.process(m, r);
        }

        // Noise floor: silent gap, after the echo tail has died.
        let gap_before = db(rms_of(seg(&mic, 13.0, 17.0)));
        let gap_after = db(rms_of(seg(&processed, 13.0, 17.0)));
        // Echo: far-end-only, after convergence (second talk burst).
        let echo_before = db(rms_of(seg(&mic, 24.0, 30.0)));
        let echo_after = db(rms_of(seg(&processed, 24.0, 30.0)));
        let erle = echo_before - echo_after;
        // Near-end speech.
        let near_before = db(rms_of(seg(&mic, 17.5, 22.0)));
        let near_after = db(rms_of(seg(&processed, 17.5, 22.0)));

        println!(
            "noise floor (silent gap): {gap_before:.1} dBFS -> {gap_after:.1} dBFS ({:.1} dB)",
            gap_after - gap_before
        );
        println!(
            "echo (far-end only, converged): {echo_before:.1} dBFS -> {echo_after:.1} dBFS, ERLE {erle:.1} dB"
        );
        println!("near-end speech: {near_before:.1} dBFS -> {near_after:.1} dBFS");
        println!(
            "near-speech-to-floor SNR: before {:.1} dB, after {:.1} dB",
            near_before - gap_before,
            near_after - gap_after
        );

        assert!(gap_before - gap_after > 6.0, "noise floor not reduced enough");
        assert!(erle > 10.0, "echo not reduced enough");
        assert!(
            near_after - gap_after > near_before - gap_before,
            "near-end SNR should improve"
        );
    }
}
