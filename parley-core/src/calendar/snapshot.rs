use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::models::{Attendee, CalendarEvent};

/// Filename used for the sidecar snapshot dropped into a meeting folder
/// alongside `metadata.json` / `transcripts.json` when the meeting is
/// linked to a calendar event.
pub const SNAPSHOT_FILENAME: &str = "calendar_event.json";
const SNAPSHOT_VERSION: &str = "1.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEventSnapshot {
    pub version: String,
    pub linked_at: String,
    pub event: SnapshotEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEvent {
    pub id: String,
    pub source_id: String,
    pub ics_uid: String,
    pub recurrence_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub organizer: SnapshotOrganizer,
    pub start_at: String,
    pub end_at: String,
    pub is_all_day: bool,
    pub attendees: Vec<Attendee>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotOrganizer {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl CalendarEventSnapshot {
    pub fn from_event(event: &CalendarEvent) -> Self {
        CalendarEventSnapshot {
            version: SNAPSHOT_VERSION.to_string(),
            linked_at: Utc::now().to_rfc3339(),
            event: SnapshotEvent {
                id: event.id.clone(),
                source_id: event.source_id.clone(),
                ics_uid: event.ics_uid.clone(),
                recurrence_id: event.recurrence_id.clone(),
                summary: event.summary.clone(),
                description: event.description.clone(),
                location: event.location.clone(),
                organizer: SnapshotOrganizer {
                    name: event.organizer_name.clone(),
                    email: event.organizer_email.clone(),
                },
                start_at: event.start_at.to_rfc3339(),
                end_at: event.end_at.to_rfc3339(),
                is_all_day: event.is_all_day,
                attendees: event.attendees.clone(),
            },
        }
    }
}

/// Look up a meeting's `folder_path` so the calendar feature can write
/// sidecar files alongside `metadata.json`. Returns None when the
/// recording was saved without a folder (e.g. `auto_save = false`),
/// in which case there's nothing to attach the snapshot to.
pub async fn lookup_meeting_folder(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT folder_path FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(p,)| p))
}

/// Write the snapshot atomically (`.tmp` → rename) so a partial write
/// never leaves a corrupted sidecar in the user's recordings folder.
pub fn write_snapshot(folder: &Path, snapshot: &CalendarEventSnapshot) -> Result<()> {
    if !folder.exists() {
        return Err(anyhow::anyhow!(
            "meeting folder does not exist: {}",
            folder.display()
        ));
    }
    let target = folder.join(SNAPSHOT_FILENAME);
    let tmp = folder.join(format!(".{}.tmp", SNAPSHOT_FILENAME));
    let json = serde_json::to_string_pretty(snapshot).context("serialize snapshot")?;
    std::fs::write(&tmp, json).context("write snapshot tmp file")?;
    std::fs::rename(&tmp, &target).context("rename snapshot tmp into place")?;
    Ok(())
}

/// Remove the snapshot if it exists. No-op when missing.
pub fn delete_snapshot(folder: &Path) -> Result<()> {
    let target = folder.join(SNAPSHOT_FILENAME);
    if target.exists() {
        std::fs::remove_file(&target).context("delete snapshot")?;
    }
    Ok(())
}
