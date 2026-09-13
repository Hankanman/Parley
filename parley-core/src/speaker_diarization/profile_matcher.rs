//! Match a live embedding against stored voice profiles.
//!
//! Wraps `sherpa_onnx::SpeakerEmbeddingManager`. The manager keeps an
//! in-memory index keyed by **profile id** (not name), so renames don't
//! invalidate the index. We map the matched id back to the profile id used
//! by the rest of the app via the populated `id_to_name` table.
//!
//! Loaded once per session (live recording or batch job) with all profiles
//! whose embedding dim matches the active model. Profiles with mismatched
//! dims are skipped (logged) so a model upgrade doesn't crash the matcher.

use anyhow::Result;
use sherpa_onnx::SpeakerEmbeddingManager;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ProfileMatch {
    pub profile_id: String,
    pub name: String,
    pub score: f32,
}

pub struct SpeakerProfileMatcher {
    manager: SpeakerEmbeddingManager,
    /// Maps the profile id stored in the underlying manager back to the
    /// (profile_id, display_name) pair we expose to callers.
    profiles: HashMap<String, String>, // id -> name
    threshold: f32,
}

impl SpeakerProfileMatcher {
    /// Build a matcher that searches for embeddings of the given dimension.
    /// `profiles`: iterable of `(id, name, embedding)` from the DB. Embeddings
    /// with mismatched dim are skipped.
    pub fn new<I>(dim: usize, profiles: I, threshold: f32) -> Result<Self>
    where
        I: IntoIterator<Item = (String, String, Vec<f32>)>,
    {
        let manager = SpeakerEmbeddingManager::create(dim as i32)
            .ok_or_else(|| anyhow::anyhow!("Failed to create SpeakerEmbeddingManager"))?;

        let mut id_to_name = HashMap::new();
        let mut loaded = 0usize;
        let mut skipped = 0usize;
        for (id, name, embedding) in profiles {
            if embedding.len() != dim {
                log::warn!(
                    "Skipping voice profile '{}' (id={}): embedding dim {} != active model dim {}",
                    name,
                    id,
                    embedding.len(),
                    dim
                );
                skipped += 1;
                continue;
            }
            // Use the profile id (not the name) as the key in the manager so
            // duplicate display names don't collide and renames don't require
            // a rebuild.
            if manager.add(&id, &embedding) {
                id_to_name.insert(id, name);
                loaded += 1;
            } else {
                log::warn!("SpeakerEmbeddingManager::add failed for '{}'", name);
            }
        }
        log::info!(
            "Profile matcher loaded {} profiles ({} skipped, dim={}, threshold={})",
            loaded,
            skipped,
            dim,
            threshold
        );

        Ok(Self {
            manager,
            profiles: id_to_name,
            threshold,
        })
    }

    pub fn num_profiles(&self) -> usize {
        self.profiles.len()
    }

    /// Find the best matching stored profile for `embedding`, or `None` if no
    /// profile is above threshold.
    pub fn search(&self, embedding: &[f32]) -> Option<ProfileMatch> {
        // `get_best_matches` returns up to `n` matches above threshold; we
        // only care about the top one. Score is cosine sim in [-1, 1].
        let mut matches = self
            .manager
            .get_best_matches(embedding, self.threshold, 1);
        let m = matches.pop()?;
        let name = self.profiles.get(&m.name).cloned()?;
        Some(ProfileMatch {
            profile_id: m.name, // sherpa stores our profile_id under "name"
            name,
            score: m.score,
        })
    }
}

// `SpeakerEmbeddingManager` is `Send + Sync` per its sherpa-onnx unsafe impl.
// We rely on that here so the matcher can live behind an `Arc` shared by the
// transcription worker and the diarizer.
