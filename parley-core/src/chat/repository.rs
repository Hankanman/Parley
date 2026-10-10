//! Persistence for "Ask Parley" chat history (`meeting_chat_messages`).
//!
//! One row per turn, owned by a meeting (ON DELETE CASCADE, plus an explicit
//! delete in `MeetingsRepository::delete_meeting`). Turns are listed oldest
//! first; `created_at` carries nanosecond precision and `rowid` breaks any
//! remaining tie, so a question and its answer never swap places.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Error as SqlxError, FromRow, SqlitePool};
use uuid::Uuid;

pub const ROLE_USER: &str = "user";
pub const ROLE_ASSISTANT: &str = "assistant";

const COLUMNS: &str = "id, meeting_id, role, content, created_at";

/// One stored chat turn.
#[derive(Debug, Clone, PartialEq, Eq, FromRow, Serialize, Deserialize)]
pub struct MeetingChatMessage {
    pub id: String,
    pub meeting_id: String,
    /// [`ROLE_USER`] | [`ROLE_ASSISTANT`].
    pub role: String,
    pub content: String,
    pub created_at: String,
}

impl MeetingChatMessage {
    pub fn is_user(&self) -> bool {
        self.role == ROLE_USER
    }
}

pub struct MeetingChatRepository;

impl MeetingChatRepository {
    /// Append a turn. Rejects an unknown role or empty content.
    pub async fn append(
        pool: &SqlitePool,
        meeting_id: &str,
        role: &str,
        content: &str,
    ) -> Result<MeetingChatMessage, SqlxError> {
        if role != ROLE_USER && role != ROLE_ASSISTANT {
            return Err(SqlxError::Protocol(format!("invalid chat role: {role}")));
        }
        let content = content.trim();
        if content.is_empty() {
            return Err(SqlxError::Protocol(
                "chat message cannot be empty".to_string(),
            ));
        }

        let message = MeetingChatMessage {
            id: format!("chat-{}", Uuid::new_v4()),
            meeting_id: meeting_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        };

        sqlx::query(
            "INSERT INTO meeting_chat_messages (id, meeting_id, role, content, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&message.id)
        .bind(&message.meeting_id)
        .bind(&message.role)
        .bind(&message.content)
        .bind(&message.created_at)
        .execute(pool)
        .await?;

        Ok(message)
    }

    /// A meeting's chat, oldest turn first.
    pub async fn list_by_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<MeetingChatMessage>, SqlxError> {
        sqlx::query_as::<_, MeetingChatMessage>(&format!(
            "SELECT {COLUMNS} FROM meeting_chat_messages WHERE meeting_id = ? \
             ORDER BY created_at ASC, rowid ASC"
        ))
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    /// Remove one turn (used to roll back a question whose answer failed).
    pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, SqlxError> {
        let res = sqlx::query("DELETE FROM meeting_chat_messages WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// "Clear chat": drop a meeting's whole history. Returns rows removed.
    pub async fn clear(pool: &SqlitePool, meeting_id: &str) -> Result<u64, SqlxError> {
        let res = sqlx::query("DELETE FROM meeting_chat_messages WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::meeting::MeetingsRepository;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    pub(crate) async fn test_pool() -> SqlitePool {
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
        pool
    }

    #[tokio::test]
    async fn append_and_list_in_order() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "Standup", None)
            .await
            .unwrap();

        let q = MeetingChatRepository::append(&pool, "m1", ROLE_USER, "  What was agreed?  ")
            .await
            .unwrap();
        assert_eq!(q.content, "What was agreed?");
        MeetingChatRepository::append(&pool, "m1", ROLE_ASSISTANT, "Ship Friday [01:02].")
            .await
            .unwrap();
        MeetingChatRepository::append(&pool, "m1", ROLE_USER, "Who owns it?")
            .await
            .unwrap();

        let all = MeetingChatRepository::list_by_meeting(&pool, "m1").await.unwrap();
        let roles: Vec<&str> = all.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        assert_eq!(all[0].id, q.id);
        assert!(all[0].is_user());
        assert!(MeetingChatRepository::list_by_meeting(&pool, "other")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn rejects_bad_role_and_empty_content() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "Standup", None)
            .await
            .unwrap();
        assert!(MeetingChatRepository::append(&pool, "m1", "system", "hi").await.is_err());
        assert!(MeetingChatRepository::append(&pool, "m1", ROLE_USER, "   ").await.is_err());
    }

    #[tokio::test]
    async fn delete_and_clear() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None).await.unwrap();
        MeetingsRepository::create_recording_meeting(&pool, "m2", "B", None).await.unwrap();
        let a = MeetingChatRepository::append(&pool, "m1", ROLE_USER, "one").await.unwrap();
        MeetingChatRepository::append(&pool, "m1", ROLE_ASSISTANT, "two").await.unwrap();
        MeetingChatRepository::append(&pool, "m2", ROLE_USER, "keep").await.unwrap();

        assert!(MeetingChatRepository::delete(&pool, &a.id).await.unwrap());
        assert!(!MeetingChatRepository::delete(&pool, &a.id).await.unwrap());
        assert_eq!(MeetingChatRepository::clear(&pool, "m1").await.unwrap(), 1);
        assert!(MeetingChatRepository::list_by_meeting(&pool, "m1").await.unwrap().is_empty());
        assert_eq!(MeetingChatRepository::list_by_meeting(&pool, "m2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_meeting_removes_chat() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None).await.unwrap();
        MeetingChatRepository::append(&pool, "m1", ROLE_USER, "hello").await.unwrap();
        assert!(MeetingsRepository::delete_meeting(&pool, "m1").await.unwrap());
        assert!(MeetingChatRepository::list_by_meeting(&pool, "m1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cascade_removes_chat_on_raw_meeting_delete() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None).await.unwrap();
        MeetingChatRepository::append(&pool, "m1", ROLE_USER, "hello").await.unwrap();
        sqlx::query("DELETE FROM meetings WHERE id = 'm1'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(MeetingChatRepository::list_by_meeting(&pool, "m1").await.unwrap().is_empty());
    }
}
