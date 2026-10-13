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
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

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
    /// Profiles the meeting's linked calendar event invited, when known.
    /// Anyone else needs [`NON_ATTENDEE_PENALTY`] more similarity to match.
    /// Set after construction (the event is usually linked once recording
    /// has started), hence the lock.
    expected: RwLock<Option<HashSet<String>>>,
}

/// How much stronger a match must be for someone the meeting's calendar
/// event didn't invite. Not a ban — people join uninvited — but the
/// default 0.60 becomes 0.75, so a merely similar voice no longer puts a
/// stranger's name on the transcript.
pub const NON_ATTENDEE_PENALTY: f32 = 0.15;

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
            expected: RwLock::new(None),
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

    /// Restrict confident matches to the profiles a meeting invited
    /// (`None` = no linked event, everyone equal). The self profile is
    /// always expected.
    pub fn set_expected_profiles(&self, ids: Option<HashSet<String>>) {
        if let Ok(mut e) = self.expected.write() {
            *e = ids;
        }
    }

    /// Whether `profile_id` belongs to someone the linked event didn't
    /// invite. Always `false` without a linked event, and for the user.
    pub fn is_unexpected(&self, profile_id: &str) -> bool {
        if self.is_self(profile_id) {
            return false;
        }
        self.expected
            .read()
            .ok()
            .and_then(|e| e.as_ref().map(|ids| !ids.contains(profile_id)))
            .unwrap_or(false)
    }

    /// The similarity `profile_id` needs to count as a match.
    pub fn threshold_for(&self, profile_id: &str) -> f32 {
        if self.is_unexpected(profile_id) {
            self.threshold + NON_ATTENDEE_PENALTY
        } else {
            self.threshold
        }
    }

    pub fn num_profiles(&self) -> usize {
        self.profiles.len()
    }

    /// Find the best matching stored profile for `embedding`, or `None` if no
    /// profile is above threshold. With a linked event, uninvited profiles
    /// compete with their score less [`NON_ATTENDEE_PENALTY`], so an invitee
    /// wins a near tie and a stranger needs a clearly better match.
    pub fn search(&self, embedding: &[f32]) -> Option<ProfileMatch> {
        let restricted = self.expected.read().ok().is_some_and(|e| e.is_some());
        if restricted {
            return self
                .embeddings
                .keys()
                .filter_map(|id| {
                    let score = self.score_against(id, embedding)?;
                    let penalty = self.threshold_for(id) - self.threshold;
                    (score - penalty >= self.threshold).then_some((id, score, score - penalty))
                })
                .max_by(|a, b| a.2.total_cmp(&b.2))
                .and_then(|(id, score, _)| {
                    Some(ProfileMatch {
                        profile_id: id.clone(),
                        name: self.profiles.get(id)?.clone(),
                        score,
                    })
                });
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Zoe on one axis, Garry on another, the user on a third.
    fn matcher() -> Option<SpeakerProfileMatcher> {
        match SpeakerProfileMatcher::new(
            4,
            vec![
                (
                    "p-zoe".to_string(),
                    "Zoe".to_string(),
                    vec![1.0, 0.0, 0.0, 0.0],
                ),
                (
                    "p-garry".to_string(),
                    "Garry".to_string(),
                    vec![0.0, 1.0, 0.0, 0.0],
                ),
                (
                    "p-me".to_string(),
                    "Me".to_string(),
                    vec![0.0, 0.0, 1.0, 0.0],
                ),
            ],
            0.60,
        ) {
            Ok(m) => Some(m.with_self_profile(Some("p-me".to_string()))),
            Err(e) => {
                eprintln!("skipping: SpeakerEmbeddingManager unavailable ({})", e);
                None
            }
        }
    }

    /// A unit vector `a` alike to Zoe and `b` alike to Garry.
    fn probe(a: f32, b: f32) -> Vec<f32> {
        vec![a, b, 0.0, (1.0 - a * a - b * b).sqrt()]
    }

    fn only_garry_invited() -> Option<HashSet<String>> {
        Some(HashSet::from(["p-garry".to_string()]))
    }

    #[test]
    fn an_uninvited_voice_needs_a_much_stronger_match() {
        let Some(m) = matcher() else { return };
        let similar = probe(0.70, 0.0);
        assert_eq!(m.search(&similar).map(|x| x.name), Some("Zoe".into()));

        m.set_expected_profiles(only_garry_invited());
        assert!(
            m.search(&similar).is_none(),
            "0.70 is not enough for an uninvited voice"
        );
        let clear = probe(0.80, 0.0);
        assert_eq!(
            m.search(&clear).map(|x| x.name),
            Some("Zoe".into()),
            "unlikely, not impossible"
        );

        m.set_expected_profiles(None);
        assert_eq!(m.search(&similar).map(|x| x.name), Some("Zoe".into()));
    }

    #[test]
    fn an_invitee_wins_a_near_tie_with_a_stranger() {
        let Some(m) = matcher() else { return };
        let between = probe(0.68, 0.63);
        assert_eq!(m.search(&between).map(|x| x.name), Some("Zoe".into()));
        m.set_expected_profiles(only_garry_invited());
        let hit = m.search(&between).unwrap();
        assert_eq!(hit.name, "Garry");
        assert!((hit.score - 0.63).abs() < 1e-4, "the raw score is reported");
    }

    #[test]
    fn the_user_is_never_treated_as_uninvited() {
        let Some(m) = matcher() else { return };
        m.set_expected_profiles(Some(HashSet::new()));
        assert!(!m.is_unexpected("p-me"));
        assert!(m.is_unexpected("p-zoe"));
        let me = vec![0.0, 0.0, 0.65, (1.0f32 - 0.65 * 0.65).sqrt()];
        assert_eq!(m.search(&me).map(|x| x.name), Some("Me".into()));
    }
}
