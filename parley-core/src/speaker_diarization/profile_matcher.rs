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
    /// L2-normalized copy of every loaded profile's centroid, so offline
    /// refinement can score a cluster against one *specific* profile (the
    /// self profile) at a threshold other than the matcher's own.
    embeddings: HashMap<String, Vec<f32>>,
    /// Id of the `is_self` profile, when one is loaded. See
    /// [`Self::with_self_profile`].
    self_profile_id: Option<String>,
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
        let mut embeddings = HashMap::new();
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
                let mut normalized = embedding;
                crate::speaker_diarization::embedding_math::l2_normalize(&mut normalized);
                embeddings.insert(id.clone(), normalized);
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
            embeddings,
            self_profile_id: None,
            threshold,
        })
    }

    /// Tell the matcher which loaded profile is the local user's own
    /// (`voice_profiles.is_self`). Ignored when that id wasn't loaded (e.g.
    /// a dim mismatch skipped it).
    pub fn with_self_profile(mut self, self_profile_id: Option<String>) -> Self {
        self.self_profile_id = self_profile_id.filter(|id| self.profiles.contains_key(id));
        self
    }

    /// Id of the loaded `is_self` profile, if any.
    pub fn self_profile_id(&self) -> Option<&str> {
        self.self_profile_id.as_deref()
    }

    /// Whether `profile_id` stands for the local user: the `is_self` profile,
    /// or another profile with the same name (trimmed, case-insensitive).
    /// The second case is the duplicate a user ends up with by naming a
    /// chip after themselves (#102); until they merge it, it must get the
    /// same "system channel needs a strong match" treatment as the real one.
    pub fn is_self(&self, profile_id: &str) -> bool {
        let Some(self_id) = self.self_profile_id.as_deref() else {
            return false;
        };
        if profile_id == self_id {
            return true;
        }
        let key = |id: &str| self.profiles.get(id).map(|n| n.trim().to_lowercase());
        key(profile_id).is_some() && key(profile_id) == key(self_id)
    }

    /// Cosine similarity of `embedding` to one specific stored profile,
    /// regardless of the match threshold. `None` if the profile isn't loaded
    /// or the dims differ.
    pub fn score_against(&self, profile_id: &str, embedding: &[f32]) -> Option<f32> {
        let stored = self.embeddings.get(profile_id)?;
        if stored.len() != embedding.len() {
            return None;
        }
        let mut probe = embedding.to_vec();
        crate::speaker_diarization::embedding_math::l2_normalize(&mut probe);
        Some(crate::speaker_diarization::embedding_math::dot(
            stored, &probe,
        ))
    }

    /// The self profile as a [`ProfileMatch`] with its score against
    /// `embedding`, regardless of threshold.
    pub fn self_match(&self, embedding: &[f32]) -> Option<ProfileMatch> {
        let id = self.self_profile_id.as_ref()?;
        let score = self.score_against(id, embedding)?;
        Some(ProfileMatch {
            profile_id: id.clone(),
            name: self.profiles.get(id)?.clone(),
            score,
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
        let mut matches = self.manager.get_best_matches(embedding, self.threshold, 1);
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
