//! Persistence for stored speaker voice profiles.
//!
//! Each profile is a (name, embedding centroid, dim, sample_count) tuple.
//! The centroid is stored as a packed little-endian f32 BLOB; the dim is
//! stored alongside so a model change can be detected at load time before
//! we feed a wrong-shaped vector to the matcher.
//!
//! One profile may carry `is_self = 1`: the local user's enrolled voice (see
//! `speaker_diarization::enrollment`). It's an ordinary profile in every
//! other respect — the matcher loads it alongside the rest, so the user's own
//! voice gets recognized in meetings like any known speaker.

use crate::database::models::VoiceProfile;
use chrono::Utc;
use sqlx::{Error as SqlxError, SqlitePool};
use uuid::Uuid;

/// Columns of `voice_profiles` in the order [`VoiceProfile`] declares them.
const PROFILE_COLUMNS: &str =
    "id, name, email, embedding, embedding_dim, sample_count, is_self, created_at, updated_at";

pub struct VoiceProfilesRepository;

impl VoiceProfilesRepository {
    pub async fn list_all(pool: &SqlitePool) -> Result<Vec<VoiceProfile>, SqlxError> {
        sqlx::query_as::<_, VoiceProfile>(&format!(
            "SELECT {PROFILE_COLUMNS} FROM voice_profiles ORDER BY name COLLATE NOCASE"
        ))
        .fetch_all(pool)
        .await
    }

    pub async fn get_by_id(
        pool: &SqlitePool,
        id: &str,
    ) -> Result<Option<VoiceProfile>, SqlxError> {
        sqlx::query_as::<_, VoiceProfile>(&format!(
            "SELECT {PROFILE_COLUMNS} FROM voice_profiles WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
    }

    /// The local user's enrolled voice profile, if they've recorded one.
    /// At most one row can satisfy this (partial unique index on `is_self`).
    pub async fn get_self(pool: &SqlitePool) -> Result<Option<VoiceProfile>, SqlxError> {
        sqlx::query_as::<_, VoiceProfile>(&format!(
            "SELECT {PROFILE_COLUMNS} FROM voice_profiles WHERE is_self = 1"
        ))
        .fetch_optional(pool)
        .await
    }

    /// Clear the self flag from every profile, leaving the rows themselves
    /// intact. Returns the number of profiles demoted (0 or 1 in practice).
    ///
    /// Used to demote-without-deleting, and internally by [`Self::upsert_self`]
    /// to guarantee the "only one self profile" invariant holds *before* the
    /// new self row is flagged — the unique index is checked per statement, so
    /// clearing first avoids a transient two-self state that would abort the
    /// transaction.
    pub async fn clear_self(pool: &SqlitePool) -> Result<u64, SqlxError> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query("UPDATE voice_profiles SET is_self = 0, updated_at = ? WHERE is_self = 1")
            .bind(&now)
            .execute(pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Create or replace the local user's self profile in one atomic step.
    ///
    /// Re-enrollment **reuses the existing row's id** rather than deleting and
    /// re-inserting, so transcripts already linked to the self profile
    /// (`transcripts.voice_profile_id`) keep pointing at it — a user
    /// re-recording their baseline shouldn't orphan their meeting history.
    ///
    /// `sample_count` is the number of embedding windows that went into
    /// `embedding` (the centroid), not a delta: enrollment always rebuilds the
    /// centroid from scratch out of the fresh recording.
    pub async fn upsert_self(
        pool: &SqlitePool,
        name: &str,
        embedding: &[f32],
        sample_count: i64,
    ) -> Result<String, SqlxError> {
        let now = Utc::now().to_rfc3339();
        let bytes = floats_to_bytes(embedding);
        let mut tx = pool.begin().await?;

        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM voice_profiles WHERE is_self = 1")
                .fetch_optional(&mut *tx)
                .await?;

        // Demote every currently-flagged row first (see `clear_self`).
        sqlx::query("UPDATE voice_profiles SET is_self = 0, updated_at = ? WHERE is_self = 1")
            .bind(&now)
            .execute(&mut *tx)
            .await?;

        let id = match existing {
            Some(id) => {
                sqlx::query(
                    "UPDATE voice_profiles
                     SET name = ?, embedding = ?, embedding_dim = ?, sample_count = ?,
                         is_self = 1, updated_at = ?
                     WHERE id = ?",
                )
                .bind(name)
                .bind(&bytes)
                .bind(embedding.len() as i64)
                .bind(sample_count)
                .bind(&now)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                let id = format!("profile-{}", Uuid::new_v4());
                sqlx::query(
                    "INSERT INTO voice_profiles
                     (id, name, email, embedding, embedding_dim, sample_count, is_self,
                      created_at, updated_at)
                     VALUES (?, ?, NULL, ?, ?, ?, 1, ?, ?)",
                )
                .bind(&id)
                .bind(name)
                .bind(&bytes)
                .bind(embedding.len() as i64)
                .bind(sample_count)
                .bind(&now)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                id
            }
        };

        tx.commit().await?;
        Ok(id)
    }

    /// Insert a new profile for *another* speaker. `email` is optional — pass
    /// `None` if the user only provided a display name. Returns the generated
    /// id.
    ///
    /// Always creates a non-self profile; the local user's own profile has its
    /// own entry point ([`Self::upsert_self`]) because it carries a
    /// replace-in-place invariant this API doesn't.
    pub async fn create(
        pool: &SqlitePool,
        name: &str,
        email: Option<&str>,
        embedding: &[f32],
        sample_count: i64,
    ) -> Result<String, SqlxError> {
        let id = format!("profile-{}", Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let bytes = floats_to_bytes(embedding);

        sqlx::query(
            "INSERT INTO voice_profiles
             (id, name, email, embedding, embedding_dim, sample_count, is_self,
              created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(email)
        .bind(&bytes)
        .bind(embedding.len() as i64)
        .bind(sample_count)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Ok(id)
    }

    /// Update a profile's display fields (name + optional email). Replaces the
    /// previous narrow `rename` API — every UI surface for editing a profile
    /// shows both fields together, so the command does too.
    ///
    /// `name` is trimmed and rejected if empty; `email` is trimmed and blank
    /// values are normalised to `NULL` — every caller (Tauri command, GPUI
    /// view) gets this for free instead of reimplementing it.
    pub async fn update_profile(
        pool: &SqlitePool,
        id: &str,
        name: &str,
        email: Option<&str>,
    ) -> Result<bool, SqlxError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(SqlxError::Protocol(
                "voice profile name cannot be empty".to_string(),
            ));
        }
        let email = email.map(str::trim).filter(|s| !s.is_empty());

        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE voice_profiles SET name = ?, email = ?, updated_at = ? WHERE id = ?",
        )
        .bind(name)
        .bind(email)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, SqlxError> {
        let res = sqlx::query("DELETE FROM voice_profiles WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        // Detach any transcript rows that referenced this profile so foreign
        // references don't dangle. We don't NULL out `speaker` because the
        // textual label may still be meaningful to the user.
        sqlx::query("UPDATE transcripts SET voice_profile_id = NULL WHERE voice_profile_id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Replace a profile's centroid with a new one (used after the user merges
    /// additional samples or rebuilds from scratch). `sample_count` is the new
    /// total, not a delta.
    pub async fn update_centroid(
        pool: &SqlitePool,
        id: &str,
        embedding: &[f32],
        sample_count: i64,
    ) -> Result<bool, SqlxError> {
        let now = Utc::now().to_rfc3339();
        let bytes = floats_to_bytes(embedding);
        let res = sqlx::query(
            "UPDATE voice_profiles
             SET embedding = ?, embedding_dim = ?, sample_count = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&bytes)
        .bind(embedding.len() as i64)
        .bind(sample_count)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

/// Pack `f32` slice as little-endian bytes for BLOB storage. Total bytes = 4 * len.
pub fn floats_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Inverse of [`floats_to_bytes`]. Returns `None` if `bytes.len()` is not a
/// multiple of 4 (corrupt blob).
pub fn bytes_to_floats(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().ok()?;
        out.push(f32::from_le_bytes(arr));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_floats_bytes() {
        let v = vec![1.0_f32, -2.5, 3.14159, 0.0, f32::INFINITY];
        let b = floats_to_bytes(&v);
        let r = bytes_to_floats(&b).unwrap();
        assert_eq!(r.len(), v.len());
        for (a, b) in v.iter().zip(&r) {
            assert!((a == b) || (a.is_nan() && b.is_nan()));
        }
    }

    #[test]
    fn corrupt_blob_returns_none() {
        let bytes = vec![0u8; 5]; // not a multiple of 4
        assert!(bytes_to_floats(&bytes).is_none());
    }
}
