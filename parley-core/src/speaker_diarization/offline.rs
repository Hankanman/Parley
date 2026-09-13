//! Offline (batch) speaker diarization via sherpa-onnx.
//!
//! Unlike the online cosine clusterer (which greedily seeds a new speaker
//! whenever a snippet dips below a similarity threshold, and so over-counts),
//! this runs pyannote segmentation + speaker embeddings + global clustering
//! over the *entire* recording at once. It can be told the exact number of
//! speakers (`num_speakers`) for an exact result, or auto-estimate.
//!
//! Requires the whole audio buffer, so it only runs on Import / re-diarize,
//! never live. Input must be 16 kHz mono f32 (what the import path produces).

use std::path::Path;

use anyhow::{anyhow, Result};
use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractorConfig,
};

/// A diarized speaker turn: `[start, end)` seconds → 0-based speaker index.
#[derive(Debug, Clone, Copy)]
pub struct SpeakerTurn {
    pub start: f32,
    pub end: f32,
    pub speaker: i32,
}

/// Run offline diarization on 16 kHz mono `samples`.
///
/// `num_speakers > 0` forces exactly that many clusters; `<= 0` auto-estimates.
pub fn diarize_offline(
    samples: &[f32],
    segmentation_model: &Path,
    embedding_model: &Path,
    num_speakers: i32,
    num_threads: i32,
) -> Result<Vec<SpeakerTurn>> {
    if !segmentation_model.exists() {
        return Err(anyhow!(
            "Pyannote segmentation model missing at {}",
            segmentation_model.display()
        ));
    }
    if !embedding_model.exists() {
        return Err(anyhow!(
            "Speaker embedding model missing at {}",
            embedding_model.display()
        ));
    }

    let config = OfflineSpeakerDiarizationConfig {
        segmentation: OfflineSpeakerSegmentationModelConfig {
            pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                model: Some(segmentation_model.to_string_lossy().into_owned()),
                // Newer sherpa-onnx releases add fields here (e.g.
                // `window_shift_ratio`); defaulting the rest compiles against
                // both the locked 1.13.2 and those, with the crate's defaults.
                ..Default::default()
            },
            num_threads,
            debug: false,
            provider: Some("cpu".to_string()),
        },
        embedding: SpeakerEmbeddingExtractorConfig {
            model: Some(embedding_model.to_string_lossy().into_owned()),
            num_threads,
            debug: false,
            provider: Some("cpu".to_string()),
        },
        clustering: FastClusteringConfig {
            num_clusters: if num_speakers > 0 { num_speakers } else { -1 },
            threshold: 0.5,
            // Newer sherpa-onnx releases add fields here (e.g.
            // `compute_confidence`); defaulting the rest compiles against
            // both the locked baseline and those, with the crate's defaults.
            ..Default::default()
        },
        min_duration_on: 0.3,
        min_duration_off: 0.5,
    };

    let sd = OfflineSpeakerDiarization::create(&config)
        .ok_or_else(|| anyhow!("Failed to create OfflineSpeakerDiarization"))?;

    let result = sd
        .process(samples)
        .ok_or_else(|| anyhow!("Offline diarization returned no result"))?;

    let turns: Vec<SpeakerTurn> = result
        .sort_by_start_time()
        .into_iter()
        .map(|s| SpeakerTurn {
            start: s.start,
            end: s.end,
            speaker: s.speaker,
        })
        .collect();

    log::info!(
        "Offline diarization: {} turns across {} speaker(s) (requested={})",
        turns.len(),
        result.num_speakers(),
        num_speakers
    );

    Ok(turns)
}

/// 0-based speaker index of the turn that overlaps `[start_s, end_s]` the
/// most, or `None` if no turn overlaps at all. This is the raw index fed
/// into `Diarizer::process_with_hint`'s `forced_cluster` (see
/// `audio::common::run_batch_transcription`); [`speaker_for_range`] is a
/// thin `"Speaker N"`-formatting wrapper around it for direct-display
/// callers.
///
/// Ties (equal overlap) keep the first turn seen — `>=` on the running best
/// means a later equal-overlap turn never displaces it, which also makes the
/// result deterministic when `turns` is sorted by start time (as
/// `diarize_offline`'s output always is).
pub fn speaker_index_for_range(turns: &[SpeakerTurn], start_s: f32, end_s: f32) -> Option<usize> {
    let mut best: Option<(i32, f32)> = None;
    for t in turns {
        let overlap = (end_s.min(t.end) - start_s.max(t.start)).max(0.0);
        if overlap <= 0.0 {
            continue;
        }
        match best {
            Some((_, best_overlap)) if best_overlap >= overlap => {}
            _ => best = Some((t.speaker, overlap)),
        }
    }
    best.map(|(spk, _)| spk.max(0) as usize)
}

/// Label a transcript segment `[start_s, end_s]` with the speaker whose turn
/// overlaps it the most. Returns e.g. `"Speaker 1"`, or `None` if no overlap.
pub fn speaker_for_range(turns: &[SpeakerTurn], start_s: f32, end_s: f32) -> Option<String> {
    speaker_index_for_range(turns, start_s, end_s).map(|idx| format!("Speaker {}", idx + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(start: f32, end: f32, speaker: i32) -> SpeakerTurn {
        SpeakerTurn {
            start,
            end,
            speaker,
        }
    }

    #[test]
    fn picks_the_turn_with_most_overlap() {
        // [0,5) speaker 0, [5,10) speaker 1. A segment mostly inside the
        // second turn should be labelled speaker 1, not speaker 0 just
        // because it touches both.
        let turns = vec![turn(0.0, 5.0, 0), turn(5.0, 10.0, 1)];
        assert_eq!(
            speaker_for_range(&turns, 4.5, 9.0),
            Some("Speaker 2".to_string())
        );
        assert_eq!(speaker_index_for_range(&turns, 4.5, 9.0), Some(1));
    }

    #[test]
    fn exact_containment_picks_that_turn() {
        let turns = vec![turn(0.0, 5.0, 0), turn(5.0, 10.0, 1), turn(10.0, 15.0, 2)];
        assert_eq!(speaker_index_for_range(&turns, 6.0, 9.0), Some(1));
    }

    #[test]
    fn no_overlap_returns_none() {
        let turns = vec![turn(0.0, 5.0, 0), turn(10.0, 15.0, 1)];
        // Falls entirely in the silent gap between the two turns.
        assert_eq!(speaker_index_for_range(&turns, 6.0, 9.0), None);
        assert_eq!(speaker_for_range(&turns, 6.0, 9.0), None);
    }

    #[test]
    fn empty_turns_returns_none() {
        assert_eq!(speaker_index_for_range(&[], 0.0, 1.0), None);
    }

    #[test]
    fn tie_keeps_first_turn_seen() {
        // Equal overlap on both sides of a turn boundary that splits the
        // range exactly in half.
        let turns = vec![turn(0.0, 5.0, 0), turn(5.0, 10.0, 1)];
        assert_eq!(speaker_index_for_range(&turns, 2.5, 7.5), Some(0));
    }
}
