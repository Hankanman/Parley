//! Post-recording offline refinement.
//!
//! The live path clusters greedily: each embedding is compared against the
//! centroids that exist *at that moment* and joins the best one above
//! threshold. That decision is irreversible and order-dependent — an early
//! embedding can seed a cluster that later turns out to be two people, and
//! the same voice arriving before its cluster has a stable centroid can be
//! stranded under its own "Speaker N".
//!
//! Once recording stops we have every embedding, so we re-cluster offline
//! with **average-linkage hierarchical agglomerative clustering** (HAC).
//! Every pairwise distance is known up front, so the result depends only on
//! the *set* of embeddings, not the order they arrived in. That's what makes
//! this a genuine re-grouping rather than the greedy pass re-run: HAC can
//! split a cluster the live pass wrongly merged and merge clusters it
//! wrongly split.
//!
//! Speaker count is unknown, so we cut the dendrogram on a **distance
//! threshold** rather than a fixed k: merging stops once the closest pair of
//! clusters is farther apart than `1.0 - similarity_threshold`.
//!
//! ## Profile handling
//!
//! Segments the live pass matched to a stored profile ("pinned" rows) are
//! authoritative: their label and profile id are never changed here. They do
//! participate in the clustering, though, and lend their identity outward —
//! a cluster containing pinned "John" rows labels its *unpinned* members
//! "John" too. This repairs the common live-pass artifact where half a
//! voice's segments cross the profile-match threshold and the rest sit just
//! under it, splitting one person into "John" plus a parallel "Speaker N".
//!
//! Clusters with no pinned members get a second chance against the stored
//! profiles: the cluster centroid (averaged over the whole session, so much
//! less noisy than any single segment) is searched against the matcher.
//! Only clusters that still match nothing fall back to "Speaker N",
//! numbered by first appearance so the first unrecognized voice heard is
//! "Speaker 1".

use crate::speaker_diarization::embedding_math::{average_and_normalize, cosine_distance, l2_normalize};
use crate::speaker_diarization::{EmbeddingRecord, SpeakerProfileMatcher};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize, Clone)]
pub struct RefinedAssignment {
    pub sequence_id: u64,
    pub speaker: String,
    /// Profile id after refinement. Always `Some` when the live pass matched
    /// a profile (we never unmatch); may also be newly `Some` for a segment
    /// whose cluster was folded into a stored profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_profile_id: Option<String>,
    /// True when this segment's label or profile link changed vs the live
    /// pass — the frontend can use this to flash-update only the affected
    /// rows, and the DB writer uses it to skip no-op updates.
    pub changed: bool,
    /// The label the live pass assigned. The DB update matches on this so a
    /// row the user renamed underneath us is never clobbered.
    #[serde(skip)]
    pub previous_speaker: String,
}

/// Re-cluster `history` offline and return one assignment per record.
///
/// `threshold` is a cosine **similarity** threshold (see
/// `REFINE_CLUSTER_THRESHOLD`); internally it becomes the cosine-distance
/// merge cutoff `1.0 - threshold`. `matcher` provides the stored voice
/// profiles used to name clusters (see module docs); `None` disables the
/// profile steps and everything unpinned falls back to "Speaker N".
///
/// Output order matches `history` order.
pub fn refine(
    history: &[EmbeddingRecord],
    threshold: f32,
    matcher: Option<&SpeakerProfileMatcher>,
) -> Vec<RefinedAssignment> {
    if history.is_empty() {
        return Vec::new();
    }

    // Cluster EVERY record — pinned rows included, so their cluster mates
    // can inherit the profile (pinned rows themselves never change).
    let embeddings: Vec<Vec<f32>> = history.iter().map(|r| r.embedding.clone()).collect();
    let cluster_of = agglomerative_cluster(&embeddings, 1.0 - threshold);

    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &c) in cluster_of.iter().enumerate() {
        members.entry(c).or_default().push(i);
    }

    // Resolve each cluster's identity in history (first-appearance) order so
    // the "Speaker N" numbering is stable and human-friendly: the first
    // unrecognized voice to speak is "Speaker 1". Raw HAC cluster indices
    // carry no meaningful order.
    let mut identity_of: HashMap<usize, (String, Option<String>)> = HashMap::new();
    let mut next_label = 1usize;
    for &cluster in &cluster_of {
        if identity_of.contains_key(&cluster) {
            continue;
        }
        let identity = cluster_identity(&members[&cluster], history, matcher)
            .unwrap_or_else(|| {
                let label = format!("Speaker {}", next_label);
                next_label += 1;
                (label, None)
            });
        identity_of.insert(cluster, identity);
    }

    history
        .iter()
        .enumerate()
        .map(|(i, record)| {
            if let Some(profile_id) = &record.voice_profile_id {
                // Pinned: a live profile match is authoritative.
                return RefinedAssignment {
                    sequence_id: record.sequence_id,
                    speaker: record.label.clone(),
                    voice_profile_id: Some(profile_id.clone()),
                    changed: false,
                    previous_speaker: record.label.clone(),
                };
            }
            let (label, profile_id) = identity_of[&cluster_of[i]].clone();
            RefinedAssignment {
                sequence_id: record.sequence_id,
                speaker: label.clone(),
                changed: label != record.label || profile_id.is_some(),
                voice_profile_id: profile_id,
                previous_speaker: record.label.clone(),
            }
        })
        .collect()
}

/// A cluster's (label, profile id) when it can be tied to a stored profile:
/// the dominant pinned profile among its members, else a matcher hit on the
/// cluster centroid. `None` means "unrecognized voice" (caller assigns a
/// "Speaker N").
fn cluster_identity(
    member_indices: &[usize],
    history: &[EmbeddingRecord],
    matcher: Option<&SpeakerProfileMatcher>,
) -> Option<(String, Option<String>)> {
    // Dominant pinned profile. Iterating in history order with a
    // strictly-greater comparison makes ties resolve to the profile pinned
    // first, deterministically.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut best: Option<(&EmbeddingRecord, usize)> = None;
    for &i in member_indices {
        let record = &history[i];
        if let Some(profile_id) = &record.voice_profile_id {
            let count = counts.entry(profile_id.as_str()).or_insert(0);
            *count += 1;
            if best.map_or(true, |(_, c)| *count > c) {
                best = Some((record, *count));
            }
        }
    }
    if let Some((record, _)) = best {
        return Some((record.label.clone(), record.voice_profile_id.clone()));
    }

    // No pinned members — try the (low-noise) cluster centroid against the
    // stored profiles.
    let matcher = matcher?;
    let member_embeddings: Vec<Vec<f32>> = member_indices
        .iter()
        .map(|&i| history[i].embedding.clone())
        .collect();
    let centroid = average_and_normalize(&member_embeddings);
    matcher
        .search(&centroid)
        .map(|m| (m.name, Some(m.profile_id)))
}

/// Average-linkage HAC over `embeddings` using cosine distance, cutting the
/// dendrogram once the nearest pair exceeds `max_distance`.
///
/// Returns a cluster index per input (indices are arbitrary but consistent;
/// the caller relabels them).
///
/// Cost: O(n²) memory for the distance matrix. Merging keeps a cached
/// nearest-neighbour per active cluster, so the usual case is ~O(n²) total
/// rather than the O(n³) a naive full rescan per merge would cost.
fn agglomerative_cluster(embeddings: &[Vec<f32>], max_distance: f32) -> Vec<usize> {
    let n = embeddings.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![0];
    }

    // Work on L2-normalized copies so cosine similarity is a plain dot product.
    let normed: Vec<Vec<f32>> = embeddings
        .iter()
        .map(|e| {
            let mut v = e.clone();
            l2_normalize(&mut v);
            v
        })
        .collect();

    // dist[i][j] — cosine distance. Only entries between *active* clusters
    // are meaningful; merged-away rows are ignored via `active`.
    let mut dist = vec![vec![0.0f32; n]; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let d = cosine_distance(&normed[i], &normed[j]);
            dist[i][j] = d;
            dist[j][i] = d;
        }
    }

    let mut active: Vec<bool> = vec![true; n];
    let mut size: Vec<f32> = vec![1.0; n];
    // Which original points live in each active cluster.
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();

    // nn[i] = (nearest active j, distance) for active i. Recomputed lazily
    // when a merge invalidates it.
    let mut nn: Vec<Option<(usize, f32)>> = (0..n)
        .map(|i| nearest_neighbour(i, &dist, &active))
        .collect();

    loop {
        // Global closest pair, via the cached per-cluster nearest neighbours.
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..n {
            if !active[i] {
                continue;
            }
            if let Some((j, d)) = nn[i] {
                if best.map_or(true, |(_, _, bd)| d < bd) {
                    best = Some((i, j, d));
                }
            }
        }

        let Some((i, j, d)) = best else { break };
        // Dendrogram cut: nothing left that's close enough to merge.
        if d >= max_distance {
            break;
        }

        // Merge j into i using the Lance-Williams update for average linkage:
        //   d(k, i∪j) = (|i|·d(k,i) + |j|·d(k,j)) / (|i| + |j|)
        let (si, sj) = (size[i], size[j]);
        let total = si + sj;
        for k in 0..n {
            if !active[k] || k == i || k == j {
                continue;
            }
            let merged = (si * dist[i][k] + sj * dist[j][k]) / total;
            dist[i][k] = merged;
            dist[k][i] = merged;
        }
        size[i] = total;
        let moved = std::mem::take(&mut members[j]);
        members[i].extend(moved);
        active[j] = false;
        nn[j] = None;

        // i's distances all changed, and anyone pointing at i or j is stale.
        for k in 0..n {
            if !active[k] {
                continue;
            }
            if k == i || matches!(nn[k], Some((t, _)) if t == i || t == j) {
                nn[k] = nearest_neighbour(k, &dist, &active);
            }
        }
    }

    let mut out = vec![0usize; n];
    let mut cluster_index = 0usize;
    for i in 0..n {
        if !active[i] {
            continue;
        }
        for &m in &members[i] {
            out[m] = cluster_index;
        }
        cluster_index += 1;
    }
    out
}

/// Nearest active cluster to `i` (excluding itself), or `None` if `i` is the
/// only active cluster left.
fn nearest_neighbour(i: usize, dist: &[Vec<f32>], active: &[bool]) -> Option<(usize, f32)> {
    if !active[i] {
        return None;
    }
    let mut best: Option<(usize, f32)> = None;
    for (j, &is_active) in active.iter().enumerate() {
        if !is_active || j == i {
            continue;
        }
        let d = dist[i][j];
        if best.map_or(true, |(_, bd)| d < bd) {
            best = Some((j, d));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker_diarization::EmbeddingRecord;

    fn rec(seq: u64, emb: Vec<f32>, label: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            sequence_id: seq,
            embedding: emb,
            voice_profile_id: None,
            label: label.to_string(),
        }
    }

    fn pinned(seq: u64, emb: Vec<f32>, label: &str, profile_id: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            sequence_id: seq,
            embedding: emb,
            voice_profile_id: Some(profile_id.to_string()),
            label: label.to_string(),
        }
    }

    /// Number of distinct speaker labels in a refinement result.
    fn distinct_speakers(out: &[RefinedAssignment]) -> usize {
        out.iter()
            .map(|a| a.speaker.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    #[test]
    fn empty_history_returns_empty() {
        let out = refine(&[], 0.55, None);
        assert!(out.is_empty());
    }

    #[test]
    fn profile_matched_rows_are_pinned() {
        let r = pinned(1, vec![1.0, 0.0], "John", "profile-1");
        let out = refine(&[r], 0.55, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        assert!(!out[0].changed);
    }

    /// The profile-extension path: an unpinned row that clusters with pinned
    /// "John" rows inherits the profile instead of surfacing as a parallel
    /// "Speaker N" — while the pinned row itself never changes.
    #[test]
    fn cluster_mates_of_a_pinned_row_inherit_its_profile() {
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            pinned(2, vec![1.0, 0.0, 0.0], "John", "profile-1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, 0.55, None);
        // The unpinned same-voice row folds into John, flagged as changed.
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        assert!(out[0].changed);
        // The pinned row is untouched.
        assert_eq!(out[1].speaker, "John");
        assert!(!out[1].changed);
        // The remaining unrecognized voice is the first unnamed cluster.
        assert_eq!(out[2].speaker, "Speaker 1");
        assert!(out[2].voice_profile_id.is_none());
    }

    /// Two pinned rows of *different* profiles keep their own labels even if
    /// HAC puts them in one cluster — pins are always authoritative.
    #[test]
    fn pinned_rows_never_change_even_when_clustered_together() {
        let history = vec![
            pinned(1, vec![1.0, 0.0, 0.0], "John", "profile-1"),
            pinned(2, vec![0.98, 0.02, 0.0], "Mary", "profile-2"),
        ];
        let out = refine(&history, 0.55, None);
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[1].speaker, "Mary");
        assert!(!out[0].changed);
        assert!(!out[1].changed);
    }

    /// Unpinned members of a mixed cluster follow the *dominant* pinned
    /// profile, with ties broken by first appearance.
    #[test]
    fn dominant_pinned_profile_wins_a_mixed_cluster() {
        let voice = vec![1.0, 0.0, 0.0];
        let history = vec![
            rec(1, voice.clone(), "Speaker 1"),
            pinned(2, voice.clone(), "Mary", "profile-2"),
            pinned(3, voice.clone(), "John", "profile-1"),
            pinned(4, voice.clone(), "John", "profile-1"),
        ];
        let out = refine(&history, 0.55, None);
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        // Pins keep their own labels regardless.
        assert_eq!(out[1].speaker, "Mary");
        assert_eq!(out[2].speaker, "John");
    }

    /// A cluster with no pinned members is searched against the stored
    /// profiles by centroid, so a voice whose individual segments all sat
    /// just under the live match threshold still gets named.
    #[test]
    fn unpinned_cluster_matches_profile_by_centroid() {
        let matcher = match SpeakerProfileMatcher::new(
            3,
            vec![(
                "profile-1".to_string(),
                "John".to_string(),
                vec![1.0, 0.0, 0.0],
            )],
            0.60,
        ) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skipping: SpeakerEmbeddingManager unavailable ({})", e);
                return;
            }
        };
        let history = vec![
            rec(1, vec![0.99, 0.01, 0.0], "Speaker 1"),
            rec(2, vec![0.98, 0.02, 0.0], "Speaker 1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, 0.55, Some(&matcher));
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        assert!(out[0].changed);
        assert_eq!(out[1].speaker, "John");
        // The orthogonal voice matches nothing and stays unnamed.
        assert_eq!(out[2].speaker, "Speaker 1");
        assert!(out[2].voice_profile_id.is_none());
    }

    #[test]
    fn two_clear_groups_produce_two_clusters() {
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(2, vec![0.98, 0.02, 0.0], "Speaker 1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
            rec(4, vec![0.02, 0.98, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, 0.55, None);
        assert_eq!(distinct_speakers(&out), 2);
        assert_eq!(out[0].speaker, out[1].speaker);
        assert_eq!(out[2].speaker, out[3].speaker);
        assert_ne!(out[0].speaker, out[2].speaker);
    }

    #[test]
    fn a_third_distinct_voice_produces_three_clusters() {
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(2, vec![0.98, 0.02, 0.0], "Speaker 1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
            rec(4, vec![0.02, 0.98, 0.0], "Speaker 2"),
            rec(5, vec![0.0, 0.0, 1.0], "Speaker 3"),
            rec(6, vec![0.0, 0.02, 0.98], "Speaker 3"),
        ];
        let out = refine(&history, 0.55, None);
        assert_eq!(distinct_speakers(&out), 3);
        assert_eq!(out[4].speaker, out[5].speaker);
        assert_ne!(out[4].speaker, out[0].speaker);
        assert_ne!(out[4].speaker, out[2].speaker);
    }

    /// The whole point of offline refinement: the grouping must depend on the
    /// *set* of embeddings, not the order they arrived in.
    #[test]
    fn clustering_is_order_independent() {
        let a1 = vec![1.0, 0.0, 0.0];
        let a2 = vec![0.97, 0.03, 0.0];
        let b1 = vec![0.0, 1.0, 0.0];
        let b2 = vec![0.03, 0.97, 0.0];

        // Same four embeddings, two different arrival orders.
        let forward = vec![
            rec(1, a1.clone(), "Speaker 1"),
            rec(2, a2.clone(), "Speaker 1"),
            rec(3, b1.clone(), "Speaker 2"),
            rec(4, b2.clone(), "Speaker 2"),
        ];
        let interleaved = vec![
            rec(1, b1.clone(), "Speaker 1"),
            rec(2, a1.clone(), "Speaker 2"),
            rec(3, b2.clone(), "Speaker 1"),
            rec(4, a2.clone(), "Speaker 2"),
        ];

        let out_f = refine(&forward, 0.55, None);
        let out_i = refine(&interleaved, 0.55, None);

        assert_eq!(distinct_speakers(&out_f), 2);
        assert_eq!(distinct_speakers(&out_i), 2);

        // In both runs the A's group together and the B's group together —
        // the *partition* is identical even though arrival order differs.
        assert_eq!(out_f[0].speaker, out_f[1].speaker); // a1 == a2
        assert_eq!(out_f[2].speaker, out_f[3].speaker); // b1 == b2
        assert_eq!(out_i[1].speaker, out_i[3].speaker); // a1 == a2
        assert_eq!(out_i[0].speaker, out_i[2].speaker); // b1 == b2
        assert_ne!(out_i[0].speaker, out_i[1].speaker); // A != B
    }

    /// A voice the live pass stranded under its own label gets folded back
    /// into the cluster it belongs to — something re-running the greedy pass
    /// in the same order could never fix.
    #[test]
    fn live_mis_split_is_repaired_and_marked_changed() {
        // All three embeddings are the same voice, but the live pass tagged
        // the middle one "Speaker 2" (warmup noise / unstable centroid).
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(2, vec![0.99, 0.01, 0.0], "Speaker 2"),
            rec(3, vec![0.98, 0.02, 0.0], "Speaker 1"),
        ];
        let out = refine(&history, 0.55, None);
        assert_eq!(distinct_speakers(&out), 1);
        assert_eq!(out[0].speaker, "Speaker 1");
        assert_eq!(out[1].speaker, "Speaker 1");
        // The repaired row is flagged, the already-correct ones are not.
        assert!(out[1].changed);
        assert!(!out[0].changed);
        assert_eq!(out[1].previous_speaker, "Speaker 2");
    }

    /// Labels are assigned by first appearance, so the first voice heard is
    /// always "Speaker 1" regardless of internal HAC cluster indices.
    #[test]
    fn labels_follow_first_appearance_order() {
        let history = vec![
            rec(1, vec![0.0, 1.0, 0.0], "Speaker 6"),
            rec(2, vec![1.0, 0.0, 0.0], "Speaker 10"),
            rec(3, vec![0.0, 0.98, 0.02], "Speaker 6"),
        ];
        let out = refine(&history, 0.55, None);
        assert_eq!(out[0].speaker, "Speaker 1");
        assert_eq!(out[1].speaker, "Speaker 2");
        assert_eq!(out[2].speaker, "Speaker 1");
    }

    #[test]
    fn single_unpinned_record_is_speaker_one() {
        let out = refine(&[rec(1, vec![1.0, 0.0, 0.0], "Speaker 4")], 0.55, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, "Speaker 1");
        assert!(out[0].changed);
    }

    #[test]
    fn every_assignment_preserves_its_sequence_id() {
        let history = vec![
            rec(41, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(42, vec![0.0, 1.0, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, 0.55, None);
        assert_eq!(out[0].sequence_id, 41);
        assert_eq!(out[1].sequence_id, 42);
    }
}
