//! Queries for the features added to Parley after the first tool set:
//! calendar events and briefs, speakers (voice profiles), chapters,
//! bookmarks, notes, follow-up drafts, and action-item edits.
//!
//! Same rules as [`crate::db`]: timestamps are passed through as stored
//! (calendar times are `to_rfc3339()` UTC), writes go through
//! [`db::with_write_retry`], and nothing here creates or migrates schema.

use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::db::{self, format_offset};

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// An invitee, as parsed from the event's ICS.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Attendee {
    pub name: Option<String>,
    pub email: Option<String>,
    /// RSVP: `ACCEPTED`, `DECLINED`, `TENTATIVE`, `NEEDS-ACTION`, …
    pub status: Option<String>,
    #[serde(rename = "isOrganizer", default)]
    pub is_organizer: bool,
}

/// A calendar event from one of the user's ICS feeds.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CalendarEvent {
    pub event_id: String,
    pub title: Option<String>,
    /// UTC, RFC 3339.
    pub start_at: String,
    pub end_at: String,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub description: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub attendees: Vec<Attendee>,
    /// Identifies a recurring series; pass to `list_meetings` as `series_id`
    /// to get every recorded occurrence.
    pub series_id: String,
    pub is_recurring: bool,
    /// Meetings recorded for this event.
    pub meeting_ids: Vec<String>,
    /// Whether Parley has prepared a brief for it (`get_brief`).
    pub has_brief: bool,
}

/// A saved voice profile: a person Parley recognises.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Speaker {
    pub speaker_id: String,
    pub name: String,
    pub email: Option<String>,
    /// The user's own voice.
    pub is_self: bool,
    /// Voice samples the profile was built from.
    pub sample_count: i64,
    /// Meetings with at least one transcript line attributed to them.
    pub meeting_count: i64,
    pub last_meeting_at: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Chapter {
    pub position: i64,
    pub title: String,
    pub start_seconds: f64,
    /// `HH:MM:SS` from the start of the recording.
    pub start: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Bookmark {
    pub label: Option<String>,
    pub offset_seconds: f64,
    pub start: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FollowupDraft {
    pub meeting_id: String,
    /// `concise`, `formal` or `friendly`.
    pub tone: String,
    pub subject: String,
    pub body: String,
    /// What it was drafted from: the structured summary, the summary, or the
    /// transcript.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A meeting a brief cites, as `[M1]`, `[M2]`, ….
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BriefSource {
    pub label: String,
    pub meeting_id: String,
    pub title: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Brief {
    pub event_id: String,
    pub event_title: Option<String>,
    pub start_at: String,
    /// Markdown; citations like `[M1]` refer to `sources`.
    pub brief: String,
    pub sources: Vec<BriefSource>,
    pub generated_at: String,
}

// ---------------------------------------------------------------------------
// Calendar
// ---------------------------------------------------------------------------

/// `source_id:ics_uid`, the key every occurrence of a series shares.
pub fn series_id(source_id: &str, ics_uid: &str) -> String {
    format!("{source_id}:{ics_uid}")
}

/// Split a [`series_id`] back into `(source_id, ics_uid)`. Source ids are
/// UUIDs, so the first `:` is the separator even if the UID contains more.
pub fn parse_series_id(value: &str) -> Option<(&str, &str)> {
    let (source, uid) = value.trim().split_once(':')?;
    (!source.is_empty() && !uid.is_empty()).then_some((source, uid))
}

const EVENT_COLUMNS: &str = r#"
    ce.id, ce.source_id, ce.ics_uid, ce.recurrence_id, ce.summary, ce.description,
    ce.location, ce.organizer_name, ce.organizer_email, ce.start_at, ce.end_at,
    ce.is_all_day, ce.attendees_json,
    (SELECT COUNT(*) FROM calendar_events s
        WHERE s.source_id = ce.source_id AND s.ics_uid = ce.ics_uid) AS occurrences,
    (SELECT GROUP_CONCAT(m.id) FROM meetings m WHERE m.calendar_event_id = ce.id) AS meeting_ids,
    EXISTS(SELECT 1 FROM meeting_briefs b
        WHERE b.source_id = ce.source_id AND b.ics_uid = ce.ics_uid
          AND julianday(b.start_at) = julianday(ce.start_at)) AS has_brief
"#;

fn event_from_row(r: sqlx::sqlite::SqliteRow) -> Result<CalendarEvent, sqlx::Error> {
    let source_id: String = r.try_get("source_id")?;
    let ics_uid: String = r.try_get("ics_uid")?;
    let recurrence_id: Option<String> = r.try_get("recurrence_id")?;
    let attendees_json: String = r.try_get("attendees_json")?;
    let meeting_ids: Option<String> = r.try_get("meeting_ids")?;
    Ok(CalendarEvent {
        event_id: r.try_get("id")?,
        title: r.try_get("summary")?,
        start_at: r.try_get("start_at")?,
        end_at: r.try_get("end_at")?,
        is_all_day: r.try_get::<i64, _>("is_all_day")? != 0,
        location: r.try_get("location")?,
        description: r.try_get("description")?,
        organizer_name: r.try_get("organizer_name")?,
        organizer_email: r.try_get("organizer_email")?,
        attendees: serde_json::from_str(&attendees_json).unwrap_or_default(),
        is_recurring: recurrence_id.is_some() || r.try_get::<i64, _>("occurrences")? > 1,
        series_id: series_id(&source_id, &ics_uid),
        meeting_ids: meeting_ids
            .map(|ids| ids.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        has_brief: r.try_get::<i64, _>("has_brief")? != 0,
    })
}

/// One event by id.
pub async fn get_calendar_event(
    pool: &SqlitePool,
    event_id: &str,
) -> Result<Option<CalendarEvent>, sqlx::Error> {
    let sql = format!("SELECT {EVENT_COLUMNS} FROM calendar_events ce WHERE ce.id = ?");
    sqlx::query(&sql)
        .bind(event_id)
        .fetch_optional(pool)
        .await?
        .map(event_from_row)
        .transpose()
}

/// Events overlapping `[from, to]` (RFC 3339 instants), earliest first,
/// optionally filtered by a substring of the title.
pub async fn list_calendar_events(
    pool: &SqlitePool,
    from: &str,
    to: &str,
    title_query: Option<&str>,
    limit: u32,
) -> Result<Vec<CalendarEvent>, sqlx::Error> {
    let mut sql = format!(
        "SELECT {EVENT_COLUMNS} FROM calendar_events ce
         WHERE julianday(ce.end_at) >= julianday(?) AND julianday(ce.start_at) <= julianday(?)"
    );
    if title_query.is_some() {
        sql.push_str(" AND ce.summary LIKE ? ESCAPE '\\'");
    }
    sql.push_str(" ORDER BY julianday(ce.start_at) LIMIT ?");
    let mut q = sqlx::query(&sql).bind(from).bind(to);
    if let Some(t) = title_query {
        q = q.bind(format!("%{}%", db::escape_like(t)));
    }
    q.bind(limit)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(event_from_row)
        .collect()
}

/// The cached brief for an event, if Parley has generated one. Briefs are
/// keyed by series and occurrence start, so a refreshed feed (new event row
/// id) still finds its brief.
pub async fn get_brief(pool: &SqlitePool, event_id: &str) -> Result<Option<Brief>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT ce.id, ce.summary, ce.start_at, b.brief, b.sources_json, b.created_at
        FROM calendar_events ce
        JOIN meeting_briefs b
          ON b.source_id = ce.source_id AND b.ics_uid = ce.ics_uid
         AND julianday(b.start_at) = julianday(ce.start_at)
        WHERE ce.id = ?
        "#,
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let sources_json: String = r.try_get("sources_json")?;
    Ok(Some(Brief {
        event_id: r.try_get("id")?,
        event_title: r.try_get("summary")?,
        start_at: r.try_get("start_at")?,
        brief: r.try_get("brief")?,
        sources: serde_json::from_str(&sources_json).unwrap_or_default(),
        generated_at: r.try_get("created_at")?,
    }))
}

// ---------------------------------------------------------------------------
// Speakers
// ---------------------------------------------------------------------------

/// Every saved voice profile, by name.
pub async fn list_speakers(pool: &SqlitePool) -> Result<Vec<Speaker>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT vp.id, vp.name, vp.email, vp.is_self, vp.sample_count,
               COUNT(DISTINCT t.meeting_id) AS meeting_count,
               MAX(m.created_at) AS last_meeting_at
        FROM voice_profiles vp
        LEFT JOIN transcripts t ON t.voice_profile_id = vp.id
        LEFT JOIN meetings m ON m.id = t.meeting_id
        WHERE TRIM(vp.name) != ''
        GROUP BY vp.id
        ORDER BY vp.is_self DESC, vp.name COLLATE NOCASE
        "#,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(Speaker {
                speaker_id: r.try_get("id")?,
                name: r.try_get("name")?,
                email: r.try_get("email")?,
                is_self: r.try_get::<i64, _>("is_self")? != 0,
                sample_count: r.try_get("sample_count")?,
                meeting_count: r.try_get("meeting_count")?,
                last_meeting_at: r.try_get("last_meeting_at")?,
            })
        })
        .collect()
}

/// Restrict a meetings query (aliased `m`) to meetings with `person`: a
/// speaker id, or a name or email matched against voice profiles,
/// transcript speaker labels and the linked event's attendees. Mirrors the
/// app's Ask person filter, plus attendees.
pub fn push_person_filter(qb: &mut QueryBuilder<'_, Sqlite>, person: &str) {
    let person = person.trim().to_lowercase();
    qb.push(
        " AND (EXISTS (SELECT 1 FROM transcripts p
                LEFT JOIN voice_profiles pv ON pv.id = p.voice_profile_id
                WHERE p.meeting_id = m.id AND (p.voice_profile_id = ",
    )
    .push_bind(person.clone())
    .push(" OR LOWER(TRIM(pv.name)) = ")
    .push_bind(person.clone())
    .push(" OR LOWER(TRIM(pv.email)) = ")
    .push_bind(person.clone())
    .push(" OR LOWER(TRIM(p.speaker)) = ")
    .push_bind(person.clone())
    .push(
        "))
           OR EXISTS (SELECT 1 FROM calendar_events pe, json_each(pe.attendees_json) a
                WHERE pe.id = m.calendar_event_id AND (LOWER(TRIM(json_extract(a.value, '$.name'))) = ",
    )
    .push_bind(person.clone())
    .push(" OR LOWER(TRIM(json_extract(a.value, '$.email'))) = ")
    .push_bind(person)
    .push(")))");
}

/// Restrict a meetings query (aliased `m`) to occurrences of one series.
pub fn push_series_filter(qb: &mut QueryBuilder<'_, Sqlite>, source_id: &str, ics_uid: &str) {
    qb.push(" AND m.calendar_event_id IN (SELECT se.id FROM calendar_events se WHERE se.source_id = ")
        .push_bind(source_id.to_string())
        .push(" AND se.ics_uid = ")
        .push_bind(ics_uid.to_string())
        .push(")");
}

// ---------------------------------------------------------------------------
// Per-meeting detail
// ---------------------------------------------------------------------------

pub async fn get_chapters(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<Chapter>, sqlx::Error> {
    let rows: Vec<(i64, String, f64)> = sqlx::query_as(
        "SELECT position, title, start_seconds FROM meeting_chapters WHERE meeting_id = ? ORDER BY position",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(position, title, start_seconds)| Chapter {
            position,
            title,
            start_seconds,
            start: format_offset(Some(start_seconds)),
        })
        .collect())
}

pub async fn get_bookmarks(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<Bookmark>, sqlx::Error> {
    let rows: Vec<(Option<String>, f64)> = sqlx::query_as(
        "SELECT label, offset_seconds FROM meeting_bookmarks WHERE meeting_id = ? ORDER BY offset_seconds",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(label, offset_seconds)| Bookmark {
            label: label.filter(|l| !l.trim().is_empty()),
            offset_seconds,
            start: format_offset(Some(offset_seconds)),
        })
        .collect())
}

pub async fn get_notes(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<db::MeetingNote>, sqlx::Error> {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT id, meeting_id, body, source, created_at FROM meeting_notes WHERE meeting_id = ? ORDER BY created_at",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, meeting_id, body, source, created_at)| db::MeetingNote {
            id,
            meeting_id,
            body,
            source,
            created_at,
        })
        .collect())
}

pub async fn get_followup_draft(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Option<FollowupDraft>, sqlx::Error> {
    let row: Option<(String, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT meeting_id, tone, subject, body, source, created_at, updated_at
         FROM meeting_followup_drafts WHERE meeting_id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(meeting_id, tone, subject, body, source, created_at, updated_at)| FollowupDraft {
            meeting_id,
            tone,
            subject,
            body,
            source,
            created_at,
            updated_at,
        },
    ))
}

// ---------------------------------------------------------------------------
// Action-item edits
// ---------------------------------------------------------------------------

/// How to change one optional action-item field.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldEdit {
    Keep,
    Clear,
    Set(String),
}

impl FieldEdit {
    /// Absent keeps the value, an empty string clears it.
    pub fn from_arg(value: Option<String>) -> Self {
        match value.map(|v| v.trim().to_string()) {
            None => FieldEdit::Keep,
            Some(v) if v.is_empty() => FieldEdit::Clear,
            Some(v) => FieldEdit::Set(v),
        }
    }
}

/// Change an action item's text, owner and/or due hint. Returns false if
/// no such item exists.
pub async fn update_action_item(
    pool: &SqlitePool,
    id: &str,
    text: Option<&str>,
    assignee: &FieldEdit,
    due_hint: &FieldEdit,
) -> Result<bool, sqlx::Error> {
    let ts = chrono::Utc::now().to_rfc3339();
    let affected = db::with_write_retry(|| async {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("UPDATE action_items SET updated_at = ");
        qb.push_bind(ts.clone());
        if let Some(text) = text {
            qb.push(r#", "text" = "#).push_bind(text.to_string());
        }
        for (column, edit) in [("assignee", assignee), ("due_hint", due_hint)] {
            match edit {
                FieldEdit::Keep => {}
                FieldEdit::Clear => {
                    qb.push(format!(", {column} = NULL"));
                }
                FieldEdit::Set(v) => {
                    qb.push(format!(", {column} = ")).push_bind(v.clone());
                }
            }
        }
        qb.push(" WHERE id = ").push_bind(id.to_string());
        qb.build().execute(pool).await.map(|r| r.rows_affected())
    })
    .await?;
    Ok(affected > 0)
}

/// Delete an action item. Returns false if no such item exists.
pub async fn delete_action_item(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let affected = db::with_write_retry(|| async {
        sqlx::query("DELETE FROM action_items WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await
            .map(|r| r.rows_affected())
    })
    .await?;
    Ok(affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("../parley-core/migrations").run(&pool).await.unwrap();
        let now = "2026-10-20T09:00:00+00:00";
        for sql in [
            "INSERT INTO calendar_sources (id, url, created_at, updated_at) VALUES ('src', 'https://x/cal.ics', '2026-10-01T00:00:00+00:00', '2026-10-01T00:00:00+00:00')",
            r#"INSERT INTO calendar_events (id, source_id, ics_uid, recurrence_id, summary, start_at, end_at, attendees_json, fetched_at) VALUES
                ('e1', 'src', 'sync@x', '2026-10-20T09:00:00+00:00', 'Weekly sync', '2026-10-20T09:00:00+00:00', '2026-10-20T09:30:00+00:00',
                 '[{"name":"Priya Shah","email":"priya@x.com","status":"ACCEPTED","isOrganizer":true}]', '2026-10-01T00:00:00+00:00'),
                ('e2', 'src', 'sync@x', '2026-10-27T09:00:00+00:00', 'Weekly sync', '2026-10-27T09:00:00+00:00', '2026-10-27T09:30:00+00:00', '[]', '2026-10-01T00:00:00+00:00'),
                ('e3', 'src', 'oneoff@x', NULL, 'Launch review', '2026-10-21T14:00:00+00:00', '2026-10-21T15:00:00+00:00', '[]', '2026-10-01T00:00:00+00:00')"#,
            "INSERT INTO voice_profiles (id, name, email, embedding, embedding_dim, sample_count, is_self, created_at, updated_at) VALUES
                ('vp1', 'Sam Lee', 'sam@x.com', x'00000000', 1, 4, 0, '2026-10-01', '2026-10-01')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        for (id, title, event) in [("m1", "Weekly sync", Some("e1")), ("m2", "Hallway chat", None)] {
            sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at, calendar_event_id) VALUES (?, ?, ?, ?, ?)")
                .bind(id).bind(title).bind(now).bind(now).bind(event)
                .execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO transcripts (id, meeting_id, transcript, timestamp, speaker, voice_profile_id) VALUES ('t1', 'm2', 'hi', '00:00', 'Sam Lee', 'vp1')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO meeting_briefs (cache_key, source_id, ics_uid, start_at, fingerprint, brief, sources_json, created_at)
                     VALUES ('k', 'src', 'sync@x', '2026-10-27T09:00:00+00:00', 'f', '- Ship v2 [M1]',
                     '[{\"label\":\"M1\",\"meeting_id\":\"m1\",\"title\":\"Weekly sync\",\"created_at\":null}]', ?)")
            .bind(now).execute(&pool).await.unwrap();
        pool
    }

    async fn meetings_matching(pool: &SqlitePool, build: impl FnOnce(&mut QueryBuilder<'_, Sqlite>)) -> Vec<String> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT m.id FROM meetings m WHERE 1 = 1");
        build(&mut qb);
        qb.push(" ORDER BY m.id");
        qb.build_query_scalar().fetch_all(pool).await.unwrap()
    }

    #[test]
    fn series_id_roundtrips() {
        let id = series_id("src-uuid", "abc:def@x");
        assert_eq!(parse_series_id(&id), Some(("src-uuid", "abc:def@x")));
        assert_eq!(parse_series_id("nocolon"), None);
        assert_eq!(parse_series_id(":uid"), None);
    }

    #[test]
    fn field_edit_distinguishes_absent_from_empty() {
        assert_eq!(FieldEdit::from_arg(None), FieldEdit::Keep);
        assert_eq!(FieldEdit::from_arg(Some("  ".into())), FieldEdit::Clear);
        assert_eq!(FieldEdit::from_arg(Some(" Priya ".into())), FieldEdit::Set("Priya".into()));
    }

    #[tokio::test]
    async fn events_carry_attendees_series_meetings_and_briefs() {
        let pool = pool().await;
        let e1 = get_calendar_event(&pool, "e1").await.unwrap().unwrap();
        assert_eq!(e1.attendees[0].name.as_deref(), Some("Priya Shah"));
        assert!(e1.attendees[0].is_organizer);
        assert!(e1.is_recurring);
        assert_eq!(e1.series_id, "src:sync@x");
        assert_eq!(e1.meeting_ids, vec!["m1"]);
        assert!(!e1.has_brief);
        let e2 = get_calendar_event(&pool, "e2").await.unwrap().unwrap();
        assert!(e2.has_brief);
        let e3 = get_calendar_event(&pool, "e3").await.unwrap().unwrap();
        assert!(!e3.is_recurring);
    }

    #[tokio::test]
    async fn list_calendar_events_filters_by_range_and_title() {
        let pool = pool().await;
        let all = list_calendar_events(&pool, "2026-10-20T00:00:00+00:00", "2026-10-22T00:00:00+00:00", None, 50)
            .await
            .unwrap();
        let ids: Vec<_> = all.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, ["e1", "e3"]);
        let launch = list_calendar_events(&pool, "2026-10-01T00:00:00+00:00", "2026-11-01T00:00:00+00:00", Some("launch"), 50)
            .await
            .unwrap();
        assert_eq!(launch.len(), 1);
    }

    #[tokio::test]
    async fn get_brief_matches_by_series_and_start() {
        let pool = pool().await;
        let brief = get_brief(&pool, "e2").await.unwrap().unwrap();
        assert_eq!(brief.brief, "- Ship v2 [M1]");
        assert_eq!(brief.sources[0].meeting_id, "m1");
        assert!(get_brief(&pool, "e1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn person_filter_matches_speakers_and_attendees() {
        let pool = pool().await;
        assert_eq!(meetings_matching(&pool, |qb| push_person_filter(qb, "sam lee")).await, ["m2"]);
        assert_eq!(meetings_matching(&pool, |qb| push_person_filter(qb, "vp1")).await, ["m2"]);
        assert_eq!(meetings_matching(&pool, |qb| push_person_filter(qb, "sam@x.com")).await, ["m2"]);
        assert_eq!(meetings_matching(&pool, |qb| push_person_filter(qb, "Priya@X.com")).await, ["m1"]);
        assert!(meetings_matching(&pool, |qb| push_person_filter(qb, "nobody")).await.is_empty());
    }

    #[tokio::test]
    async fn series_filter_finds_occurrences() {
        let pool = pool().await;
        assert_eq!(meetings_matching(&pool, |qb| push_series_filter(qb, "src", "sync@x")).await, ["m1"]);
    }

    #[tokio::test]
    async fn list_speakers_counts_meetings() {
        let pool = pool().await;
        let speakers = list_speakers(&pool).await.unwrap();
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].name, "Sam Lee");
        assert_eq!(speakers[0].meeting_count, 1);
    }

    #[tokio::test]
    async fn update_and_delete_action_items() {
        let pool = pool().await;
        let id = db::create_action_item(&pool, "m1", "Draft plan", Some("Sam"), Some("Friday"))
            .await
            .unwrap();
        assert!(update_action_item(&pool, &id, Some("Draft the plan"), &FieldEdit::Clear, &FieldEdit::Keep)
            .await
            .unwrap());
        let item = db::get_action_item(&pool, &id).await.unwrap().unwrap();
        assert_eq!(item.text, "Draft the plan");
        assert_eq!(item.assignee, None);
        assert_eq!(item.due_hint.as_deref(), Some("Friday"));

        assert!(!update_action_item(&pool, "missing", None, &FieldEdit::Keep, &FieldEdit::Keep).await.unwrap());
        assert!(delete_action_item(&pool, &id).await.unwrap());
        assert!(db::get_action_item(&pool, &id).await.unwrap().is_none());
        assert!(!delete_action_item(&pool, &id).await.unwrap());
    }

    #[tokio::test]
    async fn chapters_bookmarks_notes_and_drafts() {
        let pool = pool().await;
        for sql in [
            "INSERT INTO meeting_chapters (id, meeting_id, position, start_seconds, title, created_at) VALUES ('c2', 'm1', 1, 90, 'Budget', 'x'), ('c1', 'm1', 0, 0, 'Intro', 'x')",
            "INSERT INTO meeting_bookmarks (id, meeting_id, offset_seconds, label, created_at) VALUES ('b1', 'm1', 65, '', 'x')",
            "INSERT INTO meeting_followup_drafts (meeting_id, tone, subject, body, source, created_at, updated_at) VALUES ('m1', 'concise', 'Recap', 'Hi all', 'summary', 'x', 'x')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        db::add_meeting_note(&pool, "m1", "Ask about budget").await.unwrap();

        let chapters = get_chapters(&pool, "m1").await.unwrap();
        assert_eq!(chapters.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["Intro", "Budget"]);
        assert_eq!(chapters[1].start.as_deref(), Some("00:01:30"));
        let bookmarks = get_bookmarks(&pool, "m1").await.unwrap();
        assert_eq!(bookmarks[0].label, None, "blank labels read as none");
        assert_eq!(get_notes(&pool, "m1").await.unwrap()[0].body, "Ask about budget");
        assert_eq!(get_followup_draft(&pool, "m1").await.unwrap().unwrap().subject, "Recap");
        assert!(get_followup_draft(&pool, "m2").await.unwrap().is_none());
    }
}
