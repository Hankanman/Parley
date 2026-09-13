//! Speaker embedding extractor.
//!
//! Wraps `sherpa_onnx::SpeakerEmbeddingExtractor`. The extractor consumes a
//! 16 kHz mono audio segment via an `OnlineStream` and returns a fixed-dim
//! float vector that uniquely characterizes the speaker's voice. Cosine
//! similarity between two embeddings tells us whether they're from the
//! same person.

use anyhow::{anyhow, Result};
use sherpa_onnx::{SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig};
use std::path::Path;

const TARGET_SAMPLE_RATE: i32 = 16_000;

pub struct SpeakerEmbedder {
    extractor: SpeakerEmbeddingExtractor,
    dim: usize,
}

impl SpeakerEmbedder {
    /// Load the ONNX speaker-embedding model at `model_path`. Uses CPU
    /// inference (the model is tiny — ~28MB CAM++ runs in <50ms on a
    /// modern laptop CPU).
    pub fn from_path(model_path: &Path, num_threads: i32) -> Result<Self> {
        if !model_path.exists() {
            return Err(anyhow!(
                "Speaker embedding model not found at {}",
                model_path.display()
            ));
        }

        let config = SpeakerEmbeddingExtractorConfig {
            model: Some(model_path.to_string_lossy().into_owned()),
            num_threads,
            debug: false,
            provider: Some("cpu".to_string()),
        };

        let extractor = SpeakerEmbeddingExtractor::create(&config)
            .ok_or_else(|| anyhow!("Failed to create SpeakerEmbeddingExtractor"))?;
        let dim = extractor.dim() as usize;

        log::info!(
            "Speaker embedder loaded: model={}, dim={}",
            model_path.display(),
            dim
        );

        Ok(Self { extractor, dim })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Extract a single embedding from a 16 kHz mono speech segment.
    /// Caller is responsible for ensuring the input contains speech (we
    /// already filter via VAD upstream).
    pub fn embed(&self, samples_16k: &[f32]) -> Result<Vec<f32>> {
        if samples_16k.is_empty() {
            return Err(anyhow!("Cannot embed empty audio"));
        }

        let stream = self
            .extractor
            .create_stream()
            .ok_or_else(|| anyhow!("Failed to create speaker-embedding stream"))?;

        stream.accept_waveform(TARGET_SAMPLE_RATE, samples_16k);
        stream.input_finished();

        if !self.extractor.is_ready(&stream) {
            // Sherpa needs a minimum amount of audio to produce a meaningful
            // embedding; for very short segments we surface this as an error
            // so the caller can fall back to the cluster placeholder.
            return Err(anyhow!(
                "Audio segment too short for speaker embedding ({} samples)",
                samples_16k.len()
            ));
        }

        self.extractor
            .compute(&stream)
            .ok_or_else(|| anyhow!("Speaker embedding computation returned null"))
    }
}
