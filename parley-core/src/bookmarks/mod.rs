//! Bookmarks: moments the user marked during a recording (issue #87).
//!
//! A bookmark is a recording-relative offset (same clock as
//! `transcripts.audio_start_time`) plus an optional short label, stored in
//! `meeting_bookmarks` and owned by its meeting (ON DELETE CASCADE, plus an
//! explicit delete in `MeetingsRepository::delete_meeting`).
//!
//! [`bookmark_now`] marks "this moment" of the recording in progress — the
//! recording view's button, the tray item and the keyboard shortcut all go
//! through it. [`nearest_segment_index`] maps a bookmark onto the transcript
//! segment it belongs to, and [`bookmark_context`] turns a meeting's
//! bookmarks into the emphasis note the summary prompt carries.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Error as SqlxError, FromRow, SqlitePool};
use uuid::Uuid;

use crate::chat::context::format_timestamp;
use crate::database::models::MeetingTranscript;
use crate::events::{EventSinkExt, SharedEventSink};

/// Emitted whenever a meeting's bookmarks change:
/// `{ "meeting_id": String }`.
pub const BOOKMARKS_CHANGED_EVENT: &str = "meeting-bookmarks-changed";

/// Longest label kept, in characters.
pub const MAX_LABEL_CHARS: usize = 80;
/// Longest transcript excerpt quoted per bookmark in the summary context.
const CONTEXT_EXCERPT_CHARS: usize = 160;
/// Most bookmarks carried into the summary context.
const MAX_CONTEXT_BOOKMARKS: usize = 30;

const COLUMNS: &str = "id, meeting_id, offset_seconds, label, created_at";

/// One stored bookmark.
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct MeetingBookmark {
    pub id: String,
    pub meeting_id: String,
    /// Seconds from recording start (pauses excluded).
    pub offset_seconds: f64,
    pub label: Option<String>,
    pub created_at: String,
}

/// Normalise a user label: control characters dropped, whitespace
/// collapsed, capped at [`MAX_LABEL_CHARS`]. Empty → `None`.
pub fn sanitize_label(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(MAX_LABEL_CHARS).collect();
    let capped = capped.trim().to_string();
    (!capped.is_empty()).then_some(capped)
}

fn valid_offset(offset_seconds: f64) -> Result<f64, SqlxError> {
    if offset_seconds.is_finite() && offset_seconds >= 0.0 {
        Ok(offset_seconds)
    } else {
        Err(SqlxError::Protocol(format!(
            "invalid bookmark offset: {offset_seconds}"
        )))
    }
}

pub struct BookmarksRepository;

impl BookmarksRepository {
    /// Add a bookmark at `offset_seconds` (must be finite and ≥ 0).
    pub async fn add(
        pool: &SqlitePool,
        meeting_id: &str,
        offset_seconds: f64,
        label: Option<&str>,
    ) -> Result<MeetingBookmark, SqlxError> {
        let bookmark = MeetingBookmark {
            id: format!("bm-{}", Uuid::new_v4()),
            meeting_id: meeting_id.to_string(),
            offset_seconds: valid_offset(offset_seconds)?,
            label: label.and_then(sanitize_label),
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        };
        sqlx::query(
            "INSERT INTO meeting_bookmarks (id, meeting_id, offset_seconds, label, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&bookmark.id)
        .bind(&bookmark.meeting_id)
        .bind(bookmark.offset_seconds)
        .bind(&bookmark.label)
        .bind(&bookmark.created_at)
        .execute(pool)
        .await?;
        Ok(bookmark)
    }

    /// A meeting's bookmarks in recording order.
    pub async fn list_by_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<MeetingBookmark>, SqlxError> {
        sqlx::query_as::<_, MeetingBookmark>(&format!(
            "SELECT {COLUMNS} FROM meeting_bookmarks WHERE meeting_id = ? \
             ORDER BY offset_seconds ASC, created_at ASC, rowid ASC"
        ))
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    /// Set (or clear, with an empty string) a bookmark's label.
    pub async fn update_label(
        pool: &SqlitePool,
        id: &str,
        label: &str,
    ) -> Result<bool, SqlxError> {
        let res = sqlx::query("UPDATE meeting_bookmarks SET label = ? WHERE id = ?")
            .bind(sanitize_label(label))
            .bind(id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, SqlxError> {
        let res = sqlx::query("DELETE FROM meeting_bookmarks WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

fn emit_changed(sink: &SharedEventSink, meeting_id: &str) {
    let _ = sink.emit_event(
        BOOKMARKS_CHANGED_EVENT,
        &serde_json::json!({ "meeting_id": meeting_id }),
    );
}

/// Bookmark the current moment of the recording in progress.
pub async fn bookmark_now(
    sink: SharedEventSink,
    pool: Option<SqlitePool>,
    label: Option<String>,
) -> Result<MeetingBookmark, String> {
    let pool = pool.ok_or_else(|| "No database available.".to_string())?;
    let meeting_id = crate::audio::recording_service::current_meeting_id()
        .ok_or_else(|| "Nothing is being recorded.".to_string())?;
    let offset = crate::audio::recording_service::current_offset_seconds()
        .ok_or_else(|| "Nothing is being recorded.".to_string())?;
    let bookmark = BookmarksRepository::add(&pool, &meeting_id, offset, label.as_deref())
        .await
        .map_err(|e| format!("Failed to save bookmark: {e}"))?;
    emit_changed(&sink, &meeting_id);
    Ok(bookmark)
}

/// Rename a bookmark and tell open views.
pub async fn rename_bookmark(
    sink: SharedEventSink,
    pool: SqlitePool,
    meeting_id: String,
    id: String,
    label: String,
) -> Result<(), String> {
    BookmarksRepository::update_label(&pool, &id, &label)
        .await
        .map_err(|e| format!("Failed to rename bookmark: {e}"))?;
    emit_changed(&sink, &meeting_id);
    Ok(())
}

/// Delete a bookmark and tell open views.
pub async fn delete_bookmark(
    sink: SharedEventSink,
    pool: SqlitePool,
    meeting_id: String,
    id: String,
) -> Result<(), String> {
    BookmarksRepository::delete(&pool, &id)
        .await
        .map_err(|e| format!("Failed to delete bookmark: {e}"))?;
    emit_changed(&sink, &meeting_id);
    Ok(())
}

/// The transcript segment a moment at `offset` belongs to.
///
/// `spans` are each segment's `(start, end)` in seconds (either may be
/// unknown). A segment that spans the offset wins (the latest-starting one
/// if several overlap); otherwise the segment whose start or end is closest
/// (the earlier one on a tie). `None` when no segment has a time.
pub fn nearest_segment_index(spans: &[(Option<f64>, Option<f64>)], offset: f64) -> Option<usize> {
    if !offset.is_finite() {
        return None;
    }
    let finite = |v: Option<f64>| v.filter(|x| x.is_finite());

    let mut containing: Option<(usize, f64)> = None;
    let mut nearest: Option<(usize, f64)> = None;
    for (ix, (start, end)) in spans.iter().enumerate() {
        let (start, end) = (finite(*start), finite(*end));
        let Some(anchor) = start.or(end) else { continue };
        let lo = start.unwrap_or(anchor);
        let hi = end.filter(|e| *e >= lo).unwrap_or(lo);
        if lo <= offset && offset <= hi && containing.is_none_or(|(_, best)| lo > best) {
            containing = Some((ix, lo));
        }
        let distance = if offset < lo {
            lo - offset
        } else if offset > hi {
            offset - hi
        } else {
            0.0
        };
        if nearest.is_none_or(|(_, best)| distance < best) {
            nearest = Some((ix, distance));
        }
    }
    containing.or(nearest).map(|(ix, _)| ix)
}

/// [`nearest_segment_index`] over a meeting's transcript rows.
pub fn nearest_transcript_index(transcripts: &[MeetingTranscript], offset: f64) -> Option<usize> {
    let spans: Vec<(Option<f64>, Option<f64>)> = transcripts
        .iter()
        .map(|t| (t.audio_start_time, t.audio_end_time))
        .collect();
    nearest_segment_index(&spans, offset)
}

fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= CONTEXT_EXCERPT_CHARS {
        flat
    } else {
        let cut: String = flat.chars().take(CONTEXT_EXCERPT_CHARS).collect();
        format!("{}…", cut.trim_end())
    }
}

/// The note the summary prompt carries so bookmarked moments get emphasis:
/// one `[mm:ss]` line per bookmark with its label and what was being said.
/// `None` when there are no bookmarks.
pub fn bookmark_context(
    bookmarks: &[MeetingBookmark],
    transcripts: &[MeetingTranscript],
) -> Option<String> {
    if bookmarks.is_empty() {
        return None;
    }
    let mut out = String::from(
        "The user bookmarked these moments during the meeting as important. \
         Give them extra emphasis in the summary:\n",
    );
    for bookmark in bookmarks.iter().take(MAX_CONTEXT_BOOKMARKS) {
        out.push_str("- ");
        out.push_str(&format_timestamp(bookmark.offset_seconds));
        if let Some(label) = bookmark.label.as_deref().and_then(sanitize_label) {
            out.push_str(&format!(" \"{label}\""));
        }
        if let Some(ix) = nearest_transcript_index(transcripts, bookmark.offset_seconds) {
            let said = excerpt(&transcripts[ix].text);
            if !said.is_empty() {
                out.push_str(&format!(" — near: {said}"));
            }
        }
        out.push('\n');
    }
    Some(out.trim_end().to_string())
}

/// Load `meeting_id`'s bookmarks and transcript and build
/// [`bookmark_context`]. Best-effort: any DB error just means no context.
pub async fn load_bookmark_context(pool: &SqlitePool, meeting_id: &str) -> Option<String> {
    let bookmarks = BookmarksRepository::list_by_meeting(pool, meeting_id).await.ok()?;
    if bookmarks.is_empty() {
        return None;
    }
    let transcripts = crate::database::repositories::meeting::MeetingsRepository::get_meeting(
        pool, meeting_id,
    )
    .await
    .ok()
    .flatten()
    .map(|d| d.transcripts)
    .unwrap_or_default();
    bookmark_context(&bookmarks, &transcripts)
}

/// `custom_prompt` with the meeting's bookmark context appended (unchanged
/// when there are no bookmarks). The summary service's single hook.
pub async fn with_bookmark_context(
    pool: &SqlitePool,
    meeting_id: &str,
    custom_prompt: String,
) -> String {
    match load_bookmark_context(pool, meeting_id).await {
        Some(context) if custom_prompt.trim().is_empty() => context,
        Some(context) => format!("{}\n\n{context}", custom_prompt.trim_end()),
        None => custom_prompt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::meeting::MeetingsRepository;
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
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn seg(id: &str, start: Option<f64>, end: Option<f64>, text: &str) -> MeetingTranscript {
        MeetingTranscript {
            id: id.into(),
            text: text.into(),
            timestamp: "x".into(),
            audio_start_time: start,
            audio_end_time: end,
            duration: None,
            speaker: None,
            voice_profile_id: None,
            source: None,
            confidence: None,
        }
    }

    #[tokio::test]
    async fn add_list_rename_delete() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None).await.unwrap();
        MeetingsRepository::create_recording_meeting(&pool, "m2", "B", None).await.unwrap();

        let late = BookmarksRepository::add(&pool, "m1", 90.5, Some("  Budget\n call ")).await.unwrap();
        assert_eq!(late.label.as_deref(), Some("Budget call"));
        let early = BookmarksRepository::add(&pool, "m1", 12.0, None).await.unwrap();
        BookmarksRepository::add(&pool, "m2", 1.0, Some("other")).await.unwrap();

        let list = BookmarksRepository::list_by_meeting(&pool, "m1").await.unwrap();
        assert_eq!(list.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), vec![early.id.as_str(), late.id.as_str()]);
        assert_eq!(list[0].label, None);

        assert!(BookmarksRepository::update_label(&pool, &early.id, "Decision").await.unwrap());
        assert!(BookmarksRepository::update_label(&pool, &late.id, "   ").await.unwrap());
        let list = BookmarksRepository::list_by_meeting(&pool, "m1").await.unwrap();
        assert_eq!(list[0].label.as_deref(), Some("Decision"));
        assert_eq!(list[1].label, None);
        assert!(!BookmarksRepository::update_label(&pool, "missing", "x").await.unwrap());

        assert!(BookmarksRepository::delete(&pool, &early.id).await.unwrap());
        assert!(!BookmarksRepository::delete(&pool, &early.id).await.unwrap());
        assert_eq!(BookmarksRepository::list_by_meeting(&pool, "m1").await.unwrap().len(), 1);
        assert_eq!(BookmarksRepository::list_by_meeting(&pool, "m2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_bad_offsets() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None).await.unwrap();
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(BookmarksRepository::add(&pool, "m1", bad, None).await.is_err());
        }
        // Unknown meeting: FK violation.
        assert!(BookmarksRepository::add(&pool, "nope", 1.0, None).await.is_err());
    }

    #[tokio::test]
    async fn deleting_meeting_removes_bookmarks() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None).await.unwrap();
        MeetingsRepository::create_recording_meeting(&pool, "m2", "B", None).await.unwrap();
        BookmarksRepository::add(&pool, "m1", 1.0, None).await.unwrap();
        BookmarksRepository::add(&pool, "m2", 1.0, None).await.unwrap();
        assert!(MeetingsRepository::delete_meeting(&pool, "m1").await.unwrap());
        assert!(BookmarksRepository::list_by_meeting(&pool, "m1").await.unwrap().is_empty());
        sqlx::query("DELETE FROM meetings WHERE id = 'm2'").execute(&pool).await.unwrap();
        assert!(BookmarksRepository::list_by_meeting(&pool, "m2").await.unwrap().is_empty());
    }

    #[test]
    fn label_sanitising() {
        assert_eq!(sanitize_label("  hi \t there\u{0007} "), Some("hi there".into()));
        assert_eq!(sanitize_label("\n\r "), None);
        let long = "x".repeat(500);
        assert_eq!(sanitize_label(&long).unwrap().chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn maps_bookmark_to_containing_segment() {
        let spans = [
            (Some(0.0), Some(4.0)),
            (Some(5.0), Some(9.0)),
            (Some(10.0), Some(20.0)),
        ];
        assert_eq!(nearest_segment_index(&spans, 0.0), Some(0));
        assert_eq!(nearest_segment_index(&spans, 6.5), Some(1));
        assert_eq!(nearest_segment_index(&spans, 20.0), Some(2));
    }

    #[test]
    fn maps_bookmark_in_gap_to_closest_segment() {
        let spans = [(Some(0.0), Some(4.0)), (Some(10.0), Some(12.0))];
        assert_eq!(nearest_segment_index(&spans, 5.0), Some(0));
        assert_eq!(nearest_segment_index(&spans, 9.0), Some(1));
        // Equidistant: the earlier segment.
        assert_eq!(nearest_segment_index(&spans, 7.0), Some(0));
        // Past the end / before the start.
        assert_eq!(nearest_segment_index(&spans, 500.0), Some(1));
        let late = [(Some(30.0), Some(40.0))];
        assert_eq!(nearest_segment_index(&late, 1.0), Some(0));
    }

    #[test]
    fn mapping_handles_overlap_unsorted_and_missing_times() {
        // Overlapping mic/system segments: the later-starting one wins.
        let spans = [(Some(10.0), Some(30.0)), (Some(0.0), Some(5.0)), (Some(15.0), Some(25.0))];
        assert_eq!(nearest_segment_index(&spans, 20.0), Some(2));
        // Untimed rows are skipped; start-only rows act as points.
        let spans = [(None, None), (Some(8.0), None), (Some(f64::NAN), Some(2.0))];
        assert_eq!(nearest_segment_index(&spans, 7.0), Some(1));
        assert_eq!(nearest_segment_index(&spans, 1.0), Some(2));
        assert_eq!(nearest_segment_index(&[(None, None)], 1.0), None);
        assert_eq!(nearest_segment_index(&[], 1.0), None);
        assert_eq!(nearest_segment_index(&[(Some(1.0), Some(2.0))], f64::NAN), None);
    }

    #[test]
    fn context_lists_bookmarks_with_excerpts() {
        let transcripts = vec![
            seg("a", Some(0.0), Some(5.0), "Hello everyone."),
            seg("b", Some(60.0), Some(70.0), "We agreed to   ship on Friday."),
        ];
        let bookmark = |offset: f64, label: Option<&str>| MeetingBookmark {
            id: "x".into(),
            meeting_id: "m".into(),
            offset_seconds: offset,
            label: label.map(str::to_string),
            created_at: "x".into(),
        };
        assert_eq!(bookmark_context(&[], &transcripts), None);
        let text = bookmark_context(
            &[bookmark(65.2, Some("Ship date")), bookmark(2.0, None)],
            &transcripts,
        )
        .unwrap();
        assert!(text.contains("[01:05] \"Ship date\" — near: We agreed to ship on Friday."), "{text}");
        assert!(text.contains("[00:02] — near: Hello everyone."), "{text}");
        // No transcript: still lists the moments.
        assert!(bookmark_context(&[bookmark(3.0, None)], &[]).unwrap().contains("[00:03]"));
    }

    #[tokio::test]
    async fn custom_prompt_gets_context_appended() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None).await.unwrap();
        assert_eq!(with_bookmark_context(&pool, "m1", "Be brief".into()).await, "Be brief");
        BookmarksRepository::add(&pool, "m1", 61.0, Some("Key point")).await.unwrap();
        let out = with_bookmark_context(&pool, "m1", "Be brief".into()).await;
        assert!(out.starts_with("Be brief\n\nThe user bookmarked"), "{out}");
        assert!(out.contains("[01:01] \"Key point\""), "{out}");
        let out = with_bookmark_context(&pool, "m1", String::new()).await;
        assert!(out.starts_with("The user bookmarked"), "{out}");
    }
}
