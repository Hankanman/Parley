//! Acoustic echo cancellation for the microphone (WebRTC AEC3).
//!
//! When the user listens to a meeting through speakers (not headphones), the
//! microphone picks up the remote participants coming out of those speakers.
//! That bleed makes remote speech get transcribed a second time on the mic
//! stream (a duplicate "Me") and makes mic-side playback echo. Because we
//! capture the system audio separately, we have the exact far-end reference —
//! the textbook AEC setup — so we subtract it from the mic before the mic
//! reaches VAD/transcription and the recording.
//!
//! WebRTC's audio processing module works on fixed 10 ms frames (sample_rate /
//! 100 samples per channel). The pipeline runs at 48 kHz with 50 ms windows, so
//! each window is exactly five 480-sample frames — no cross-window buffering.

use log::{info, warn};
use webrtc_audio_processing::config::EchoCanceller;
use webrtc_audio_processing::{Config, Processor};

/// Pipeline sample rate AEC is wired for. WebRTC APM accepts 8/16/32/48 kHz;
/// the capture layer resamples everything to 48 kHz before the pipeline.
const SAMPLE_RATE: u32 = 48_000;
/// 10 ms @ 48 kHz — WebRTC APM's fixed per-channel frame size.
const FRAME: usize = (SAMPLE_RATE / 100) as usize;

/// Wraps a WebRTC processor configured for mono echo cancellation.
pub struct MicEchoCanceller {
    processor: Processor,
}

impl MicEchoCanceller {
    /// Create an echo canceller for the given pipeline sample rate. Returns
    /// `None` (AEC disabled, recording still works) if the rate isn't the
    /// expected 48 kHz or the processor can't be created.
    pub fn new(sample_rate: u32) -> Option<Self> {
        if sample_rate != SAMPLE_RATE {
            warn!(
                "AEC disabled: pipeline sample rate {} Hz != {} Hz",
                sample_rate, SAMPLE_RATE
            );
            return None;
        }
        match Processor::new(sample_rate) {
            Ok(processor) => {
                // Full AEC3 with the internal delay estimator (stream delay
                // left unset), so it adapts to the mic↔speaker acoustic delay.
                processor.set_config(Config {
                    echo_canceller: Some(EchoCanceller::default()),
                    ..Default::default()
                });
                info!("✅ Microphone echo cancellation enabled (WebRTC AEC3)");
                Some(Self { processor })
            }
            Err(e) => {
                warn!("AEC init failed ({e:?}); continuing without echo cancellation");
                None
            }
        }
    }

    /// Remove `reference` (the system audio played to the speakers) from `mic`
    /// in place. Processes as many whole 10 ms frames as both slices share; any
    /// short trailing remainder is left untouched (windows are always whole
    /// frames, so this is only a defensive guard).
    pub fn cancel(&mut self, mic: &mut [f32], reference: &[f32]) {
        for (mic_frame, ref_frame) in mic.chunks_mut(FRAME).zip(reference.chunks(FRAME)) {
            if mic_frame.len() != FRAME || ref_frame.len() != FRAME {
                break;
            }
            // Feed the far-end reference first (what was played), then cancel
            // it out of the near-end mic frame in place.
            let _ = self.processor.analyze_render_frame([ref_frame]);
            let _ = self.processor.process_capture_frame([mic_frame]);
        }
    }
}
