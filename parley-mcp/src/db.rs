//! Database access: connection setup, row types, queries, and the pure
//! helpers that shape rows into agent-facing values.
//!
//! # Concurrency
//!
//! The Parley desktop app may have this same SQLite file open while we run.
//! Two things make that safe:
//!
//! * **WAL journal mode** — readers never block the writer and vice versa, so
//!   our reads can't stall a recording in progress.
//! * **`busy_timeout`** — when we *do* need the write lock and the app holds
//!   it, SQLite blocks and retries internally instead of instantly returning
//!   `SQLITE_BUSY`. On top of that, [`with_write_retry`] retries a write a few
//!   times with backoff, which covers the one case `busy_timeout` doesn't:
//!   `SQLITE_BUSY_SNAPSHOT` on a write that began as a read transaction.
//!
//! We never run migrations and never create the file — the app owns the
//! schema. `create_if_missing(false)` enforces that.
//!
//! # Timestamps
//!
//! Timestamp columns are TEXT, but the app has written them in two shapes over
//! its lifetime: `to_rfc3339()` output (`2026-07-15T09:30:00+00:00`) and sqlx's
//! own `DateTime<Utc>` encoding (`2026-07-15 09:30:00.123456+00:00`). They
//! agree on the leading `YYYY-MM-DD`, and nothing more. So we read timestamps
//! as opaque strings and pass them through untouched, and date filtering
//! compares only that 10-character date prefix (see [`date_prefix_filter`]).

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, Row, SqlitePool};

/// How long SQLite blocks waiting for a lock held by the Parley app before
/// giving up on a statement.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on any `limit` argument, so a careless agent can't ask for the
/// entire transcript corpus in one response.
pub const MAX_LIMIT: u32 = 500;

/// How many meetings `list_resources` advertises. Resource listing is
/// unpaginated here; see the note in `server.rs`.
pub const MAX_RESOURCES: u32 = 100;

/// Open the app database read-write, in WAL mode, tolerant of contention.
pub async fn connect(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        // The app owns creation and migration. If the file isn't there we want
        // a clear error, not an empty database that silently answers every
        // query with "no meetings".
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(BUSY_TIMEOUT)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
}

/// Run a write, retrying briefly if SQLite reports the database is locked.
///
/// `busy_timeout` already handles most contention, but a write that upgrades
/// from a read snapshot can fail immediately with `SQLITE_BUSY_SNAPSHOT`
/// regardless of the timeout. Retrying the whole operation is the documented
/// remedy.
pub async fn with_write_retry<F, Fut, T>(mut op: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    const ATTEMPTS: u32 = 4;
    let mut delay = Duration::from_millis(50);

    for attempt in 1..=ATTEMPTS {
        match op().await {
            Err(e) if attempt < ATTEMPTS && is_locked_error(&e) => {
                tracing::warn!(attempt, error = %e, "database busy, retrying write");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            other => return other,
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// Whether an error is SQLite's "someone else holds the lock" family, which is
/// worth retrying — as opposed to a schema or constraint error, which isn't.
fn is_locked_error(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db) => {
            // `SQLITE_BUSY` = 5, `SQLITE_LOCKED` = 6, plus their extended
            // variants (e.g. `SQLITE_BUSY_SNAPSHOT` = 517), which share the
            // low byte with their primary code.
            matches!(db.code().as_deref(), Some(code) if {
                let primary = code.parse::<u32>().map(|c| c & 0xff);
                matches!(primary, Ok(5) | Ok(6))
            })
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Clamp an agent-supplied limit into `1..=MAX_LIMIT`, defaulting when absent.
pub fn clamp_limit(limit: Option<u32>, default: u32) -> u32 {
    limit.unwrap_or(default).clamp(1, MAX_LIMIT)
}

/// Validate a `YYYY-MM-DD` date filter.
///
/// Deliberately strict: filtering compares the string against the date prefix
/// of a timestamp column, so anything longer or differently shaped would
/// compare against the *wrong characters* and silently return nonsense rather
/// than fail. Rejecting it up front is the only way the agent learns.
pub fn validate_date(value: &str, field: &str) -> Result<String, String> {
    let trimmed = value.trim();
    let ok = trimmed.len() == 10
        && trimmed.as_bytes()[4] == b'-'
        && trimmed.as_bytes()[7] == b'-'
        && trimmed
            .as_bytes()
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit());

    if !ok {
        return Err(format!(
            "`{field}` must be a date in YYYY-MM-DD form (got {value:?}). \
             Timestamps are UTC."
        ));
    }
    Ok(trimmed.to_string())
}

/// The SQL fragment used for date filtering. See the module-level note on
/// timestamp formats for why this compares a substring rather than using
/// `date()` or a plain `>=`.
pub fn date_prefix_filter(column: &str) -> String {
    format!("substr({column}, 1, 10)")
}

/// Validate an action-item status against the two the schema allows.
pub fn validate_status(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "open" => Ok("open".to_string()),
        "done" => Ok("done".to_string()),
        other => Err(format!(
            "`status` must be \"open\" or \"done\" (got {other:?})."
        )),
    }
}

/// Escape LIKE wildcards in user input so a query containing `%` or `_`
/// searches for those literal characters. Pair with `ESCAPE '\'` in the SQL.
pub fn escape_like(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 8);
    for ch in query.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Extract a window of text around the first case-insensitive hit of `query`,
/// with ellipses where it was cut. Falls back to a head-truncation when the
/// query isn't found (which happens for a `%`-escaped search that matched a
/// different row than the snippet's).
pub fn make_snippet(text: &str, query: &str, radius: usize) -> String {
    let haystack = text.to_lowercase();
    let needle = query.to_lowercase();

    let hit = haystack.find(&needle).unwrap_or(0);
    // Char-boundary-safe window: work in chars, not bytes, so multi-byte text
    // can't panic the slice.
    let chars: Vec<char> = text.chars().collect();
    let hit_char = text[..hit].chars().count();

    let start = hit_char.saturating_sub(radius);
    let end = (hit_char + needle.chars().count() + radius).min(chars.len());

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(&chars[start..end]);
    if end < chars.len() {
        snippet.push('…');
    }
    snippet
}

/// Human label for a transcript segment's speaker.
///
/// `voice_profile_name` is the enrolled person (best), `speaker` is the raw
/// audio-source or diarizer label (`mic`/`system`/`Speaker 1`), and neither
/// being present means we genuinely don't know.
pub fn speaker_label(speaker: Option<&str>, voice_profile_name: Option<&str>) -> String {
    if let Some(name) = voice_profile_name.filter(|n| !n.trim().is_empty()) {
        return name.to_string();
    }
    match speaker.map(str::trim).filter(|s| !s.is_empty()) {
        Some("mic") => "You (microphone)".to_string(),
        Some("system") => "Others (system audio)".to_string(),
        Some(other) => other.to_string(),
        None => "Unknown".to_string(),
    }
}

/// Format seconds-from-recording-start as `HH:MM:SS`.
pub fn format_offset(seconds: Option<f64>) -> Option<String> {
    let s = seconds?;
    if !s.is_finite() || s < 0.0 {
        return None;
    }
    let total = s as u64;
    Some(format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    ))
}

/// Render ordered segments as plain text, optionally attributed.
pub fn render_transcript(segments: &[TranscriptSegment], include_speakers: bool) -> String {
    let mut out = String::new();
    let mut last_speaker: Option<&str> = None;

    for seg in segments {
        if include_speakers {
            // Only re-print the speaker when it changes — a transcript where
            // every line repeats "You (microphone):" is mostly noise.
            let changed = last_speaker != Some(seg.speaker_label.as_str());
            if changed {
                if !out.is_empty() {
                    out.push('\n');
                }
                match &seg.start {
                    Some(t) => out.push_str(&format!("[{t}] {}: ", seg.speaker_label)),
                    None => out.push_str(&format!("{}: ", seg.speaker_label)),
                }
                last_speaker = Some(seg.speaker_label.as_str());
            } else {
                out.push(' ');
            }
            out.push_str(seg.text.trim());
        } else {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(seg.text.trim());
        }
    }
    out
}

/// The resource URI for a meeting.
pub fn meeting_resource_uri(meeting_id: &str) -> String {
    format!("parley://meeting/{meeting_id}")
}

/// Inverse of [`meeting_resource_uri`]. Also accepts the pre-rename
/// `meetily://` scheme, which clients may still hold from earlier sessions.
pub fn parse_meeting_resource_uri(uri: &str) -> Option<&str> {
    uri.strip_prefix("parley://meeting/")
        .or_else(|| uri.strip_prefix("meetily://meeting/"))
        .filter(|id| !id.is_empty())
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// One row of `list_meetings`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeetingListItem {
    pub id: String,
    pub title: String,
    /// UTC, as stored. Format varies; see the module note.
    pub created_at: String,
    pub has_summary: bool,
    pub open_action_item_count: i64,
}

/// Full detail for a single meeting.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeetingDetail {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub folder_path: Option<String>,
    pub calendar_event_id: Option<String>,
    /// The generated summary in markdown, or null if none exists yet.
    pub summary: Option<String>,
    /// Status of the summary generation process, if one has ever run.
    pub summary_status: Option<String>,
    pub transcript_segment_count: i64,
    pub open_action_item_count: i64,
    pub done_action_item_count: i64,
    pub note_count: i64,
}

/// One ordered transcript segment.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TranscriptSegment {
    pub text: String,
    /// Resolved speaker name; see [`speaker_label`].
    pub speaker_label: String,
    /// Raw source label as stored (`mic`, `system`, a diarizer label, …).
    pub speaker: Option<String>,
    /// `HH:MM:SS` from the start of the recording, when known.
    pub start: Option<String>,
    pub audio_start_time: Option<f64>,
    pub audio_end_time: Option<f64>,
}

/// One `search_transcripts` hit.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TranscriptHit {
    pub meeting_id: String,
    pub meeting_title: String,
    pub meeting_created_at: String,
    pub snippet: String,
    pub speaker_label: String,
    pub start: Option<String>,
}

/// An action item.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ActionItem {
    pub id: String,
    pub meeting_id: String,
    pub meeting_title: String,
    pub text: String,
    pub assignee: Option<String>,
    /// Free-text due hint as spoken ("by Friday"), not a parsed date.
    pub due_hint: Option<String>,
    pub status: String,
    /// `summary` (auto-extracted), `manual` (user), or `agent` (via this server).
    pub source: String,
    pub external_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

/// A meeting's summary, as returned by `get_recent_summaries`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeetingSummary {
    pub meeting_id: String,
    pub title: String,
    pub created_at: String,
    pub summary_updated_at: String,
    pub summary: String,
}

/// A note attached to a meeting.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeetingNote {
    pub id: String,
    pub meeting_id: String,
    pub body: String,
    pub source: String,
    pub created_at: String,
}

/// Minimal meeting identity, for resource listing.
#[derive(Debug, FromRow)]
pub struct MeetingRef {
    pub id: String,
    pub title: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// `created_at`-descending list of meetings with summary/action-item rollups.
pub async fn list_meetings(
    pool: &SqlitePool,
    limit: u32,
    since: Option<&str>,
    until: Option<&str>,
    title_query: Option<&str>,
) -> Result<Vec<MeetingListItem>, sqlx::Error> {
    let date_expr = date_prefix_filter("m.created_at");
    let mut sql = String::from(
        r#"
        SELECT
            m.id,
            m.title,
            m.created_at,
            EXISTS(
                SELECT 1 FROM summary_processes sp
                WHERE sp.meeting_id = m.id
                  AND sp.result IS NOT NULL AND sp.result != ''
            ) AS has_summary,
            (
                SELECT COUNT(*) FROM action_items ai
                WHERE ai.meeting_id = m.id AND ai.status = 'open'
            ) AS open_action_item_count
        FROM meetings m
        WHERE 1 = 1
        "#
    );
    if since.is_some() {
        sql.push_str(&format!(" AND {date_expr} >= ?"));
    }
    if until.is_some() {
        sql.push_str(&format!(" AND {date_expr} <= ?"));
    }
    if title_query.is_some() {
        sql.push_str(" AND m.title LIKE ? ESCAPE '\\'");
    }
    sql.push_str(" ORDER BY m.created_at DESC LIMIT ?");

    let mut q = sqlx::query(&sql);
    if let Some(v) = since {
        q = q.bind(v);
    }
    if let Some(v) = until {
        q = q.bind(v);
    }
    if let Some(v) = title_query {
        q = q.bind(format!("%{}%", escape_like(v)));
    }
    q = q.bind(limit);

    let rows = q.fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok(MeetingListItem {
                id: r.try_get("id")?,
                title: r.try_get("title")?,
                created_at: r.try_get("created_at")?,
                has_summary: r.try_get::<i64, _>("has_summary")? != 0,
                open_action_item_count: r.try_get("open_action_item_count")?,
            })
        })
        .collect()
}

/// Full detail for one meeting, or `None` if the id is unknown.
pub async fn get_meeting(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Option<MeetingDetail>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT
            m.id, m.title, m.created_at, m.updated_at,
            m.folder_path, m.calendar_event_id,
            sp.result AS summary,
            sp.status AS summary_status,
            (SELECT COUNT(*) FROM transcripts t WHERE t.meeting_id = m.id)
                AS transcript_segment_count,
            (SELECT COUNT(*) FROM action_items ai
                WHERE ai.meeting_id = m.id AND ai.status = 'open') AS open_count,
            (SELECT COUNT(*) FROM action_items ai
                WHERE ai.meeting_id = m.id AND ai.status = 'done') AS done_count,
            (SELECT COUNT(*) FROM meeting_notes mn WHERE mn.meeting_id = m.id)
                AS note_count
        FROM meetings m
        LEFT JOIN summary_processes sp ON sp.meeting_id = m.id
        WHERE m.id = ?
        "#,
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await?;

    let Some(r) = row else { return Ok(None) };
    Ok(Some(MeetingDetail {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        folder_path: r.try_get("folder_path")?,
        calendar_event_id: r.try_get("calendar_event_id")?,
        // An empty result means "generated nothing", which is the same as no
        // summary from the agent's point of view.
        summary: r.try_get::<Option<String>, _>("summary")?.filter(|s| !s.is_empty()),
        summary_status: r.try_get("summary_status")?,
        transcript_segment_count: r.try_get("transcript_segment_count")?,
        open_action_item_count: r.try_get("open_count")?,
        done_action_item_count: r.try_get("done_count")?,
        note_count: r.try_get("note_count")?,
    }))
}

/// Whether a meeting id exists — used to turn writes against a bogus id into a
/// clear error instead of a foreign-key violation.
pub async fn meeting_exists(pool: &SqlitePool, meeting_id: &str) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT 1 FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Ordered transcript segments for a meeting.
pub async fn get_transcript(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Vec<TranscriptSegment>, sqlx::Error> {
    // sequence_id is the authoritative order but is NULL for rows written
    // before it existed and for imported/retranscribed meetings, so
    // audio_start_time is the fallback and timestamp the last resort.
    let rows = sqlx::query(
        r#"
        SELECT
            t.transcript, t.speaker, t.audio_start_time, t.audio_end_time,
            vp.name AS voice_profile_name
        FROM transcripts t
        LEFT JOIN voice_profiles vp ON vp.id = t.voice_profile_id
        WHERE t.meeting_id = ?
        ORDER BY
            t.sequence_id IS NULL, t.sequence_id,
            t.audio_start_time IS NULL, t.audio_start_time,
            t.timestamp
        "#,
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            let speaker: Option<String> = r.try_get("speaker")?;
            let profile: Option<String> = r.try_get("voice_profile_name")?;
            let audio_start_time: Option<f64> = r.try_get("audio_start_time")?;
            Ok(TranscriptSegment {
                text: r.try_get("transcript")?,
                speaker_label: speaker_label(speaker.as_deref(), profile.as_deref()),
                speaker,
                start: format_offset(audio_start_time),
                audio_start_time,
                audio_end_time: r.try_get("audio_end_time")?,
            })
        })
        .collect()
}

/// Substring search across every transcript segment.
pub async fn search_transcripts(
    pool: &SqlitePool,
    query: &str,
    limit: u32,
) -> Result<Vec<TranscriptHit>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT
            m.id AS meeting_id,
            m.title AS meeting_title,
            m.created_at AS meeting_created_at,
            t.transcript,
            t.speaker,
            t.audio_start_time,
            vp.name AS voice_profile_name
        FROM transcripts t
        JOIN meetings m ON m.id = t.meeting_id
        LEFT JOIN voice_profiles vp ON vp.id = t.voice_profile_id
        WHERE t.transcript LIKE ? ESCAPE '\'
        ORDER BY m.created_at DESC, t.sequence_id, t.audio_start_time
        LIMIT ?
        "#,
    )
    .bind(format!("%{}%", escape_like(query)))
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            let text: String = r.try_get("transcript")?;
            let speaker: Option<String> = r.try_get("speaker")?;
            let profile: Option<String> = r.try_get("voice_profile_name")?;
            Ok(TranscriptHit {
                meeting_id: r.try_get("meeting_id")?,
                meeting_title: r.try_get("meeting_title")?,
                meeting_created_at: r.try_get("meeting_created_at")?,
                snippet: make_snippet(&text, query, 120),
                speaker_label: speaker_label(speaker.as_deref(), profile.as_deref()),
                start: format_offset(r.try_get("audio_start_time")?),
            })
        })
        .collect()
}

/// Action items, optionally scoped to a meeting and/or status.
pub async fn get_action_items(
    pool: &SqlitePool,
    meeting_id: Option<&str>,
    status: Option<&str>,
) -> Result<Vec<ActionItem>, sqlx::Error> {
    let mut sql = String::from(
        r#"
        SELECT ai.id, ai.meeting_id, m.title AS meeting_title, ai."text",
               ai.assignee, ai.due_hint, ai.status, ai.source, ai.external_ref,
               ai.created_at, ai.updated_at, ai.completed_at
        FROM action_items ai
        JOIN meetings m ON m.id = ai.meeting_id
        WHERE 1 = 1
        "#,
    );
    if meeting_id.is_some() {
        sql.push_str(" AND ai.meeting_id = ?");
    }
    if status.is_some() {
        sql.push_str(" AND ai.status = ?");
    }
    // Open items first, then most recent — the order an agent triaging work
    // would want.
    sql.push_str(" ORDER BY ai.status = 'done', ai.created_at DESC");

    let mut q = sqlx::query(&sql);
    if let Some(v) = meeting_id {
        q = q.bind(v);
    }
    if let Some(v) = status {
        q = q.bind(v);
    }

    let rows = q.fetch_all(pool).await?;
    rows.into_iter().map(action_item_from_row).collect()
}

/// Fetch a single action item by id.
pub async fn get_action_item(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<ActionItem>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT ai.id, ai.meeting_id, m.title AS meeting_title, ai."text",
               ai.assignee, ai.due_hint, ai.status, ai.source, ai.external_ref,
               ai.created_at, ai.updated_at, ai.completed_at
        FROM action_items ai
        JOIN meetings m ON m.id = ai.meeting_id
        WHERE ai.id = ?
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.map(action_item_from_row).transpose()
}

fn action_item_from_row(r: sqlx::sqlite::SqliteRow) -> Result<ActionItem, sqlx::Error> {
    Ok(ActionItem {
        id: r.try_get("id")?,
        meeting_id: r.try_get("meeting_id")?,
        meeting_title: r.try_get("meeting_title")?,
        text: r.try_get("text")?,
        assignee: r.try_get("assignee")?,
        due_hint: r.try_get("due_hint")?,
        status: r.try_get("status")?,
        source: r.try_get("source")?,
        external_ref: r.try_get("external_ref")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        completed_at: r.try_get("completed_at")?,
    })
}

/// Most recently updated summaries.
pub async fn get_recent_summaries(
    pool: &SqlitePool,
    limit: u32,
) -> Result<Vec<MeetingSummary>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT m.id AS meeting_id, m.title, m.created_at,
               sp.updated_at AS summary_updated_at, sp.result AS summary
        FROM summary_processes sp
        JOIN meetings m ON m.id = sp.meeting_id
        WHERE sp.result IS NOT NULL AND sp.result != ''
        ORDER BY sp.updated_at DESC
        LIMIT ?
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(MeetingSummary {
                meeting_id: r.try_get("meeting_id")?,
                title: r.try_get("title")?,
                created_at: r.try_get("created_at")?,
                summary_updated_at: r.try_get("summary_updated_at")?,
                summary: r.try_get("summary")?,
            })
        })
        .collect()
}

/// Meetings for resource listing.
pub async fn list_meeting_refs(
    pool: &SqlitePool,
    limit: u32,
) -> Result<Vec<MeetingRef>, sqlx::Error> {
    sqlx::query_as::<_, MeetingRef>(
        "SELECT id, title, created_at FROM meetings ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Timestamp in the format the app writes (`to_rfc3339`).
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Insert a note with `source = 'agent'`.
pub async fn add_meeting_note(
    pool: &SqlitePool,
    meeting_id: &str,
    body: &str,
) -> Result<MeetingNote, sqlx::Error> {
    let note = MeetingNote {
        id: new_id(),
        meeting_id: meeting_id.to_string(),
        body: body.to_string(),
        source: "agent".to_string(),
        created_at: now(),
    };

    with_write_retry(|| async {
        sqlx::query(
            "INSERT INTO meeting_notes (id, meeting_id, body, source, created_at)
             VALUES (?, ?, ?, 'agent', ?)",
        )
        .bind(&note.id)
        .bind(&note.meeting_id)
        .bind(&note.body)
        .bind(&note.created_at)
        .execute(pool)
        .await
        .map(|_| ())
    })
    .await?;

    Ok(note)
}

/// Insert an open action item with `source = 'agent'`.
pub async fn create_action_item(
    pool: &SqlitePool,
    meeting_id: &str,
    text: &str,
    assignee: Option<&str>,
    due_hint: Option<&str>,
) -> Result<String, sqlx::Error> {
    let id = new_id();
    let ts = now();

    with_write_retry(|| async {
        sqlx::query(
            r#"
            INSERT INTO action_items
                (id, meeting_id, "text", assignee, due_hint, status, source,
                 created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, 'open', 'agent', ?, ?)
            "#,
        )
        .bind(&id)
        .bind(meeting_id)
        .bind(text)
        .bind(assignee)
        .bind(due_hint)
        .bind(&ts)
        .bind(&ts)
        .execute(pool)
        .await
        .map(|_| ())
    })
    .await?;

    Ok(id)
}

/// Set an action item's status, maintaining `completed_at`.
///
/// Returns false if no such item exists.
pub async fn set_action_item_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
) -> Result<bool, sqlx::Error> {
    let ts = now();
    // Reopening clears completed_at — leaving a completion timestamp on an
    // open item would be a lie that outlives this call.
    let completed_at = (status == "done").then(|| ts.clone());

    let affected = with_write_retry(|| async {
        sqlx::query(
            "UPDATE action_items SET status = ?, updated_at = ?, completed_at = ? WHERE id = ?",
        )
        .bind(status)
        .bind(&ts)
        .bind(&completed_at)
        .bind(id)
        .execute(pool)
        .await
        .map(|r| r.rows_affected())
    })
    .await?;

    Ok(affected > 0)
}

/// Result of an [`update_summary`] call.
pub struct SummaryWrite {
    /// The `status` the row carries after the write.
    pub status: String,
    pub updated_at: String,
    /// Set when the row was mid-generation, meaning the app is likely to
    /// overwrite what we just wrote.
    pub warning: Option<String>,
}

/// Write `summary_processes.result` for a meeting, preserving `status`.
///
/// If a generation is in flight (`PENDING`/`PROCESSING`), we still write —
/// the caller asked — but we report a warning, because the app's generator
/// will overwrite `result` when it finishes.
pub async fn update_summary(
    pool: &SqlitePool,
    meeting_id: &str,
    markdown: &str,
) -> Result<SummaryWrite, sqlx::Error> {
    let ts = now();

    let existing_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM summary_processes WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;

    with_write_retry(|| async {
        // ON CONFLICT touches only result and updated_at, so an in-flight
        // generation's status/start_time/error survive intact.
        sqlx::query(
            r#"
            INSERT INTO summary_processes (meeting_id, status, created_at, updated_at, result)
            VALUES (?, 'completed', ?, ?, ?)
            ON CONFLICT(meeting_id) DO UPDATE SET
                result = excluded.result,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(meeting_id)
        .bind(&ts)
        .bind(&ts)
        .bind(markdown)
        .execute(pool)
        .await
        .map(|_| ())
    })
    .await?;

    let status = existing_status.clone().unwrap_or_else(|| "completed".into());
    let warning = existing_status
        .filter(|s| matches!(s.to_ascii_uppercase().as_str(), "PENDING" | "PROCESSING"))
        .map(|s| {
            format!(
                "Parley is generating a summary for this meeting right now \
                 (status={s}). The summary was written, but the app will \
                 overwrite it when generation finishes."
            )
        });

    Ok(SummaryWrite {
        status,
        updated_at: ts,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, speaker: &str, start: Option<f64>) -> TranscriptSegment {
        TranscriptSegment {
            text: text.into(),
            speaker_label: speaker_label(Some(speaker), None),
            speaker: Some(speaker.into()),
            start: format_offset(start),
            audio_start_time: start,
            audio_end_time: None,
        }
    }

    #[test]
    fn clamp_limit_applies_default_and_bounds() {
        assert_eq!(clamp_limit(None, 50), 50);
        assert_eq!(clamp_limit(Some(10), 50), 10);
        assert_eq!(clamp_limit(Some(0), 50), 1, "zero clamps up to 1");
        assert_eq!(clamp_limit(Some(99_999), 50), MAX_LIMIT);
    }

    #[test]
    fn validate_date_accepts_iso_dates() {
        assert_eq!(validate_date("2026-07-15", "since").unwrap(), "2026-07-15");
        assert_eq!(
            validate_date("  2026-07-15  ", "since").unwrap(),
            "2026-07-15",
            "surrounding whitespace is trimmed"
        );
    }

    #[test]
    fn validate_date_rejects_anything_else() {
        // A full timestamp is the tempting mistake, and it would compare
        // against the wrong characters — so it must be rejected, not truncated.
        for bad in [
            "2026-07-15T10:00:00Z",
            "15/07/2026",
            "2026-7-5",
            "yesterday",
            "",
        ] {
            let err = validate_date(bad, "since").unwrap_err();
            assert!(err.contains("YYYY-MM-DD"), "{bad:?} -> {err}");
        }
    }

    #[test]
    fn validate_status_normalizes_case() {
        assert_eq!(validate_status("open").unwrap(), "open");
        assert_eq!(validate_status("DONE").unwrap(), "done");
        assert_eq!(validate_status(" Done ").unwrap(), "done");
        assert!(validate_status("closed").is_err());
    }

    #[test]
    fn escape_like_neutralizes_wildcards() {
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
        assert_eq!(escape_like("plain text"), "plain text");
    }

    #[test]
    fn make_snippet_windows_around_the_hit() {
        let text = "aaaaaaaaaa budget review bbbbbbbbbb";
        let s = make_snippet(text, "budget", 4);
        assert!(s.contains("budget"), "{s}");
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
    }

    #[test]
    fn make_snippet_is_case_insensitive_and_keeps_original_case() {
        let s = make_snippet("We discussed the Budget today", "budget", 100);
        assert!(s.contains("Budget"), "{s}");
        assert!(!s.contains('…'), "short text needs no ellipsis: {s}");
    }

    #[test]
    fn make_snippet_handles_multibyte_text() {
        // Byte-slicing here would panic; char-slicing must not.
        let text = "café ☕ budget discussion 日本語テキスト";
        let s = make_snippet(text, "budget", 3);
        assert!(s.contains("budget"), "{s}");
    }

    #[test]
    fn make_snippet_falls_back_when_query_is_absent() {
        let s = make_snippet("some text", "nomatch", 4);
        assert!(s.starts_with("some"), "{s}");
    }

    #[test]
    fn speaker_label_prefers_enrolled_name() {
        assert_eq!(speaker_label(Some("mic"), Some("Seb")), "Seb");
        assert_eq!(speaker_label(Some("mic"), None), "You (microphone)");
        assert_eq!(speaker_label(Some("system"), None), "Others (system audio)");
        assert_eq!(speaker_label(Some("Speaker 2"), None), "Speaker 2");
        assert_eq!(speaker_label(None, None), "Unknown");
        assert_eq!(
            speaker_label(Some("mic"), Some("   ")),
            "You (microphone)",
            "a blank profile name is not a name"
        );
    }

    #[test]
    fn format_offset_renders_hms() {
        assert_eq!(format_offset(Some(0.0)).unwrap(), "00:00:00");
        assert_eq!(format_offset(Some(125.3)).unwrap(), "00:02:05");
        assert_eq!(format_offset(Some(3725.0)).unwrap(), "01:02:05");
        assert_eq!(format_offset(None), None);
        assert_eq!(format_offset(Some(-1.0)), None);
        assert_eq!(format_offset(Some(f64::NAN)), None);
    }

    #[test]
    fn render_transcript_groups_consecutive_speaker_runs() {
        let segments = vec![
            seg("Hello there.", "mic", Some(0.0)),
            seg("How are you?", "mic", Some(2.0)),
            seg("Doing well.", "system", Some(4.0)),
        ];
        let out = render_transcript(&segments, true);
        assert_eq!(
            out,
            "[00:00:00] You (microphone): Hello there. How are you?\n\
             [00:00:04] Others (system audio): Doing well."
        );
    }

    #[test]
    fn render_transcript_plain_drops_attribution() {
        let segments = vec![
            seg("Hello there.", "mic", Some(0.0)),
            seg("Doing well.", "system", Some(4.0)),
        ];
        assert_eq!(
            render_transcript(&segments, false),
            "Hello there. Doing well."
        );
    }

    #[test]
    fn render_transcript_handles_empty_input() {
        assert_eq!(render_transcript(&[], true), "");
        assert_eq!(render_transcript(&[], false), "");
    }

    #[test]
    fn meeting_resource_uri_roundtrips() {
        let uri = meeting_resource_uri("abc-123");
        assert_eq!(uri, "parley://meeting/abc-123");
        assert_eq!(parse_meeting_resource_uri(&uri), Some("abc-123"));
        assert_eq!(
            parse_meeting_resource_uri("meetily://meeting/abc-123"),
            Some("abc-123")
        );
    }

    #[test]
    fn parse_meeting_resource_uri_rejects_other_schemes() {
        assert_eq!(parse_meeting_resource_uri("file:///tmp/x"), None);
        assert_eq!(parse_meeting_resource_uri("parley://meeting/"), None);
        assert_eq!(parse_meeting_resource_uri("parley://note/1"), None);
    }

    #[test]
    fn date_prefix_filter_targets_the_date_only() {
        assert_eq!(date_prefix_filter("m.created_at"), "substr(m.created_at, 1, 10)");
    }
}
