//! LLM chapters: split a meeting into titled topic sections (issue #87).
//!
//! [`generate_chapters`] sends the transcript, as compact `[mm:ss] Speaker:
//! text` lines, to the provider/model configured for summaries (resolved
//! the same way `chat` does). Long meetings are cut into chunks that fit the
//! model's window; each chunk is chaptered on its own and the results are
//! merged. The reply is untrusted text and goes through [`parse`]'s strict
//! line parser — anything out of range, out of order or not a short plain
//! title is dropped — before replacing the meeting's rows in
//! `meeting_chapters`. A failed or empty run leaves existing chapters alone.
//!
//! [`maybe_auto_chapter`] runs this after a recording, behind the same
//! "auto-generate summary" switch that already opts the user into
//! post-meeting LLM calls.

pub mod parse;

use chrono::{SecondsFormat, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use sqlx::{Error as SqlxError, FromRow, SqlitePool};
use std::collections::HashSet;
use std::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::chat::context::{chunk_segments, format_timestamp, ContextSegment};
use crate::database::models::Transcript;
use crate::database::repositories::setting::{SettingsRepository, KEY_UI_CONFIG};
use crate::events::{EventSinkExt, SharedEventSink};
use crate::summary::llm_client::{generate_summary, LlmConfig};
use crate::summary::processor::rough_token_count;
use parse::{is_valid_chapter_list, merge_chapters, parse_chapters, ChapterDraft, ChapterLimits};

/// Emitted when a meeting's chapters change or a run starts/ends:
/// `{ "meeting_id": String, "status": "running" | "done" | "failed", "error"?: String }`.
pub const CHAPTERS_EVENT: &str = "meeting-chapters";

/// Upper bound on one chunk, in characters, even for huge-context models:
/// small focused chunks give better boundaries than one giant prompt.
const MAX_CHUNK_CHARS: usize = 24_000;
/// Lower bound, so a tiny reported window still makes progress.
const MIN_CHUNK_CHARS: usize = 2_000;
/// Room kept for the prompt scaffolding and the model's reply.
const RESERVED_TOKENS: usize = 1_000;

const SYSTEM_PROMPT: &str = "You divide meeting transcripts into chapters by topic. \
Each transcript line starts with its time as [mm:ss].\n\
Rules:\n\
- Output ONLY chapter lines, one per line, in exactly this format: [mm:ss] Chapter title\n\
- The time is the transcript time where the topic starts, copied exactly from a transcript line.\n\
- Times must increase from one chapter to the next.\n\
- Titles are short plain text (2 to 8 words). No markdown, no links, no quotes, no speaker names.\n\
- A new chapter only where the topic clearly changes; a few minutes per chapter is typical.\n\
- No introduction, explanation or numbering.";

/// One stored chapter.
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct MeetingChapter {
    pub id: String,
    pub meeting_id: String,
    pub position: i64,
    pub start_seconds: f64,
    pub title: String,
    pub created_at: String,
}

pub struct ChaptersRepository;

impl ChaptersRepository {
    /// A meeting's chapters, in order.
    pub async fn list_by_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<MeetingChapter>, SqlxError> {
        sqlx::query_as::<_, MeetingChapter>(
            "SELECT id, meeting_id, position, start_seconds, title, created_at \
             FROM meeting_chapters WHERE meeting_id = ? ORDER BY position ASC",
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    /// Replace a meeting's chapters with `chapters` atomically. Refuses a
    /// list that fails [`is_valid_chapter_list`] against `duration`.
    pub async fn replace(
        pool: &SqlitePool,
        meeting_id: &str,
        chapters: &[ChapterDraft],
        duration: f64,
    ) -> Result<Vec<MeetingChapter>, SqlxError> {
        if !is_valid_chapter_list(chapters, duration) {
            return Err(SqlxError::Protocol("invalid chapter list".to_string()));
        }
        let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let rows: Vec<MeetingChapter> = chapters
            .iter()
            .enumerate()
            .map(|(ix, c)| MeetingChapter {
                id: format!("ch-{}", Uuid::new_v4()),
                meeting_id: meeting_id.to_string(),
                position: ix as i64,
                start_seconds: c.start_seconds,
                title: c.title.clone(),
                created_at: created_at.clone(),
            })
            .collect();

        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM meeting_chapters WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(&mut *tx)
            .await?;
        for row in &rows {
            sqlx::query(
                "INSERT INTO meeting_chapters (id, meeting_id, position, start_seconds, title, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&row.id)
            .bind(&row.meeting_id)
            .bind(row.position)
            .bind(row.start_seconds)
            .bind(&row.title)
            .bind(&row.created_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(rows)
    }

    /// Drop a meeting's chapters. Returns rows removed.
    pub async fn clear(pool: &SqlitePool, meeting_id: &str) -> Result<u64, SqlxError> {
        let res = sqlx::query("DELETE FROM meeting_chapters WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected())
    }
}

/// Meetings with a chapter run in flight (one run per meeting at a time).
static IN_FLIGHT: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Removes the meeting from [`IN_FLIGHT`] however the run ends.
struct InFlightGuard(String);

impl InFlightGuard {
    fn acquire(meeting_id: &str) -> Option<Self> {
        let mut set = IN_FLIGHT.lock().ok()?;
        set.insert(meeting_id.to_string())
            .then(|| Self(meeting_id.to_string()))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = IN_FLIGHT.lock() {
            set.remove(&self.0);
        }
    }
}

/// True while chapters are being generated for `meeting_id`.
pub fn is_generating(meeting_id: &str) -> bool {
    IN_FLIGHT
        .lock()
        .map(|s| s.contains(meeting_id))
        .unwrap_or(false)
}

/// The transcript as the chapterer sees it, plus what validation needs.
#[derive(Debug, Clone, Default)]
pub struct ChapterInput {
    pub segments: Vec<ContextSegment>,
    /// End of the last segment (or the stored meeting duration if longer).
    pub duration: f64,
    pub speakers: Vec<String>,
}

/// Load a meeting's timed, non-empty segments in recording order.
pub async fn load_input(pool: &SqlitePool, meeting_id: &str) -> Result<ChapterInput, String> {
    let rows = sqlx::query_as::<_, Transcript>(
        "SELECT * FROM transcripts WHERE meeting_id = ? \
         ORDER BY COALESCE(audio_start_time, 0.0), sequence_id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("failed to load transcript: {e}"))?;
    let stored_duration: Option<(Option<f64>,)> =
        sqlx::query_as("SELECT duration_seconds FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("failed to load meeting: {e}"))?;
    let Some((stored_duration,)) = stored_duration else {
        return Err("Meeting not found".to_string());
    };
    Ok(build_input(rows, stored_duration))
}

fn build_input(rows: Vec<Transcript>, stored_duration: Option<f64>) -> ChapterInput {
    let mut duration = stored_duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(0.0);
    let mut speakers: Vec<String> = Vec::new();
    let mut segments = Vec::new();
    for t in rows {
        let Some(start) = t.audio_start_time.filter(|s| s.is_finite() && *s >= 0.0) else {
            continue;
        };
        let text = t
            .transcript
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        let end = t
            .audio_end_time
            .filter(|e| e.is_finite() && *e >= start)
            .unwrap_or(start);
        duration = duration.max(end);
        let speaker = t
            .speaker
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Speaker".to_string());
        if !speakers.contains(&speaker) {
            speakers.push(speaker.clone());
        }
        segments.push(ContextSegment {
            start: Some(start),
            speaker,
            text,
        });
    }
    ChapterInput {
        segments,
        duration,
        speakers,
    }
}

/// One model call's worth of transcript and the time range it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterChunk {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// Chunk size, in characters, for a model whose single-pass budget is
/// `token_threshold` rough tokens.
pub fn chunk_chars_for(token_threshold: usize) -> usize {
    let tokens = token_threshold.saturating_sub(RESERVED_TOKENS);
    // `rough_token_count` charges 0.35 tokens per ASCII char.
    ((tokens as f64 / 0.35) as usize).clamp(MIN_CHUNK_CHARS, MAX_CHUNK_CHARS)
}

/// Cut the transcript into chunks of at most ~`max_chars`. Each chunk
/// covers from its first segment's start to the next chunk's start (the
/// last runs to `duration`).
pub fn build_chunks(input: &ChapterInput, max_chars: usize) -> Vec<ChapterChunk> {
    let raw = chunk_segments(&input.segments, max_chars);
    let starts: Vec<f64> = raw
        .iter()
        .map(|c| input.segments[c.first_segment].start.unwrap_or(0.0))
        .collect();
    raw.into_iter()
        .enumerate()
        .map(|(ix, c)| ChapterChunk {
            text: c.text,
            start: starts[ix],
            end: starts
                .get(ix + 1)
                .copied()
                .unwrap_or(input.duration)
                .max(starts[ix]),
        })
        .collect()
}

fn user_prompt(chunk: &ChapterChunk, part: usize, parts: usize) -> String {
    let scope = if parts > 1 {
        format!(
            "This is part {part} of {parts} of a longer meeting, covering {} to {}. \
             Only give chapters that start within this part.\n\n",
            format_timestamp(chunk.start),
            format_timestamp(chunk.end)
        )
    } else {
        String::new()
    };
    format!(
        "{scope}<transcript>\n{}\n</transcript>\n\n\
         List the chapters, one per line, as: [mm:ss] Chapter title",
        chunk.text
    )
}

fn emit(sink: &SharedEventSink, meeting_id: &str, status: &str, error: Option<&str>) {
    let mut payload = serde_json::json!({ "meeting_id": meeting_id, "status": status });
    if let Some(e) = error {
        payload["error"] = serde_json::Value::String(e.to_string());
    }
    let _ = sink.emit_event(CHAPTERS_EVENT, &payload);
}

/// Generate and store chapters for `meeting_id` with the configured summary
/// model. Emits [`CHAPTERS_EVENT`] when it starts and ends.
pub async fn generate_chapters(
    sink: SharedEventSink,
    pool: SqlitePool,
    meeting_id: String,
) -> Result<Vec<MeetingChapter>, String> {
    let Some(_guard) = InFlightGuard::acquire(&meeting_id) else {
        return Err("Chapters are already being generated for this meeting.".to_string());
    };
    emit(&sink, &meeting_id, "running", None);
    let result = run(&pool, &meeting_id).await;
    match &result {
        Ok(chapters) => {
            info!("Chapters for {meeting_id}: {} stored", chapters.len());
            emit(&sink, &meeting_id, "done", None);
        }
        Err(e) => {
            warn!("Chapters for {meeting_id} failed: {e}");
            emit(&sink, &meeting_id, "failed", Some(e));
        }
    }
    result
}

async fn run(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<MeetingChapter>, String> {
    let model = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| format!("failed to read model settings: {e}"))?
        .filter(|m| !m.provider.is_empty() && !m.model.is_empty())
        .ok_or_else(|| "No model configured — set one up in Settings first.".to_string())?;
    let input = load_input(pool, meeting_id).await?;
    if input.segments.is_empty() {
        return Err("This meeting has no timed transcript to chapter.".to_string());
    }
    let config = LlmConfig::resolve(
        pool,
        &model.provider,
        &model.model,
        crate::paths::app_data_dir().ok(),
    )
    .await?;
    let threshold = config.token_threshold().await;
    let chunks = build_chunks(&input, chunk_chars_for(threshold));
    info!(
        "Chapters for {meeting_id}: provider={} model={} chunks={} (~{} tokens)",
        model.provider,
        model.model,
        chunks.len(),
        chunks
            .iter()
            .map(|c| rough_token_count(&c.text))
            .sum::<usize>()
    );

    let mut parts = Vec::with_capacity(chunks.len());
    for (ix, chunk) in chunks.iter().enumerate() {
        let reply = generate_summary(
            &config,
            SYSTEM_PROMPT,
            &user_prompt(chunk, ix + 1, chunks.len()),
            None,
            None,
        )
        .await?;
        let limits = ChapterLimits {
            min_start: chunk.start.floor(),
            max_start: chunk.end.min(input.duration),
            speakers: input.speakers.clone(),
        };
        let parsed = parse_chapters(&reply, &limits);
        if parsed.is_empty() {
            warn!(
                "Chapters for {meeting_id}: part {} gave no valid chapter lines",
                ix + 1
            );
        }
        parts.push(parsed);
    }

    let chapters = merge_chapters(parts);
    if chapters.is_empty() {
        return Err("The model didn't return any usable chapters.".to_string());
    }
    ChaptersRepository::replace(pool, meeting_id, &chapters, input.duration)
        .await
        .map_err(|e| format!("failed to save chapters: {e}"))
}

fn is_auto_enabled(ui_config: Option<&str>) -> bool {
    ui_config
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| v.get("isAutoSummary").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// After a recording: chapter the meeting when post-meeting AI is switched
/// on (Settings → Summary → auto-generate) and it has no chapters yet.
/// Best-effort; failures are logged and emitted, never raised.
pub async fn maybe_auto_chapter(
    sink: SharedEventSink,
    pool: Option<SqlitePool>,
    meeting_id: String,
) {
    let Some(pool) = pool else { return };
    let ui_config = SettingsRepository::get_setting_json(&pool, KEY_UI_CONFIG)
        .await
        .ok()
        .flatten();
    if !is_auto_enabled(ui_config.as_deref()) {
        return;
    }
    match ChaptersRepository::list_by_meeting(&pool, &meeting_id).await {
        Ok(existing) if existing.is_empty() => {}
        _ => return,
    }
    let _ = generate_chapters(sink, pool, meeting_id).await;
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

    fn draft(s: f64, t: &str) -> ChapterDraft {
        ChapterDraft {
            start_seconds: s,
            title: t.into(),
        }
    }

    #[tokio::test]
    async fn replace_list_and_clear() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None)
            .await
            .unwrap();
        MeetingsRepository::create_recording_meeting(&pool, "m2", "B", None)
            .await
            .unwrap();

        let stored = ChaptersRepository::replace(
            &pool,
            "m1",
            &[draft(0.0, "Intro"), draft(120.0, "Budget")],
            300.0,
        )
        .await
        .unwrap();
        assert_eq!(stored.len(), 2);
        ChaptersRepository::replace(&pool, "m2", &[draft(0.0, "Other")], 10.0)
            .await
            .unwrap();

        let list = ChaptersRepository::list_by_meeting(&pool, "m1")
            .await
            .unwrap();
        assert_eq!(
            list.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["Intro", "Budget"]
        );
        assert_eq!(list[1].position, 1);
        assert_eq!(list[1].start_seconds, 120.0);

        // Replacing swaps the whole set.
        ChaptersRepository::replace(&pool, "m1", &[draft(5.0, "Only")], 300.0)
            .await
            .unwrap();
        let list = ChaptersRepository::list_by_meeting(&pool, "m1")
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Only");

        assert_eq!(ChaptersRepository::clear(&pool, "m1").await.unwrap(), 1);
        assert!(ChaptersRepository::list_by_meeting(&pool, "m1")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            ChaptersRepository::list_by_meeting(&pool, "m2")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn replace_refuses_invalid_lists_and_keeps_old_rows() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None)
            .await
            .unwrap();
        ChaptersRepository::replace(&pool, "m1", &[draft(0.0, "Keep")], 100.0)
            .await
            .unwrap();
        for bad in [
            vec![draft(50.0, "B"), draft(10.0, "A")],
            vec![draft(500.0, "Past end")],
            vec![draft(1.0, "[link](https://x.y)")],
            vec![draft(f64::NAN, "Nan")],
        ] {
            assert!(ChaptersRepository::replace(&pool, "m1", &bad, 100.0)
                .await
                .is_err());
        }
        let list = ChaptersRepository::list_by_meeting(&pool, "m1")
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Keep");
    }

    #[tokio::test]
    async fn deleting_meeting_removes_chapters() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None)
            .await
            .unwrap();
        MeetingsRepository::create_recording_meeting(&pool, "m2", "B", None)
            .await
            .unwrap();
        ChaptersRepository::replace(&pool, "m1", &[draft(0.0, "A")], 10.0)
            .await
            .unwrap();
        ChaptersRepository::replace(&pool, "m2", &[draft(0.0, "B")], 10.0)
            .await
            .unwrap();
        assert!(MeetingsRepository::delete_meeting(&pool, "m1")
            .await
            .unwrap());
        assert!(ChaptersRepository::list_by_meeting(&pool, "m1")
            .await
            .unwrap()
            .is_empty());
        sqlx::query("DELETE FROM meetings WHERE id = 'm2'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(ChaptersRepository::list_by_meeting(&pool, "m2")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn loads_timed_segments_and_duration() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, speaker, sequence_id)
             VALUES ('t2', 'm1', 'Second  part.', 'x', 60.0, 75.5, 'Bob', 2),
                    ('t1', 'm1', 'First.', 'x', 0.0, 4.0, NULL, 1),
                    ('t3', 'm1', '   ', 'x', 80.0, 90.0, 'Bob', 3),
                    ('t4', 'm1', 'Untimed.', 'x', NULL, NULL, 'Bob', 4)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let input = load_input(&pool, "m1").await.unwrap();
        assert_eq!(input.segments.len(), 2);
        assert_eq!(input.segments[0].text, "First.");
        assert_eq!(input.segments[1].text, "Second part.");
        assert_eq!(input.duration, 75.5);
        assert_eq!(
            input.speakers,
            vec!["Speaker".to_string(), "Bob".to_string()]
        );
        assert!(load_input(&pool, "missing").await.is_err());
    }

    #[test]
    fn chunks_cover_the_meeting_in_order() {
        let segments: Vec<ContextSegment> = (0..40)
            .map(|i| ContextSegment {
                start: Some(i as f64 * 30.0),
                speaker: "A".into(),
                text: format!("Sentence number {i} about something."),
            })
            .collect();
        let input = ChapterInput {
            segments,
            duration: 1210.0,
            speakers: vec!["A".into()],
        };
        let chunks = build_chunks(&input, 400);
        assert!(chunks.len() > 1);
        assert_eq!(chunks[0].start, 0.0);
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
            assert!(pair[0].start < pair[1].start);
        }
        assert_eq!(chunks.last().unwrap().end, 1210.0);
        assert!(chunks.iter().all(|c| c.text.starts_with('[')));

        let single = build_chunks(&input, 1_000_000);
        assert_eq!(single.len(), 1);
        assert!(user_prompt(&single[0], 1, 1).starts_with("<transcript>"));
        assert!(user_prompt(&chunks[1], 2, chunks.len()).contains("part 2 of"));
    }

    #[test]
    fn chunk_size_is_clamped() {
        assert_eq!(chunk_chars_for(0), MIN_CHUNK_CHARS);
        assert_eq!(chunk_chars_for(1_000_000), MAX_CHUNK_CHARS);
        let mid = chunk_chars_for(4_000);
        assert!(mid > MIN_CHUNK_CHARS && mid < MAX_CHUNK_CHARS);
    }

    #[tokio::test]
    async fn generate_without_model_config_errors_and_keeps_rows() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "A", None)
            .await
            .unwrap();
        ChaptersRepository::replace(&pool, "m1", &[draft(0.0, "Keep")], 10.0)
            .await
            .unwrap();
        let sink = std::sync::Arc::new(crate::events::RecordingSink::default());
        let shared: SharedEventSink = sink.clone();
        let err = generate_chapters(shared, pool.clone(), "m1".into())
            .await
            .unwrap_err();
        assert!(err.contains("No model configured"), "{err}");
        assert!(!is_generating("m1"));
        assert_eq!(
            ChaptersRepository::list_by_meeting(&pool, "m1")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn auto_flag() {
        assert!(is_auto_enabled(Some(r#"{"isAutoSummary":true}"#)));
        assert!(!is_auto_enabled(Some(r#"{"isAutoSummary":false}"#)));
        assert!(!is_auto_enabled(Some("not json")));
        assert!(!is_auto_enabled(None));
    }
}
