//! Online speaker clustering.
//!
//! Greedy single-pass clusterer suitable for real-time use. Each incoming
//! embedding is compared by cosine similarity to every existing centroid;
//! if the best match is above `threshold` it joins that cluster and updates
//! the running centroid, otherwise it seeds a new cluster.
//!
//! Embeddings are L2-normalized on insertion so cosine similarity reduces
//! to a dot product — important for hot-path performance.

use super::embedding_math::{dot, l2_normalize};

#[derive(Debug, Clone)]
struct Centroid {
    /// Stable id used to derive the user-facing label.
    id: usize,
    /// Running average of L2-normalized embeddings (re-normalized each update).
    embedding: Vec<f32>,
    /// Number of samples merged into this centroid; weights the running average.
    sample_count: u32,
}

#[derive(Debug, Clone)]
pub struct OnlineSpeakerClusterer {
    centroids: Vec<Centroid>,
    threshold: f32,
    next_id: usize,
}

impl OnlineSpeakerClusterer {
    pub fn new(threshold: f32) -> Self {
        Self {
            centroids: Vec::new(),
            threshold,
            next_id: 0,
        }
    }

    pub fn num_speakers(&self) -> usize {
        self.centroids.len()
    }

    /// Assign `embedding` to the best matching cluster, or create a new one.
    /// Returns the assigned cluster id.
    pub fn assign(&mut self, mut embedding: Vec<f32>) -> usize {
        l2_normalize(&mut embedding);

        let mut best: Option<(usize, f32)> = None;
        for (idx, c) in self.centroids.iter().enumerate() {
            let sim = dot(&c.embedding, &embedding);
            if best.map_or(true, |(_, s)| sim > s) {
                best = Some((idx, sim));
            }
        }

        if let Some((idx, sim)) = best {
            if sim >= self.threshold {
                let centroid = &mut self.centroids[idx];
                merge_into_centroid(&mut centroid.embedding, &embedding, centroid.sample_count);
                centroid.sample_count = centroid.sample_count.saturating_add(1);
                return centroid.id;
            }
        }

        let id = self.next_id;
        self.next_id += 1;
        self.centroids.push(Centroid {
            id,
            embedding,
            sample_count: 1,
        });
        id
    }

    /// User-facing label like "Speaker 1" (1-indexed).
    pub fn label_for(&self, cluster_id: usize) -> String {
        format!("Speaker {}", cluster_id + 1)
    }
}

/// Update a running centroid with a new (already-normalized) embedding.
/// Uses a sample-count-weighted mean, then re-normalizes.
fn merge_into_centroid(centroid: &mut [f32], new_emb: &[f32], prev_count: u32) {
    debug_assert_eq!(centroid.len(), new_emb.len());
    let n = prev_count as f32;
    let denom = n + 1.0;
    for (c, e) in centroid.iter_mut().zip(new_emb) {
        *c = (*c * n + *e) / denom;
    }
    l2_normalize(centroid);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec3(x: f32, y: f32, z: f32) -> Vec<f32> {
        vec![x, y, z]
    }

    #[test]
    fn similar_embeddings_merge() {
        let mut c = OnlineSpeakerClusterer::new(0.9);
        let id1 = c.assign(vec3(1.0, 0.0, 0.0));
        let id2 = c.assign(vec3(0.99, 0.01, 0.0));
        assert_eq!(id1, id2);
        assert_eq!(c.num_speakers(), 1);
    }

    #[test]
    fn dissimilar_embeddings_separate() {
        let mut c = OnlineSpeakerClusterer::new(0.5);
        let id1 = c.assign(vec3(1.0, 0.0, 0.0));
        let id2 = c.assign(vec3(0.0, 1.0, 0.0));
        assert_ne!(id1, id2);
        assert_eq!(c.num_speakers(), 2);
    }

    #[test]
    fn cluster_ids_are_stable_and_one_indexed_in_label() {
        let mut c = OnlineSpeakerClusterer::new(0.5);
        let id1 = c.assign(vec3(1.0, 0.0, 0.0));
        let id2 = c.assign(vec3(0.0, 1.0, 0.0));
        assert_eq!(c.label_for(id1), "Speaker 1");
        assert_eq!(c.label_for(id2), "Speaker 2");
    }

    #[test]
    fn empty_embedding_does_not_panic() {
        let mut c = OnlineSpeakerClusterer::new(0.5);
        // L2-normalizing a zero vector is a no-op; we accept it as a degenerate
        // first cluster rather than rejecting (caller is responsible for valid input).
        let id = c.assign(vec![0.0; 4]);
        assert_eq!(id, 0);
    }
}
