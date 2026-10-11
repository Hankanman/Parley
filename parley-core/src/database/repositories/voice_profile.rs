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
    /// values are normalised to `NULL` — every caller (GPUI
    /// views) gets this for free instead of reimplementing it.
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

    /// The profile already called `name` (trimmed, case-insensitive), other
    /// than `exclude_id`. Used to stop a second profile being created or
    /// renamed onto an existing name (#102) — the caller offers a merge
    /// instead. If several already share the name, the self profile wins,
    /// then the one with the most samples.
    pub async fn find_by_name(
        pool: &SqlitePool,
        name: &str,
        exclude_id: Option<&str>,
    ) -> Result<Option<VoiceProfile>, SqlxError> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        sqlx::query_as::<_, VoiceProfile>(&format!(
            "SELECT {PROFILE_COLUMNS} FROM voice_profiles
             WHERE lower(trim(name)) = lower(?) AND id != ?
             ORDER BY is_self DESC, sample_count DESC, created_at
             LIMIT 1"
        ))
        .bind(name)
        .bind(exclude_id.unwrap_or(""))
        .fetch_optional(pool)
        .await
    }

    /// Fold `loser_id` into `winner_id` in one transaction:
    ///
    /// - the winner's centroid becomes the `sample_count`-weighted average
    ///   of both (skipped, with `centroid_updated = false`, if the embedding
    ///   dims differ — e.g. one predates a model change);
    /// - every transcript linked to the loser is re-pointed at the winner
    ///   and relabelled with the winner's name;
    /// - `is_self` survives: if the loser was the user's own profile, the
    ///   winner becomes it;
    /// - the winner keeps its email, or takes the loser's if it had none;
    /// - the loser is deleted.
    pub async fn merge_into(
        pool: &SqlitePool,
        winner_id: &str,
        loser_id: &str,
    ) -> Result<ProfileMergeOutcome, SqlxError> {
        if winner_id == loser_id {
            return Err(SqlxError::Protocol(
                "cannot merge a voice profile into itself".to_string(),
            ));
        }
        let mut tx = pool.begin().await?;
        let select = format!("SELECT {PROFILE_COLUMNS} FROM voice_profiles WHERE id = ?");
        let winner = sqlx::query_as::<_, VoiceProfile>(&select)
            .bind(winner_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(SqlxError::RowNotFound)?;
        let loser = sqlx::query_as::<_, VoiceProfile>(&select)
            .bind(loser_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(SqlxError::RowNotFound)?;

        let now = Utc::now().to_rfc3339();
        let merged = match (
            bytes_to_floats(&winner.embedding),
            bytes_to_floats(&loser.embedding),
        ) {
            (Some(w), Some(l)) if winner.embedding_dim == loser.embedding_dim && w.len() == l.len() => {
                Some(crate::speaker_diarization::embedding_math::merge_centroids(
                    &w,
                    winner.sample_count.max(0) as usize,
                    &l,
                    loser.sample_count.max(0) as usize,
                ))
            }
            _ => None,
        };
        let centroid_updated = merged.is_some();
        let (embedding, sample_count) = match merged {
            Some(m) => (floats_to_bytes(&m), winner.sample_count + loser.sample_count),
            None => (winner.embedding.clone(), winner.sample_count),
        };
        let is_self = winner.is_self || loser.is_self;
        let email = winner.email.clone().or_else(|| loser.email.clone());

        // Drop the loser before flagging the winner, so the "one self
        // profile" unique index never sees two.
        sqlx::query("DELETE FROM voice_profiles WHERE id = ?")
            .bind(&loser.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE voice_profiles
             SET embedding = ?, sample_count = ?, is_self = ?, email = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&embedding)
        .bind(sample_count)
        .bind(is_self)
        .bind(&email)
        .bind(&now)
        .bind(&winner.id)
        .execute(&mut *tx)
        .await?;
        let relinked = sqlx::query(
            "UPDATE transcripts SET voice_profile_id = ?, speaker = ? WHERE voice_profile_id = ?",
        )
        .bind(&winner.id)
        .bind(&winner.name)
        .bind(&loser.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        tx.commit().await?;
        Ok(ProfileMergeOutcome {
            relinked_transcripts: relinked,
            centroid_updated,
            sample_count,
            is_self,
        })
    }
}

/// What [`VoiceProfilesRepository::merge_into`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileMergeOutcome {
    pub relinked_transcripts: u64,
    pub centroid_updated: bool,
    /// The surviving profile's new sample count.
    pub sample_count: i64,
    /// Whether the surviving profile is the user's own.
    pub is_self: bool,
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

    // ── merge / duplicate names (#102), against a migrated in-memory DB ──

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn test_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at)
             VALUES ('m1', 'Meeting', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn transcript(pool: &SqlitePool, id: &str, speaker: &str, profile_id: Option<&str>) {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, speaker, voice_profile_id)
             VALUES (?, 'm1', 'hello', '2026-10-01T00:00:00Z', ?, ?)",
        )
        .bind(id)
        .bind(speaker)
        .bind(profile_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn links(pool: &SqlitePool) -> Vec<(String, Option<String>, Option<String>)> {
        sqlx::query_as("SELECT id, speaker, voice_profile_id FROM transcripts ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn merging_a_duplicate_into_the_self_profile_keeps_it_self() {
        let pool = test_pool().await;
        // The user's enrolled voice (4 samples) and a duplicate promoted from
        // a meeting chip (12 samples), both "Seb Burrell".
        let me = VoiceProfilesRepository::upsert_self(&pool, "Seb Burrell", &[1.0, 0.0], 4)
            .await
            .unwrap();
        let dup = VoiceProfilesRepository::create(
            &pool,
            "Seb Burrell",
            Some("seb@example.com"),
            &[0.0, 1.0],
            12,
        )
        .await
        .unwrap();
        transcript(&pool, "t1", "Seb Burrell", Some(&dup)).await;
        transcript(&pool, "t2", "Seb Burrell", Some(&me)).await;
        transcript(&pool, "t3", "Speaker 1", None).await;

        let outcome = VoiceProfilesRepository::merge_into(&pool, &me, &dup).await.unwrap();
        assert_eq!(
            outcome,
            ProfileMergeOutcome {
                relinked_transcripts: 1,
                centroid_updated: true,
                sample_count: 16,
                is_self: true,
            }
        );

        let profiles = VoiceProfilesRepository::list_all(&pool).await.unwrap();
        assert_eq!(profiles.len(), 1);
        let merged = &profiles[0];
        assert_eq!(merged.id, me);
        assert!(merged.is_self);
        assert_eq!(merged.sample_count, 16);
        // The winner had no email; it takes the loser's.
        assert_eq!(merged.email.as_deref(), Some("seb@example.com"));
        // Weighted 4:12 toward the duplicate's axis, then normalized.
        let c = bytes_to_floats(&merged.embedding).unwrap();
        let (x, y) = (0.25f32, 0.75f32);
        let n = (x * x + y * y).sqrt();
        assert!((c[0] - x / n).abs() < 1e-5 && (c[1] - y / n).abs() < 1e-5, "{c:?}");

        assert_eq!(
            links(&pool).await,
            vec![
                ("t1".into(), Some("Seb Burrell".into()), Some(me.clone())),
                ("t2".into(), Some("Seb Burrell".into()), Some(me.clone())),
                ("t3".into(), Some("Speaker 1".into()), None),
            ]
        );
    }

    #[tokio::test]
    async fn merging_the_self_profile_away_moves_the_flag_to_the_winner() {
        let pool = test_pool().await;
        let me = VoiceProfilesRepository::upsert_self(&pool, "Me", &[1.0, 0.0], 4)
            .await
            .unwrap();
        let other = VoiceProfilesRepository::create(&pool, "Seb", None, &[1.0, 0.0], 10)
            .await
            .unwrap();
        transcript(&pool, "t1", "Me", Some(&me)).await;

        let outcome = VoiceProfilesRepository::merge_into(&pool, &other, &me).await.unwrap();
        assert!(outcome.is_self);
        let survivor = VoiceProfilesRepository::get_self(&pool).await.unwrap().unwrap();
        assert_eq!(survivor.id, other);
        assert_eq!(survivor.name, "Seb");
        assert_eq!(
            links(&pool).await,
            vec![("t1".into(), Some("Seb".into()), Some(other.clone()))]
        );
    }

    #[tokio::test]
    async fn merging_mismatched_dims_relinks_without_touching_the_centroid() {
        let pool = test_pool().await;
        let a = VoiceProfilesRepository::create(&pool, "A", None, &[1.0, 0.0], 3)
            .await
            .unwrap();
        let b = VoiceProfilesRepository::create(&pool, "B", None, &[0.0, 0.0, 1.0], 5)
            .await
            .unwrap();
        transcript(&pool, "t1", "B", Some(&b)).await;
        let outcome = VoiceProfilesRepository::merge_into(&pool, &a, &b).await.unwrap();
        assert!(!outcome.centroid_updated);
        assert_eq!(outcome.sample_count, 3);
        assert_eq!(outcome.relinked_transcripts, 1);
        let a_row = VoiceProfilesRepository::get_by_id(&pool, &a).await.unwrap().unwrap();
        assert_eq!(bytes_to_floats(&a_row.embedding).unwrap(), vec![1.0, 0.0]);
        assert!(VoiceProfilesRepository::get_by_id(&pool, &b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn merging_into_itself_or_a_missing_profile_fails_cleanly() {
        let pool = test_pool().await;
        let a = VoiceProfilesRepository::create(&pool, "A", None, &[1.0], 1)
            .await
            .unwrap();
        assert!(VoiceProfilesRepository::merge_into(&pool, &a, &a).await.is_err());
        assert!(VoiceProfilesRepository::merge_into(&pool, &a, "nope").await.is_err());
        assert!(VoiceProfilesRepository::get_by_id(&pool, &a).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn find_by_name_is_trimmed_case_insensitive_and_prefers_self() {
        let pool = test_pool().await;
        let other = VoiceProfilesRepository::create(&pool, "Seb Burrell", None, &[1.0], 25)
            .await
            .unwrap();
        assert_eq!(
            VoiceProfilesRepository::find_by_name(&pool, "  seb burrell ", None)
                .await
                .unwrap()
                .map(|p| p.id),
            Some(other.clone())
        );
        let me = VoiceProfilesRepository::upsert_self(&pool, "Seb Burrell", &[1.0], 4)
            .await
            .unwrap();
        assert_eq!(
            VoiceProfilesRepository::find_by_name(&pool, "SEB BURRELL", None)
                .await
                .unwrap()
                .map(|p| p.id),
            Some(me.clone())
        );
        // Excluding a profile (e.g. the one being renamed) finds the other.
        assert_eq!(
            VoiceProfilesRepository::find_by_name(&pool, "Seb Burrell", Some(&me))
                .await
                .unwrap()
                .map(|p| p.id),
            Some(other)
        );
        assert!(VoiceProfilesRepository::find_by_name(&pool, "Zoe", None)
            .await
            .unwrap()
            .is_none());
        assert!(VoiceProfilesRepository::find_by_name(&pool, "  ", None)
            .await
            .unwrap()
            .is_none());
    }
}
