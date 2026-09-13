use crate::database::models::{MeetingDetails, MeetingTranscript};
use crate::database::models::{MeetingModel, Transcript};
use chrono::Utc;
use sqlx::{Connection, Error as SqlxError, SqliteConnection, SqlitePool};
use tracing::{error, info};

pub struct MeetingsRepository;

impl MeetingsRepository {
    pub async fn get_meetings(pool: &SqlitePool) -> Result<Vec<MeetingModel>, sqlx::Error> {
        let meetings =
            sqlx::query_as::<_, MeetingModel>("SELECT * FROM meetings ORDER BY created_at DESC")
                .fetch_all(pool)
                .await?;
        Ok(meetings)
    }

    pub async fn delete_meeting(pool: &SqlitePool, meeting_id: &str) -> Result<bool, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        match delete_meeting_with_transaction(&mut transaction, meeting_id).await {
            Ok(success) => {
                if success {
                    transaction.commit().await?;
                    info!(
                        "Successfully deleted meeting {} and all associated data",
                        meeting_id
                    );
                    Ok(true)
                } else {
                    transaction.rollback().await?;
                    Ok(false)
                }
            }
            Err(e) => {
                let _ = transaction.rollback().await;
                error!("Failed to delete meeting {}: {}", meeting_id, e);
                Err(e)
            }
        }
    }

    pub async fn get_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<MeetingDetails>, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        // Get meeting details
        let meeting: Option<MeetingModel> = sqlx::query_as(
            "SELECT id, title, created_at, updated_at, folder_path, status, completed_at, duration_seconds, audio_path FROM meetings WHERE id = ?",
        )
        .bind(meeting_id)
        .fetch_optional(&mut *transaction)
        .await?;

        if meeting.is_none() {
            transaction.rollback().await?;
            return Err(SqlxError::RowNotFound);
        }

        if let Some(meeting) = meeting {
            // Get all transcripts for this meeting
            let transcripts =
                sqlx::query_as::<_, Transcript>("SELECT * FROM transcripts WHERE meeting_id = ?")
                    .bind(meeting_id)
                    .fetch_all(&mut *transaction)
                    .await?;

            transaction.commit().await?;

            // Convert Transcript to MeetingTranscript
            let meeting_transcripts = transcripts
                .into_iter()
                .map(|t| MeetingTranscript {
                    id: t.id,
                    text: t.transcript,
                    timestamp: t.timestamp,
                    audio_start_time: t.audio_start_time,
                    audio_end_time: t.audio_end_time,
                    duration: t.duration,
                    speaker: t.speaker,
                    voice_profile_id: t.voice_profile_id,
                    source: t.source,
                    confidence: t.confidence,
                })
                .collect::<Vec<_>>();

            Ok(Some(MeetingDetails {
                id: meeting.id,
                title: meeting.title,
                created_at: meeting.created_at.0.to_rfc3339(),
                updated_at: meeting.updated_at.0.to_rfc3339(),
                transcripts: meeting_transcripts,
            }))
        } else {
            transaction.rollback().await?;
            Ok(None)
        }
    }

    /// Get meeting metadata without transcripts (for pagination)
    pub async fn get_meeting_metadata(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<MeetingModel>, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let meeting: Option<MeetingModel> = sqlx::query_as(
            "SELECT id, title, created_at, updated_at, folder_path, status, completed_at, duration_seconds, audio_path FROM meetings WHERE id = ?",
        )
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?;

        Ok(meeting)
    }

    /// Get meeting transcripts with pagination support
    pub async fn get_meeting_transcripts_paginated(
        pool: &SqlitePool,
        meeting_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Transcript>, i64), SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        // Get total count of transcripts for this meeting
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transcripts WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_one(pool)
            .await?;

        // Get paginated transcripts ordered by audio_start_time
        let transcripts = sqlx::query_as::<_, Transcript>(
            "SELECT * FROM transcripts
             WHERE meeting_id = ?
             ORDER BY audio_start_time ASC
             LIMIT ? OFFSET ?",
        )
        .bind(meeting_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        Ok((transcripts, total.0))
    }

    pub async fn update_meeting_title(
        pool: &SqlitePool,
        meeting_id: &str,
        new_title: &str,
    ) -> Result<bool, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol(
                "meeting_id cannot be empty".to_string(),
            ));
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now().naive_utc();

        let rows_affected =
            sqlx::query("UPDATE meetings SET title = ?, updated_at = ? WHERE id = ?")
                .bind(new_title)
                .bind(now)
                .bind(meeting_id)
                .execute(&mut *transaction)
                .await?;
        if rows_affected.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

    pub async fn update_meeting_name(
        pool: &SqlitePool,
        meeting_id: &str,
        new_title: &str,
    ) -> Result<bool, SqlxError> {
        let mut transaction = pool.begin().await?;
        let now = Utc::now();

        // Update meetings table
        let meeting_update =
            sqlx::query("UPDATE meetings SET title = ?, updated_at = ? WHERE id = ?")
                .bind(new_title)
                .bind(now)
                .bind(meeting_id)
                .execute(&mut *transaction)
                .await?;

        if meeting_update.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(false); // Meeting not found
        }

        transaction.commit().await?;
        Ok(true)
    }

    // ------------------------------------------------------------------
    // Lifecycle (issue #57 slice 2): Rust owns the meeting row from the
    // moment recording starts, so recovery after a crash is a database
    // query instead of the frontend reconciling an IndexedDB cache.
    // ------------------------------------------------------------------

    /// Insert the meeting row at the moment recording starts, status
    /// "recording". Caller-supplied `meeting_id` (a fresh uuid) is kept for
    /// the whole recording session so live transcript segments, the
    /// `recording-stopped` payload, and this row all agree on one id.
    pub async fn create_recording_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
        title: &str,
        folder_path: Option<&str>,
    ) -> Result<(), SqlxError> {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path, status)
             VALUES (?, ?, ?, ?, ?, 'recording')",
        )
        .bind(meeting_id)
        .bind(title)
        .bind(now)
        .bind(now)
        .bind(folder_path)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Finalise a meeting row on a normal stop: status -> "completed".
    pub async fn mark_meeting_completed(
        pool: &SqlitePool,
        meeting_id: &str,
        duration_seconds: Option<f64>,
        audio_path: Option<&str>,
    ) -> Result<bool, SqlxError> {
        let now = Utc::now();
        let result = sqlx::query(
            "UPDATE meetings
             SET status = 'completed', completed_at = ?, duration_seconds = ?, audio_path = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(now)
        .bind(duration_seconds)
        .bind(audio_path)
        .bind(now)
        .bind(meeting_id)
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Mark a meeting row "interrupted" — a fatal-error stop, or the
    /// crash-marker sweep at startup finding it still "recording".
    pub async fn mark_meeting_interrupted(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<bool, SqlxError> {
        let now = Utc::now();
        let result = sqlx::query(
            "UPDATE meetings SET status = 'interrupted', updated_at = ? WHERE id = ?",
        )
        .bind(now)
        .bind(meeting_id)
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Crash marker: any row still "recording" from a previous run of the
    /// app (the process ended without `stop_recording` ever finalising it)
    /// becomes "interrupted". Run once at startup, after the database pool
    /// is ready. Returns the number of rows updated.
    pub async fn mark_stale_recording_meetings_interrupted(
        pool: &SqlitePool,
    ) -> Result<u64, SqlxError> {
        let now = Utc::now();
        let result = sqlx::query(
            "UPDATE meetings SET status = 'interrupted', updated_at = ? WHERE status = 'recording'",
        )
        .bind(now)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// List every "interrupted" meeting, most recent first, with its saved
    /// transcript-segment count — everything the recovery dialog needs
    /// except whether `.checkpoints` audio still exists on disk, which is a
    /// filesystem check the caller does per row (see
    /// `audio::recovery_commands::list_interrupted_meetings`).
    pub async fn list_interrupted_meetings(
        pool: &SqlitePool,
    ) -> Result<Vec<InterruptedMeetingRow>, SqlxError> {
        let rows = sqlx::query_as::<_, InterruptedMeetingRow>(
            "SELECT m.id AS meeting_id, m.title AS title, m.folder_path AS folder_path,
                    m.created_at AS created_at,
                    COALESCE((SELECT COUNT(*) FROM transcripts t WHERE t.meeting_id = m.id), 0) AS segment_count
             FROM meetings m
             WHERE m.status = 'interrupted'
             ORDER BY m.created_at DESC",
        )
        .fetch_all(pool)
        .await?;
        Ok(rows)
    }
}

/// One row of `MeetingsRepository::list_interrupted_meetings`'s result.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct InterruptedMeetingRow {
    pub meeting_id: String,
    pub title: String,
    pub folder_path: Option<String>,
    pub created_at: crate::database::models::DateTimeUtc,
    pub segment_count: i64,
}

async fn delete_meeting_with_transaction(
    transaction: &mut SqliteConnection,
    meeting_id: &str,
) -> Result<bool, SqlxError> {
    // Check if meeting exists
    let meeting_exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(&mut *transaction)
        .await?;

    if meeting_exists.is_none() {
        error!("Meeting {} not found for deletion", meeting_id);
        return Ok(false);
    }

    // Delete from related tables in proper order
    // 1. Delete from summary_processes
    sqlx::query("DELETE FROM summary_processes WHERE meeting_id = ?")
        .bind(meeting_id)
        .execute(&mut *transaction)
        .await?;

    // 2. Delete from transcripts
    sqlx::query("DELETE FROM transcripts WHERE meeting_id = ?")
        .bind(meeting_id)
        .execute(&mut *transaction)
        .await?;

    // 3. Finally, delete the meeting
    let result = sqlx::query("DELETE FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .execute(&mut *transaction)
        .await?;

    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn status_of(pool: &SqlitePool, meeting_id: &str) -> String {
        let row: (String,) = sqlx::query_as("SELECT status FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    /// A row created at recording start is "recording"; a normal stop moves
    /// it to "completed" and stamps duration/audio_path/completed_at.
    #[tokio::test]
    async fn create_then_complete_transitions_recording_to_completed() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "Standup", Some("/tmp/m1"))
            .await
            .unwrap();
        assert_eq!(status_of(&pool, "m1").await, "recording");

        let updated =
            MeetingsRepository::mark_meeting_completed(&pool, "m1", Some(123.5), Some("/tmp/m1/audio.mp4"))
                .await
                .unwrap();
        assert!(updated);
        assert_eq!(status_of(&pool, "m1").await, "completed");

        let meeting = MeetingsRepository::get_meeting_metadata(&pool, "m1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meeting.duration_seconds, Some(123.5));
        assert_eq!(meeting.audio_path.as_deref(), Some("/tmp/m1/audio.mp4"));
        assert!(meeting.completed_at.is_some());
    }

    /// A fatal-error stop moves the row to "interrupted" instead.
    #[tokio::test]
    async fn mark_interrupted_transitions_recording_to_interrupted() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m2", "Crashy", None)
            .await
            .unwrap();

        let updated = MeetingsRepository::mark_meeting_interrupted(&pool, "m2")
            .await
            .unwrap();
        assert!(updated);
        assert_eq!(status_of(&pool, "m2").await, "interrupted");
    }

    /// The startup crash-marker sweep flips every "recording" row to
    /// "interrupted" and leaves every other status untouched.
    #[tokio::test]
    async fn stale_sweep_only_touches_recording_rows() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "still-recording", "A", None)
            .await
            .unwrap();
        MeetingsRepository::create_recording_meeting(&pool, "will-complete", "B", None)
            .await
            .unwrap();
        MeetingsRepository::mark_meeting_completed(&pool, "will-complete", None, None)
            .await
            .unwrap();

        let swept = MeetingsRepository::mark_stale_recording_meetings_interrupted(&pool)
            .await
            .unwrap();
        assert_eq!(swept, 1);

        assert_eq!(status_of(&pool, "still-recording").await, "interrupted");
        assert_eq!(status_of(&pool, "will-complete").await, "completed");

        // Idempotent: nothing left "recording" to sweep a second time.
        let swept_again = MeetingsRepository::mark_stale_recording_meetings_interrupted(&pool)
            .await
            .unwrap();
        assert_eq!(swept_again, 0);
    }

    /// `list_interrupted_meetings` returns only "interrupted" rows, most
    /// recent first, with a correct transcript segment count per row.
    #[tokio::test]
    async fn list_interrupted_meetings_reports_segment_counts() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "old", "Old", None)
            .await
            .unwrap();
        MeetingsRepository::mark_meeting_interrupted(&pool, "old")
            .await
            .unwrap();

        MeetingsRepository::create_recording_meeting(&pool, "recent", "Recent", None)
            .await
            .unwrap();
        MeetingsRepository::mark_meeting_interrupted(&pool, "recent")
            .await
            .unwrap();

        // A completed meeting must never show up here.
        MeetingsRepository::create_recording_meeting(&pool, "done", "Done", None)
            .await
            .unwrap();
        MeetingsRepository::mark_meeting_completed(&pool, "done", None, None)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, sequence_id)
             VALUES ('t1', 'recent', 'hello', '2026-01-01T00:00:00Z', 1),
                    ('t2', 'recent', 'world', '2026-01-01T00:00:01Z', 2)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let rows = MeetingsRepository::list_interrupted_meetings(&pool)
            .await
            .unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.meeting_id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"old"));
        assert!(ids.contains(&"recent"));
        assert!(!ids.contains(&"done"));

        let recent_row = rows.iter().find(|r| r.meeting_id == "recent").unwrap();
        assert_eq!(recent_row.segment_count, 2);
        let old_row = rows.iter().find(|r| r.meeting_id == "old").unwrap();
        assert_eq!(old_row.segment_count, 0);
    }
}
