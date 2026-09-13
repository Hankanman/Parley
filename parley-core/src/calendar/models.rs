use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CalendarSourceRow {
    pub id: String,
    pub url: String,
    pub label: Option<String>,
    pub last_fetched_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CalendarEventRow {
    pub id: String,
    pub source_id: String,
    pub ics_uid: String,
    pub recurrence_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub start_at: String,
    pub end_at: String,
    pub is_all_day: i64,
    pub attendees_json: String,
    pub raw_ics: Option<String>,
    pub fetched_at: String,
}

/// Parsed attendee entry stored as JSON in `calendar_events.attendees_json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Attendee {
    pub name: Option<String>,
    pub email: Option<String>,
    pub role: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "isOrganizer", default)]
    pub is_organizer: bool,
}

/// Frontend-facing event shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    #[serde(rename = "sourceId")]
    pub source_id: String,
    #[serde(rename = "icsUid")]
    pub ics_uid: String,
    #[serde(rename = "recurrenceId")]
    pub recurrence_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    #[serde(rename = "organizerName")]
    pub organizer_name: Option<String>,
    #[serde(rename = "organizerEmail")]
    pub organizer_email: Option<String>,
    #[serde(rename = "startAt")]
    pub start_at: DateTime<Utc>,
    #[serde(rename = "endAt")]
    pub end_at: DateTime<Utc>,
    #[serde(rename = "isAllDay")]
    pub is_all_day: bool,
    pub attendees: Vec<Attendee>,
}

impl TryFrom<CalendarEventRow> for CalendarEvent {
    type Error = String;

    fn try_from(row: CalendarEventRow) -> Result<Self, Self::Error> {
        let start_at = DateTime::parse_from_rfc3339(&row.start_at)
            .map_err(|e| format!("invalid start_at {}: {}", row.start_at, e))?
            .with_timezone(&Utc);
        let end_at = DateTime::parse_from_rfc3339(&row.end_at)
            .map_err(|e| format!("invalid end_at {}: {}", row.end_at, e))?
            .with_timezone(&Utc);
        let attendees: Vec<Attendee> = serde_json::from_str(&row.attendees_json)
            .map_err(|e| format!("invalid attendees_json: {}", e))?;

        Ok(CalendarEvent {
            id: row.id,
            source_id: row.source_id,
            ics_uid: row.ics_uid,
            recurrence_id: row.recurrence_id,
            summary: row.summary,
            description: row.description,
            location: row.location,
            organizer_name: row.organizer_name,
            organizer_email: row.organizer_email,
            start_at,
            end_at,
            is_all_day: row.is_all_day != 0,
            attendees,
        })
    }
}

/// In-memory event used between parser and repository before being assigned a
/// row id and stored.
#[derive(Debug, Clone)]
pub struct ParsedEvent {
    pub ics_uid: String,
    pub recurrence_id: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub is_all_day: bool,
    pub attendees: Vec<Attendee>,
    pub rrule_block: Option<String>,
    pub raw_ics: String,
}
