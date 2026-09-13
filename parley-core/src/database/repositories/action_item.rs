//! Persistence for meeting action items.
//!
//! Action items used to exist only as prose inside the summary markdown. These
//! rows make them queryable and completable: the UI renders them as a checkable
//! task list, the MCP server can list/close them, and `external_ref` leaves room
//! for pushing an item to an outside tracker later.
//!
//! Provenance (`source`) matters for ownership. The post-summary extractor owns
//! `source = 'summary'` rows and replaces them wholesale when a summary is
//! regenerated ([`ActionItemsRepository::replace_summary_items`]); rows the user
//! (`manual`) or an agent (`agent`) created are never touched by that path.

use crate::database::models::ActionItem;
use chrono::Utc;
use sqlx::{Error as SqlxError, SqlitePool};
use uuid::Uuid;

/// Columns of `action_items` in the order [`ActionItem`] declares them.
const ACTION_ITEM_COLUMNS: &str = "id, meeting_id, text, assignee, due_hint, status, source, \
     external_ref, source_start_secs, source_end_secs, source_quote, created_at, updated_at, \
     completed_at";

/// Lifecycle values accepted by [`ActionItemsRepository::set_status`].
pub const STATUS_OPEN: &str = "open";
pub const STATUS_DONE: &str = "done";

/// Provenance values. See the module docs for the ownership rules.
pub const SOURCE_SUMMARY: &str = "summary";
pub const SOURCE_MANUAL: &str = "manual";
pub const SOURCE_AGENT: &str = "agent";

/// An action item about to be written, before it has an id or timestamps.
/// Produced by the extractor and by the manual-create command.
#[derive(Debug, Clone, Default)]
pub struct NewActionItem {
    pub text: String,
    pub assignee: Option<String>,
    pub due_hint: Option<String>,
    /// Recording-relative seconds of the transcript segment the item was
    /// grounded to (transcript-sourced extractor only). `None` otherwise.
    pub source_start_secs: Option<f64>,
    pub source_end_secs: Option<f64>,
    /// The transcript sentence the model cited as evidence.
    pub source_quote: Option<String>,
}

pub struct ActionItemsRepository;

impl ActionItemsRepository {
    /// Insert one item. `source` should be one of [`SOURCE_SUMMARY`],
    /// [`SOURCE_MANUAL`], [`SOURCE_AGENT`]. New items always start `open`.
    ///
    /// `item.text` is trimmed and rejected if empty — the same rule
    /// [`Self::update`] enforces — so every caller (Tauri command, GPUI
    /// view, extractor) gets validation for free instead of reimplementing
    /// it at the call site.
    pub async fn create(
        pool: &SqlitePool,
        meeting_id: &str,
        item: &NewActionItem,
        source: &str,
    ) -> Result<ActionItem, SqlxError> {
        let text = item.text.trim();
        if text.is_empty() {
            return Err(SqlxError::Protocol(
                "action item text cannot be empty".to_string(),
            ));
        }

        let id = format!("action-{}", Uuid::new_v4());
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO action_items
             (id, meeting_id, text, assignee, due_hint, status, source, external_ref,
              created_at, updated_at, completed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, NULL)",
        )
        .bind(&id)
        .bind(meeting_id)
        .bind(text)
        .bind(&item.assignee)
        .bind(&item.due_hint)
        .bind(STATUS_OPEN)
        .bind(source)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get_by_id(pool, &id)
            .await?
            .ok_or(SqlxError::RowNotFound)
    }

    pub async fn get_by_id(pool: &SqlitePool, id: &str) -> Result<Option<ActionItem>, SqlxError> {
        sqlx::query_as::<_, ActionItem>(&format!(
            "SELECT {ACTION_ITEM_COLUMNS} FROM action_items WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
    }

    /// Every item for a meeting, open ones first, then oldest-first within each
    /// group — the order the task list renders in.
    pub async fn list_by_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<ActionItem>, SqlxError> {
        sqlx::query_as::<_, ActionItem>(&format!(
            "SELECT {ACTION_ITEM_COLUMNS} FROM action_items
             WHERE meeting_id = ?
             ORDER BY (status = '{STATUS_DONE}') ASC, created_at ASC"
        ))
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    /// Every item across every meeting, newest first. The unfiltered view an
    /// agent asks for when it wants the whole picture, not just what's due.
    pub async fn list_all(pool: &SqlitePool) -> Result<Vec<ActionItem>, SqlxError> {
        sqlx::query_as::<_, ActionItem>(&format!(
            "SELECT {ACTION_ITEM_COLUMNS} FROM action_items ORDER BY created_at DESC"
        ))
        .fetch_all(pool)
        .await
    }

    /// Open items only. `meeting_id = None` spans every meeting (newest first),
    /// which is what the cross-meeting "what's outstanding" surfaces want.
    pub async fn list_open(
        pool: &SqlitePool,
        meeting_id: Option<&str>,
    ) -> Result<Vec<ActionItem>, SqlxError> {
        match meeting_id {
            Some(mid) => {
                sqlx::query_as::<_, ActionItem>(&format!(
                    "SELECT {ACTION_ITEM_COLUMNS} FROM action_items
                     WHERE meeting_id = ? AND status = '{STATUS_OPEN}'
                     ORDER BY created_at ASC"
                ))
                .bind(mid)
                .fetch_all(pool)
                .await
            }
            None => {
                sqlx::query_as::<_, ActionItem>(&format!(
                    "SELECT {ACTION_ITEM_COLUMNS} FROM action_items
                     WHERE status = '{STATUS_OPEN}'
                     ORDER BY created_at DESC"
                ))
                .fetch_all(pool)
                .await
            }
        }
    }

    pub async fn count_open_for_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<i64, SqlxError> {
        let count: (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM action_items WHERE meeting_id = ? AND status = '{STATUS_OPEN}'"
        ))
        .bind(meeting_id)
        .fetch_one(pool)
        .await?;
        Ok(count.0)
    }

    /// Flip an item between `open` and `done`, keeping `completed_at` in sync:
    /// set on completion, cleared on reopen. Returns `None` when the id is
    /// unknown. Rejects any status outside the two lifecycle values rather than
    /// writing a value the UI can't render.
    pub async fn set_status(
        pool: &SqlitePool,
        id: &str,
        status: &str,
    ) -> Result<Option<ActionItem>, SqlxError> {
        if status != STATUS_OPEN && status != STATUS_DONE {
            return Err(SqlxError::Protocol(format!(
                "invalid action item status '{}': expected '{}' or '{}'",
                status, STATUS_OPEN, STATUS_DONE
            )));
        }

        let now = Utc::now().to_rfc3339();
        let completed_at = (status == STATUS_DONE).then(|| now.clone());

        let res = sqlx::query(
            "UPDATE action_items SET status = ?, completed_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(status)
        .bind(&completed_at)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;

        if res.rows_affected() == 0 {
            return Ok(None);
        }
        Self::get_by_id(pool, id).await
    }

    /// Patch an item's editable fields. Each argument is "leave unchanged" when
    /// `None`; passing `Some("")` for `assignee`/`due_hint` clears the field
    /// (an empty owner is never meaningful, so it's spelled as NULL). An empty
    /// `text` is rejected — an item with no text can't be acted on.
    pub async fn update(
        pool: &SqlitePool,
        id: &str,
        text: Option<&str>,
        assignee: Option<&str>,
        due_hint: Option<&str>,
    ) -> Result<Option<ActionItem>, SqlxError> {
        if let Some(t) = text {
            if t.trim().is_empty() {
                return Err(SqlxError::Protocol(
                    "action item text cannot be empty".to_string(),
                ));
            }
        }
        if text.is_none() && assignee.is_none() && due_hint.is_none() {
            return Self::get_by_id(pool, id).await;
        }

        let now = Utc::now().to_rfc3339();
        let mut sets: Vec<&str> = Vec::new();
        if text.is_some() {
            sets.push("text = ?");
        }
        if assignee.is_some() {
            sets.push("assignee = ?");
        }
        if due_hint.is_some() {
            sets.push("due_hint = ?");
        }
        sets.push("updated_at = ?");

        let sql = format!("UPDATE action_items SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(t) = text {
            q = q.bind(t.trim().to_string());
        }
        if let Some(a) = assignee {
            q = q.bind(blank_to_none(a));
        }
        if let Some(d) = due_hint {
            q = q.bind(blank_to_none(d));
        }
        let res = q.bind(&now).bind(id).execute(pool).await?;

        if res.rows_affected() == 0 {
            return Ok(None);
        }
        Self::get_by_id(pool, id).await
    }

    pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, SqlxError> {
        let res = sqlx::query("DELETE FROM action_items WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Replace the extractor-owned items for a meeting with `items`, atomically.
    ///
    /// This is what makes re-summarizing idempotent: without it, every
    /// regeneration would append a near-duplicate set. `manual`/`agent` items
    /// are outside the extractor's ownership and survive untouched.
    ///
    /// Items whose text matches a previous extraction (case-insensitively,
    /// whitespace-normalized) **keep their id, status, completion time and
    /// `external_ref`** — a user who ticked off "send the budget" before
    /// regenerating the summary shouldn't see it come back unchecked, and any
    /// external tracker link stays valid.
    ///
    /// Returns the number of rows written.
    pub async fn replace_summary_items(
        pool: &SqlitePool,
        meeting_id: &str,
        items: &[NewActionItem],
    ) -> Result<usize, SqlxError> {
        let now = Utc::now().to_rfc3339();
        let mut tx = pool.begin().await?;

        // Carry-over state from the extraction we're about to replace.
        let previous = sqlx::query_as::<_, ActionItem>(&format!(
            "SELECT {ACTION_ITEM_COLUMNS} FROM action_items
             WHERE meeting_id = ? AND source = '{SOURCE_SUMMARY}'"
        ))
        .bind(meeting_id)
        .fetch_all(&mut *tx)
        .await?;

        sqlx::query(&format!(
            "DELETE FROM action_items WHERE meeting_id = ? AND source = '{SOURCE_SUMMARY}'"
        ))
        .bind(meeting_id)
        .execute(&mut *tx)
        .await?;

        let mut written = 0usize;
        // Ids already claimed by this batch. A previous row can only be carried
        // over once, however the caller de-duplicated: reusing an id twice would
        // violate the primary key and roll back the entire replace.
        let mut claimed: Vec<&str> = Vec::new();

        for item in items {
            let key = normalize_text_key(&item.text);
            let carried = previous
                .iter()
                .find(|p| normalize_text_key(&p.text) == key && !claimed.contains(&p.id.as_str()));
            if let Some(p) = carried {
                claimed.push(&p.id);
            }

            let (id, status, completed_at, external_ref, created_at) = match carried {
                Some(p) => (
                    p.id.clone(),
                    p.status.clone(),
                    p.completed_at.clone(),
                    p.external_ref.clone(),
                    p.created_at.clone(),
                ),
                None => (
                    format!("action-{}", Uuid::new_v4()),
                    STATUS_OPEN.to_string(),
                    None,
                    None,
                    now.clone(),
                ),
            };

            sqlx::query(
                "INSERT INTO action_items
                 (id, meeting_id, text, assignee, due_hint, status, source, external_ref,
                  source_start_secs, source_end_secs, source_quote,
                  created_at, updated_at, completed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(meeting_id)
            .bind(&item.text)
            .bind(&item.assignee)
            .bind(&item.due_hint)
            .bind(&status)
            .bind(SOURCE_SUMMARY)
            .bind(&external_ref)
            .bind(item.source_start_secs)
            .bind(item.source_end_secs)
            .bind(&item.source_quote)
            .bind(&created_at)
            .bind(&now)
            .bind(&completed_at)
            .execute(&mut *tx)
            .await?;
            written += 1;
        }

        tx.commit().await?;
        Ok(written)
    }
}

/// Trim, then map an empty string to NULL. Used for the optional text columns
/// where "" and "no value" mean the same thing to every consumer.
fn blank_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Identity key for matching an item across extractions: lowercase, with runs
/// of whitespace collapsed and any trailing period dropped. Two LLM passes over
/// the same summary rarely produce byte-identical text (trailing periods,
/// capitalization), so an exact match would lose completion state on almost
/// every regeneration.
///
/// Public because the extractor must de-duplicate its parsed items with the
/// *same* notion of sameness this uses for carry-over. If it de-duped more
/// loosely, two items could match one previous row and both try to reuse its
/// id — a primary-key collision that aborts the whole replace.
pub fn normalize_text_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_end_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_to_none_maps_empty_and_whitespace() {
        assert_eq!(blank_to_none(""), None);
        assert_eq!(blank_to_none("   "), None);
        assert_eq!(blank_to_none(" Seb "), Some("Seb".to_string()));
    }

    #[test]
    fn normalize_key_ignores_case_spacing_and_trailing_period() {
        assert_eq!(
            normalize_text_key("Send  the Q3 budget to Finance."),
            normalize_text_key("send the q3 budget to finance")
        );
    }

    #[test]
    fn normalize_key_keeps_distinct_items_distinct() {
        assert_ne!(
            normalize_text_key("Send the budget"),
            normalize_text_key("Send the deck")
        );
    }

    #[test]
    fn normalize_key_is_stable_for_already_clean_text() {
        assert_eq!(normalize_text_key("send the budget"), "send the budget");
    }
}
