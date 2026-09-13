//! Persistence for free-text meeting notes.
//!
//! Notes are append-only in practice: the user (or an agent over MCP) jots
//! something against a meeting and it stands as written. There's no update
//! path — editing a note is spelled as delete + add, which keeps the audit
//! trail honest about when each observation was made.

use crate::database::models::MeetingNote;
use chrono::Utc;
use sqlx::{Error as SqlxError, SqlitePool};
use uuid::Uuid;

/// Columns of `meeting_notes` in the order [`MeetingNote`] declares them.
const NOTE_COLUMNS: &str = "id, meeting_id, body, source, created_at";

/// Provenance values for `meeting_notes.source`.
pub const SOURCE_MANUAL: &str = "manual";
pub const SOURCE_AGENT: &str = "agent";

pub struct MeetingNotesRepository;

impl MeetingNotesRepository {
    /// Append a note. Rejects an empty body — a blank note carries no
    /// information and would render as an empty row forever.
    pub async fn create(
        pool: &SqlitePool,
        meeting_id: &str,
        body: &str,
        source: &str,
    ) -> Result<MeetingNote, SqlxError> {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return Err(SqlxError::Protocol(
                "note body cannot be empty".to_string(),
            ));
        }

        let id = format!("note-{}", Uuid::new_v4());
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO meeting_notes (id, meeting_id, body, source, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(meeting_id)
        .bind(trimmed)
        .bind(source)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get_by_id(pool, &id)
            .await?
            .ok_or(SqlxError::RowNotFound)
    }

    pub async fn get_by_id(pool: &SqlitePool, id: &str) -> Result<Option<MeetingNote>, SqlxError> {
        sqlx::query_as::<_, MeetingNote>(&format!(
            "SELECT {NOTE_COLUMNS} FROM meeting_notes WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
    }

    /// Notes for a meeting, oldest first — they read as a running log.
    pub async fn list_by_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<MeetingNote>, SqlxError> {
        sqlx::query_as::<_, MeetingNote>(&format!(
            "SELECT {NOTE_COLUMNS} FROM meeting_notes WHERE meeting_id = ? ORDER BY created_at ASC"
        ))
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, SqlxError> {
        let res = sqlx::query("DELETE FROM meeting_notes WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}
