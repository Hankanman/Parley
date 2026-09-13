//! Shared embedding-vector math.
//!
//! Every centroid in the system — the online clusterer's running centroids,
//! enrollment centroids, promote/merge centroids, and refinement's HAC input —
//! must be built and normalized the same way, or cosine comparisons between
//! them drift. All of that math lives here so it can't diverge.

/// L2-normalize `v` in place. A (near-)zero vector is left untouched rather
/// than dividing by ~0.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        let inv = 1.0 / norm;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
}

/// Dot product — equal to cosine similarity when both inputs are L2-normalized.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Cosine distance in [0, 2] for L2-normalized inputs.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    1.0 - dot(a, b)
}

/// Average a set of embeddings and L2-normalize the result.
pub fn average_and_normalize(embeddings: &[Vec<f32>]) -> Vec<f32> {
    if embeddings.is_empty() {
        return Vec::new();
    }
    let dim = embeddings[0].len();
    let mut acc = vec![0.0f32; dim];
    for e in embeddings {
        debug_assert_eq!(e.len(), dim);
        for (i, v) in e.iter().enumerate() {
            acc[i] += v;
        }
    }
    let n = embeddings.len() as f32;
    for v in acc.iter_mut() {
        *v /= n;
    }
    l2_normalize(&mut acc);
    acc
}

/// Combine two centroid+sample-count pairs into a single L2-normalized
/// centroid representing the union of samples.
///
/// Only the precomputed centroids are available (the raw embeddings aren't
/// kept once a diarizer's history is dropped), so this is a
/// sample-count-weighted average rather than a true mean over the raw
/// vectors. For reasonably similar voices that's close enough; the
/// renormalize keeps the result on the unit sphere where cosine-similarity
/// matching expects it.
pub fn merge_centroids(a: &[f32], a_n: usize, b: &[f32], b_n: usize) -> Vec<f32> {
    debug_assert_eq!(a.len(), b.len());
    let dim = a.len();
    let total = (a_n + b_n) as f32;
    let mut acc = vec![0.0f32; dim];
    for i in 0..dim {
        acc[i] = (a[i] * a_n as f32 + b[i] * b_n as f32) / total;
    }
    l2_normalize(&mut acc);
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centroid_is_unit_length() {
        let embeddings = vec![vec![3.0f32, 0.0, 0.0], vec![0.0, 4.0, 0.0]];
        let c = average_and_normalize(&embeddings);
        let norm = c.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {}", norm);
    }

    #[test]
    fn centroid_of_identical_embeddings_is_that_embedding() {
        let e = vec![0.6f32, 0.8, 0.0]; // already unit length
        let c = average_and_normalize(&[e.clone(), e.clone(), e.clone()]);
        for (a, b) in e.iter().zip(&c) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn empty_embeddings_produce_empty_centroid() {
        assert!(average_and_normalize(&[]).is_empty());
    }

    #[test]
    fn merged_centroid_is_unit_length_and_weighted() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        // 3:1 weighting should pull the merged centroid toward `a`.
        let m = merge_centroids(&a, 3, &b, 1);
        let norm = m.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert!(m[0] > m[1]);
    }

    #[test]
    fn zero_vector_survives_normalize() {
        let mut v = vec![0.0f32; 4];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0f32; 4]);
    }
}
