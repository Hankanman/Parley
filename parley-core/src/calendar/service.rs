//! Shared core of the `calendar_link_meeting` Tauri command: link (or
//! unlink) a meeting to a calendar event, then best-effort write/delete the
//! sidecar `calendar_event.json` snapshot alongside the recording folder.
//! Extracted so the GPUI shell can call the exact same behaviour without
//! going through Tauri.

use std::path::PathBuf;

use sqlx::SqlitePool;

use super::repository::CalendarRepository;
use super::snapshot::{self, lookup_meeting_folder, CalendarEventSnapshot, SNAPSHOT_FILENAME};

/// Link `meeting_id` to `event_id` (or unlink when `None`), then best-effort
/// write or delete the sidecar snapshot file. Returns `false` only when the
/// meeting row itself wasn't found/updated — snapshot failures are logged,
/// not surfaced, matching the Tauri command.
pub async fn link_meeting_with_snapshot(
    pool: &SqlitePool,
    meeting_id: &str,
    event_id: Option<&str>,
) -> Result<bool, String> {
    let updated = CalendarRepository::link_meeting(pool, meeting_id, event_id)
        .await
        .map_err(|e| e.to_string())?;

    if !updated {
        return Ok(false);
    }

    let folder = match lookup_meeting_folder(pool, meeting_id).await {
        Ok(Some(p)) => Some(PathBuf::from(p)),
        Ok(None) => None,
        Err(e) => {
            log::warn!(
                "calendar snapshot: meeting folder lookup failed for {}: {}",
                meeting_id,
                e
            );
            None
        }
    };

    if let Some(folder) = folder {
        if let Some(event_id) = event_id {
            match CalendarRepository::get_event(pool, event_id).await {
                Ok(Some(event)) => {
                    let snapshot_data = CalendarEventSnapshot::from_event(&event);
                    if let Err(e) = snapshot::write_snapshot(&folder, &snapshot_data) {
                        log::warn!(
                            "calendar snapshot: failed to write {} for meeting {}: {}",
                            SNAPSHOT_FILENAME,
                            meeting_id,
                            e
                        );
                    }
                }
                Ok(None) => log::warn!(
                    "calendar snapshot: event {} not found when writing snapshot for meeting {}",
                    event_id,
                    meeting_id
                ),
                Err(e) => log::warn!("calendar snapshot: get_event {} failed: {}", event_id, e),
            }
        } else if let Err(e) = snapshot::delete_snapshot(&folder) {
            log::warn!(
                "calendar snapshot: failed to delete {} for meeting {}: {}",
                SNAPSHOT_FILENAME,
                meeting_id,
                e
            );
        }
    }

    Ok(true)
}
