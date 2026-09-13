//! Speaker diarization layer.
//!
//! Sits behind the dual-VAD audio pipeline (see `audio::pipeline`): when a
//! speech segment is about to be transcribed, we extract a 512-dim CAM++
//! speaker embedding and assign it to a cluster (online, real-time) so the
//! transcription gets tagged with "Speaker 1", "Speaker 2", etc.
//!
//! Both mic AND system segments run through this layer when a diarizer is
//! loaded — in a speakers-in-a-room setup the mic captures every
//! participant, so clustering the mic stream separates those voices instead
//! of lumping them under the local user. The "Me" placeholder is used only
//! when no diarizer is loaded (speaker model not downloaded).
//!
//! ## How the local user gets labelled "Me" with a diarizer loaded
//!
//! Because mic segments are clustered rather than assumed to be the user, the
//! user's own voice would otherwise surface as "Speaker N". The fix is
//! enrollment (see [`enrollment`]): the user records a baseline of their own
//! voice in settings, and it's stored as an ordinary voice profile — flagged
//! `is_self` and named [`SELF_SPEAKER_LABEL`]. Nothing in the diarizer
//! special-cases it: [`SpeakerProfileMatcher`] recognizes it like any other
//! stored speaker, and [`Diarizer::process`] renders the matched profile's
//! name, which for that row is "Me".
//!
//! That's why the label lives in the profile's *name* rather than being
//! derived from the flag at match time: it needs no branch on any hot path,
//! self-attribution works on mic and system sources alike (a user dialled into
//! their own meeting still matches), and the flag stays what it should be —
//! the stable identity enrollment uses to find and replace the profile,
//! independent of what the row is called.
//!
//! ## Lifecycle
//! - At app startup the model file is checked but not loaded (lazy).
//! - When recording starts and the model file is present, a [`Diarizer`] is
//!   built and stored in [`DIARIZER`]. It lives for the duration of the
//!   recording session and is dropped on stop, resetting cluster IDs.
//! - The transcription worker reads [`DIARIZER`] via [`current_diarizer`] and
//!   calls [`Diarizer::process`] for each chunk.
//! - When recording stops, [`Diarizer::refine`] re-clusters the whole
//!   session offline and [`Diarizer::apply_refinement`] writes the improved
//!   labels back into the history, keeping it consistent with what the DB
//!   and UI show (promote/merge look embeddings up *by label*).

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) mod clusterer;
mod embedder;
pub(crate) mod embedding_math;
pub mod enrollment;
pub mod model;
pub mod offline;
mod profile_matcher;
mod refinement;

pub mod service;

pub use clusterer::OnlineSpeakerClusterer;
pub use embedder::SpeakerEmbedder;
pub use model::{default_model_path, model_download_url, model_filename, models_dir};
pub use profile_matcher::{ProfileMatch, SpeakerProfileMatcher};
pub use refinement::RefinedAssignment;

use anyhow::Result;

// ──────────────────────────────────────────────────────────────────────────
// Similarity thresholds
//
// All three compare CAM++ embeddings by cosine similarity, and their
// ordering is load-bearing:
//
//   TURN_SPLIT_SIMILARITY (0.50) < DEFAULT_CLUSTER_THRESHOLD (0.55)
//                                < PROFILE_MATCH_THRESHOLD (0.60)
//
// - Turn splitting compares single ~1s windows — the noisiest embeddings in
//   the system — so it uses the most forgiving cutoff: only a clear drop in
//   similarity between adjacent windows is treated as a speaker change.
// - In-session clustering compares a full-segment embedding against running
//   centroids. A wrong merge here shows two people under one "Speaker N";
//   a wrong split just fragments one person across two labels (annoying but
//   repairable by refinement).
// - Profile matching is the strictest because its false positives are the
//   worst outcome: transcripts get labelled with the *wrong person's name*.
// ──────────────────────────────────────────────────────────────────────────

/// Cosine-similarity threshold above which an incoming embedding is merged
/// into an existing in-session cluster. Tuned for 3D-Speaker CAM++; lower
/// risks merging different speakers, higher risks fragmenting one speaker
/// into multiple "Speaker N" labels.
pub const DEFAULT_CLUSTER_THRESHOLD: f32 = 0.55;

/// Cosine-similarity threshold for "this is John" against a stored voice
/// profile. Stricter than clustering because a false positive mislabels
/// transcripts with someone else's name.
pub const PROFILE_MATCH_THRESHOLD: f32 = 0.60;

/// Cosine similarity *below* which two adjacent ~1s analysis windows inside
/// one VAD segment are treated as a speaker change (see
/// [`Diarizer::speaker_turns`]).
pub const TURN_SPLIT_SIMILARITY: f32 = 0.50;

/// Merge cutoff for the post-recording offline re-clustering (HAC in
/// [`refinement`]). Kept equal to [`DEFAULT_CLUSTER_THRESHOLD`] today, but
/// named separately because the regimes differ — average-linkage over the
/// full session vs greedy matching against a running centroid — and may
/// deserve independent tuning.
pub const REFINE_CLUSTER_THRESHOLD: f32 = DEFAULT_CLUSTER_THRESHOLD;

/// Default display label for the local user's own voice. Used as the initial
/// `name` on their enrolled voice profile (`is_self = 1`) and as the fallback
/// when they clear the custom label — enrollment lets the user rename it to
/// e.g. their own name (see `enrollment::self_label_or_default`). Also the
/// no-diarizer mic placeholder in
/// `audio::transcription::worker::default_speaker_for_source`; that placeholder
/// only applies with no speaker model loaded, which is exactly when no
/// enrollment (and so no custom label) can exist, so the two never disagree.
pub const SELF_SPEAKER_LABEL: &str = "Me";

/// Result of diarizing a single speech segment.
#[derive(Debug, Clone)]
pub struct DiarizationResult {
    /// Display label: a stored profile name when matched, else "Speaker N".
    /// The user's enrolled profile is named [`SELF_SPEAKER_LABEL`], so their
    /// own voice comes back as "Me" through the ordinary match path.
    pub label: String,
    /// Stored voice profile id when this segment matched a known speaker;
    /// `None` for in-session-only clusters.
    pub voice_profile_id: Option<String>,
}

/// One speaker-turn sub-range `[start, end)` (in samples) of a VAD segment,
/// as detected by [`Diarizer::speaker_turns`]. Carries the L2-normalized
/// average of the range's analysis-window embeddings when at least one
/// window embedded successfully, so callers can diarize the sub-range
/// without re-running the embedder over the same audio.
#[derive(Debug, Clone)]
pub struct SpeakerTurn {
    pub start: usize,
    pub end: usize,
    pub embedding: Option<Vec<f32>>,
}

/// Per-recording diarizer state. Holds the (shared, immutable) embedder, a
/// matcher that consults stored voice profiles, an in-session clusterer for
/// fallback labels, and a full embedding history used by 2-pass refinement
/// at recording stop.
pub struct Diarizer {
    embedder: Arc<SpeakerEmbedder>,
    profile_matcher: Option<Arc<SpeakerProfileMatcher>>,
    clusterer: Mutex<OnlineSpeakerClusterer>,
    history: Mutex<Vec<EmbeddingRecord>>,
}

/// One diarized segment in the session history. `label`/`voice_profile_id`
/// track what the segment is *currently* called: they start as the live
/// pass's assignment and are rewritten by [`Diarizer::apply_refinement`]
/// when the offline pass relabels the session, so lookups by label
/// ([`Diarizer::embeddings_for_label`]) always agree with what the DB and
/// UI show.
#[derive(Debug, Clone)]
pub struct EmbeddingRecord {
    pub sequence_id: u64,
    /// L2-normalized embedding.
    pub embedding: Vec<f32>,
    pub voice_profile_id: Option<String>,
    pub label: String,
}

impl Diarizer {
    pub fn new(
        embedder: Arc<SpeakerEmbedder>,
        threshold: f32,
        profile_matcher: Option<Arc<SpeakerProfileMatcher>>,
    ) -> Self {
        Self {
            embedder,
            profile_matcher,
            clusterer: Mutex::new(OnlineSpeakerClusterer::new(threshold)),
            history: Mutex::new(Vec::new()),
        }
    }

    /// Diarize one speech segment: try to match a stored profile, fall back
    /// to in-session clustering. Always records to history so refinement and
    /// "promote to profile" can replay it.
    ///
    /// `precomputed` short-circuits the embedder — [`speaker_turns`] already
    /// embedded the segment's analysis windows, and the normalized average of
    /// those is passed back in here so the same audio isn't embedded twice.
    /// When `None` (segment too short for turn analysis, or a batch caller),
    /// `samples_16k` is embedded directly.
    ///
    /// [`speaker_turns`]: Diarizer::speaker_turns
    pub fn process(
        &self,
        sequence_id: u64,
        samples_16k: &[f32],
        precomputed: Option<Vec<f32>>,
    ) -> Result<DiarizationResult> {
        self.process_inner(sequence_id, samples_16k, precomputed, None)
    }

    /// Like [`process`], but lets a caller who has already run *offline*
    /// (whole-file) diarization — see `speaker_diarization::offline` — steer
    /// the "Speaker N" fallback label via `forced_cluster` (a 0-based
    /// speaker index from that pass).
    ///
    /// The embedding is still computed (or taken from `precomputed`) and
    /// recorded to history exactly as [`process`] does, and — critically —
    /// a stored voice profile match still takes precedence: an enrolled
    /// voice comes out as "John" whether or not a hint was supplied. Only
    /// the *unmatched* fallback label changes: with a hint it's
    /// `format!("Speaker {}", forced_cluster + 1)` instead of the online
    /// clusterer's own greedy assignment. The online clusterer still sees
    /// every embedding regardless (its running centroids/ids stay usable
    /// for any segment that arrives without a hint), it just doesn't get to
    /// decide the label when one is present.
    ///
    /// This is how import's whole-file offline diarization (far more
    /// accurate than per-segment online clustering) gets used *without*
    /// bypassing this diarizer: the caller keeps one diarizer instance with
    /// a complete history, so promote/rename and [`refine`] work exactly as
    /// they do for a live recording.
    ///
    /// [`process`]: Diarizer::process
    /// [`refine`]: Diarizer::refine
    pub fn process_with_hint(
        &self,
        sequence_id: u64,
        samples_16k: &[f32],
        precomputed: Option<Vec<f32>>,
        forced_cluster: Option<usize>,
    ) -> Result<DiarizationResult> {
        self.process_inner(sequence_id, samples_16k, precomputed, forced_cluster)
    }

    fn process_inner(
        &self,
        sequence_id: u64,
        samples_16k: &[f32],
        precomputed: Option<Vec<f32>>,
        forced_cluster: Option<usize>,
    ) -> Result<DiarizationResult> {
        let mut embedding = match precomputed {
            Some(e) => e,
            None => self.embedder.embed(samples_16k)?,
        };
        // Normalize once here so every consumer — clusterer centroids,
        // profile matching, history averages — sees unit-length vectors and
        // no path is magnitude-weighted differently from another.
        embedding_math::l2_normalize(&mut embedding);

        // Always run the cluster step, even with a hint present — its
        // running centroids/ids need every embedding to stay useful for any
        // segment that arrives without one (see `process_with_hint` doc).
        let online_cluster_label = {
            let mut clusterer = self
                .clusterer
                .lock()
                .map_err(|_| anyhow::anyhow!("speaker clusterer mutex poisoned"))?;
            let cluster_id = clusterer.assign(embedding.clone());
            clusterer.label_for(cluster_id)
        };

        // A stored-profile match takes precedence over both the cluster
        // label and the hint. This is also the self-attribution path: the
        // enrolled self profile is just another entry in the matcher, named
        // "Me".
        let profile_match = self
            .profile_matcher
            .as_ref()
            .and_then(|m| m.search(&embedding));

        let (label, voice_profile_id) =
            resolve_label(online_cluster_label, forced_cluster, profile_match);

        if let Ok(mut h) = self.history.lock() {
            h.push(EmbeddingRecord {
                sequence_id,
                embedding,
                voice_profile_id: voice_profile_id.clone(),
                label: label.clone(),
            });
        }

        Ok(DiarizationResult {
            label,
            voice_profile_id,
        })
    }

    /// Detect speaker-turn boundaries within a single VAD segment and return
    /// the contiguous sub-ranges that each belong to one speaker. Returns a
    /// single whole-segment range when the audio is too short to split
    /// reliably or holds one speaker throughout.
    ///
    /// Works by embedding consecutive ~1s windows and cutting where adjacent
    /// voice embeddings diverge — this catches speakers who talk back-to-back
    /// with no silence gap, which VAD (silence-based) can't separate. The
    /// caller transcribes and labels each returned range independently,
    /// passing the range's precomputed embedding back into [`process`].
    ///
    /// [`process`]: Diarizer::process
    pub fn speaker_turns(&self, samples_16k: &[f32]) -> Vec<SpeakerTurn> {
        /// 1s analysis window at 16 kHz — CAM++'s reliable minimum.
        const WIN: usize = 16_000;
        /// Don't attempt a split below 2s (can't hold two speakers reliably).
        const MIN_TOTAL: usize = 2 * WIN;

        let n = samples_16k.len();
        if n < MIN_TOTAL {
            return vec![SpeakerTurn {
                start: 0,
                end: n,
                embedding: None,
            }];
        }

        let bounds = window_bounds(n, WIN);
        if bounds.len() < 2 {
            return vec![SpeakerTurn {
                start: 0,
                end: n,
                embedding: None,
            }];
        }

        // Embedding is stateless per call; safe to run here. Windows that are
        // too short/quiet to embed come back None and never force a cut.
        let embeddings: Vec<Option<Vec<f32>>> = bounds
            .iter()
            .map(|&(a, b)| {
                self.embedder.embed(&samples_16k[a..b]).ok().map(|mut e| {
                    embedding_math::l2_normalize(&mut e);
                    e
                })
            })
            .collect();

        turns_from_embeddings(&bounds, &embeddings, TURN_SPLIT_SIMILARITY)
    }

    /// Snapshot of all embeddings produced this session.
    pub fn export_history(&self) -> Vec<EmbeddingRecord> {
        self.history
            .lock()
            .map(|h| h.clone())
            .unwrap_or_default()
    }

    /// Embeddings of every history record whose *current* label is `label`.
    ///
    /// Used by promote/merge when the user names a "Speaker N" chip. Lookup
    /// is by label — not by clusterer id — because post-recording refinement
    /// renumbers labels independently of the live clusterer's ids; the label
    /// the user clicked is the only identifier guaranteed to mean the same
    /// thing in the UI, the DB, and (after [`apply_refinement`]) here.
    ///
    /// [`apply_refinement`]: Diarizer::apply_refinement
    pub fn embeddings_for_label(&self, label: &str) -> Vec<Vec<f32>> {
        self.history
            .lock()
            .map(|h| {
                h.iter()
                    .filter(|r| r.label == label)
                    .map(|r| r.embedding.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Re-cluster the whole session offline (see [`refinement`]), consulting
    /// stored voice profiles so near-threshold segments of an enrolled voice
    /// fold into the profile instead of surfacing as a parallel "Speaker N".
    pub fn refine(&self) -> Vec<RefinedAssignment> {
        let history = self.export_history();
        refinement::refine(
            &history,
            REFINE_CLUSTER_THRESHOLD,
            self.profile_matcher.as_deref(),
        )
    }

    /// Write refined labels back into the session history so subsequent
    /// label-based lookups ([`embeddings_for_label`]) agree with the labels
    /// the refinement pass persisted to the DB.
    ///
    /// [`embeddings_for_label`]: Diarizer::embeddings_for_label
    pub fn apply_refinement(&self, assignments: &[RefinedAssignment]) {
        let by_sequence: HashMap<u64, &RefinedAssignment> =
            assignments.iter().map(|a| (a.sequence_id, a)).collect();
        if let Ok(mut history) = self.history.lock() {
            for record in history.iter_mut() {
                if let Some(a) = by_sequence.get(&record.sequence_id) {
                    record.label = a.speaker.clone();
                    record.voice_profile_id = a.voice_profile_id.clone();
                }
            }
        }
    }
}

/// Decide the final label + voice-profile id for one segment, given the
/// online clusterer's own fallback label, an optional offline-diarization
/// hint that overrides it, and an optional stored-profile match that
/// overrides both. Pure and independent of any live embedder/model, so it's
/// unit-testable on its own (see `hint_tests` below) — this is the whole
/// decision `process_with_hint`/`process` make once they have their inputs.
fn resolve_label(
    online_cluster_label: String,
    forced_cluster: Option<usize>,
    profile_match: Option<ProfileMatch>,
) -> (String, Option<String>) {
    let cluster_label = match forced_cluster {
        Some(idx) => format!("Speaker {}", idx + 1),
        None => online_cluster_label,
    };
    match profile_match {
        Some(m) => (m.name, Some(m.profile_id)),
        None => (cluster_label, None),
    }
}

/// Consecutive analysis-window `[start, end)` bounds over `n` samples. A short
/// trailing remainder (< half a window) is folded into the previous window so
/// we never embed a tiny, unreliable stub.
fn window_bounds(n: usize, win: usize) -> Vec<(usize, usize)> {
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut s = 0;
    while s < n {
        let e = (s + win).min(n);
        if e - s < win / 2 {
            match bounds.last_mut() {
                Some(last) => last.1 = n,
                None => bounds.push((s, n)),
            }
            break;
        }
        bounds.push((s, e));
        s = e;
    }
    bounds
}

/// Group windows into speaker-turn ranges, cutting where adjacent (already
/// L2-normalized) embeddings fall below `min_similarity`. Windows that failed
/// to embed (`None`) never introduce a cut. The result always covers
/// `[bounds[0].0, bounds.last().1)` contiguously with no gaps. Each turn
/// carries the normalized average of its successfully-embedded windows.
fn turns_from_embeddings(
    bounds: &[(usize, usize)],
    embeddings: &[Option<Vec<f32>>],
    min_similarity: f32,
) -> Vec<SpeakerTurn> {
    if bounds.is_empty() {
        return Vec::new();
    }
    let make_turn = |from: usize, to: usize| {
        let windows: Vec<Vec<f32>> = embeddings[from..=to]
            .iter()
            .filter_map(|e| e.clone())
            .collect();
        SpeakerTurn {
            start: bounds[from].0,
            end: bounds[to].1,
            embedding: if windows.is_empty() {
                None
            } else {
                Some(embedding_math::average_and_normalize(&windows))
            },
        }
    };

    let mut turns = Vec::new();
    let mut start = 0usize;
    for i in 1..bounds.len() {
        let changed = match (&embeddings[i - 1], &embeddings[i]) {
            (Some(a), Some(b)) => embedding_math::dot(a, b) < min_similarity,
            _ => false, // a window we couldn't embed can't establish a boundary
        };
        if changed {
            turns.push(make_turn(start, i - 1));
            start = i;
        }
    }
    turns.push(make_turn(start, bounds.len() - 1));
    turns
}

#[cfg(test)]
mod turn_tests {
    use super::*;

    fn ranges(turns: &[SpeakerTurn]) -> Vec<(usize, usize)> {
        turns.iter().map(|t| (t.start, t.end)).collect()
    }

    #[test]
    fn window_bounds_folds_short_trailer() {
        // 2.4s @ 16k: two full 1s windows, 0.4s trailer folded into the second.
        let b = window_bounds(38_400, 16_000);
        assert_eq!(b, vec![(0, 16_000), (16_000, 38_400)]);
    }

    #[test]
    fn window_bounds_exact_multiple() {
        let b = window_bounds(32_000, 16_000);
        assert_eq!(b, vec![(0, 16_000), (16_000, 32_000)]);
    }

    #[test]
    fn single_speaker_stays_one_turn() {
        let bounds = vec![(0, 16_000), (16_000, 32_000), (32_000, 48_000)];
        let a = Some(vec![1.0, 0.0]);
        let embs = vec![a.clone(), a.clone(), a];
        let turns = turns_from_embeddings(&bounds, &embs, 0.5);
        assert_eq!(ranges(&turns), vec![(0, 48_000)]);
        // The turn carries the average of its window embeddings.
        assert_eq!(turns[0].embedding.as_deref(), Some(&[1.0, 0.0][..]));
    }

    #[test]
    fn speaker_change_splits_and_covers_contiguously() {
        let bounds = vec![(0, 16_000), (16_000, 32_000), (32_000, 48_000)];
        // window 0,1 = speaker A; window 2 = speaker B (orthogonal => sim 0).
        let embs = vec![
            Some(vec![1.0, 0.0]),
            Some(vec![1.0, 0.0]),
            Some(vec![0.0, 1.0]),
        ];
        let turns = turns_from_embeddings(&bounds, &embs, 0.5);
        assert_eq!(ranges(&turns), vec![(0, 32_000), (32_000, 48_000)]);
        assert_eq!(turns[0].embedding.as_deref(), Some(&[1.0, 0.0][..]));
        assert_eq!(turns[1].embedding.as_deref(), Some(&[0.0, 1.0][..]));
    }

    #[test]
    fn failed_embedding_does_not_cut() {
        let bounds = vec![(0, 16_000), (16_000, 32_000)];
        let embs = vec![Some(vec![1.0, 0.0]), None];
        let turns = turns_from_embeddings(&bounds, &embs, 0.5);
        assert_eq!(ranges(&turns), vec![(0, 32_000)]);
        // Average over the one window that did embed.
        assert_eq!(turns[0].embedding.as_deref(), Some(&[1.0, 0.0][..]));
    }

    #[test]
    fn all_windows_failed_yields_no_embedding() {
        let bounds = vec![(0, 16_000), (16_000, 32_000)];
        let embs: Vec<Option<Vec<f32>>> = vec![None, None];
        let turns = turns_from_embeddings(&bounds, &embs, 0.5);
        assert_eq!(ranges(&turns), vec![(0, 32_000)]);
        assert!(turns[0].embedding.is_none());
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;

    fn pm(profile_id: &str, name: &str) -> ProfileMatch {
        ProfileMatch {
            profile_id: profile_id.to_string(),
            name: name.to_string(),
            score: 0.9,
        }
    }

    #[test]
    fn hinted_cluster_used_when_no_profile_match() {
        let (label, voice_profile_id) = resolve_label("Speaker 7".to_string(), Some(2), None);
        // forced_cluster is 0-based; the label is 1-based.
        assert_eq!(label, "Speaker 3");
        assert!(voice_profile_id.is_none());
    }

    #[test]
    fn profile_match_wins_over_hint() {
        let (label, voice_profile_id) =
            resolve_label("Speaker 7".to_string(), Some(2), Some(pm("p1", "John")));
        assert_eq!(label, "John");
        assert_eq!(voice_profile_id.as_deref(), Some("p1"));
    }

    #[test]
    fn profile_match_wins_with_no_hint_too() {
        let (label, voice_profile_id) =
            resolve_label("Speaker 7".to_string(), None, Some(pm("p1", "John")));
        assert_eq!(label, "John");
        assert_eq!(voice_profile_id.as_deref(), Some("p1"));
    }

    #[test]
    fn no_hint_no_match_falls_back_to_online_cluster_label() {
        let (label, voice_profile_id) = resolve_label("Speaker 7".to_string(), None, None);
        assert_eq!(label, "Speaker 7");
        assert!(voice_profile_id.is_none());
    }
}

/// Process-wide diarizer slot. Set by `start_recording` (when the model is
/// ready), cleared by `stop_recording`. The transcription worker reads this
/// to decide whether chunks get clustered or fall back to the source
/// placeholder labels.
static DIARIZER: Lazy<Mutex<Option<Arc<Diarizer>>>> = Lazy::new(|| Mutex::new(None));

pub fn set_current_diarizer(diarizer: Option<Arc<Diarizer>>) {
    if let Ok(mut slot) = DIARIZER.lock() {
        *slot = diarizer;
    }
}

pub fn current_diarizer() -> Option<Arc<Diarizer>> {
    DIARIZER.lock().ok().and_then(|s| s.clone())
}
