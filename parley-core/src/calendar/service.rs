//! Link (or unlink) a meeting to a calendar event, then best-effort
//! write/delete the sidecar `calendar_event.json` snapshot alongside the
//! recording folder.

use std::path::PathBuf;

use sqlx::SqlitePool;

use super::repository::CalendarRepository;
use super::snapshot::{self, lookup_meeting_folder, CalendarEventSnapshot, SNAPSHOT_FILENAME};

/// Link `meeting_id` to `event_id` (or unlink when `None`), then best-effort
/// write or delete the sidecar snapshot file. Returns `false` only when the
/// meeting row itself wasn't found/updated — snapshot failures are logged,
/// not surfaced, since the link itself is what matters.
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
        sync_snapshot(pool, meeting_id, &folder, event_id).await;
    }

    Ok(true)
}

/// Make the folder's snapshot describe `event_id`, or remove it when
/// unlinking. A snapshot that can't be brought up to date is removed rather
/// than left describing the previous link: [`repair_dangling_links`] trusts
/// whatever snapshot it finds, so a stale one could otherwise re-attach a
/// meeting to an event the user moved it away from.
async fn sync_snapshot(pool: &SqlitePool, meeting_id: &str, folder: &std::path::Path, event_id: Option<&str>) {
    let written = match event_id {
        None => false,
        Some(event_id) => match CalendarRepository::get_event(pool, event_id).await {
            Ok(Some(event)) => match snapshot::write_snapshot(folder, &CalendarEventSnapshot::from_event(&event)) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!(
                        "calendar snapshot: failed to write {} for meeting {}: {}",
                        SNAPSHOT_FILENAME,
                        meeting_id,
                        e
                    );
                    false
                }
            },
            Ok(None) => {
                log::warn!(
                    "calendar snapshot: event {} not found when writing snapshot for meeting {}",
                    event_id,
                    meeting_id
                );
                false
            }
            Err(e) => {
                log::warn!("calendar snapshot: get_event {} failed: {}", event_id, e);
                false
            }
        },
    };
    if !written {
        if let Err(e) = snapshot::delete_snapshot(folder) {
            log::warn!(
                "calendar snapshot: failed to delete {} for meeting {}: {}",
                SNAPSHOT_FILENAME,
                meeting_id,
                e
            );
        }
    }
}

/// Re-write the snapshot for a link made earlier (an auto-link at recording
/// start, once the meeting folder is final at stop) — but only if the
/// meeting is still linked to `event_id`. If the user unlinked or relinked
/// it in the meantime, their choice stands and nothing changes. Returns
/// whether the meeting was still linked to `event_id`.
pub async fn refresh_link_snapshot(pool: &SqlitePool, meeting_id: &str, event_id: &str) -> Result<bool, String> {
    let current: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT calendar_event_id, folder_path FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
    let Some((linked, folder)) = current else { return Ok(false) };
    if linked.as_deref() != Some(event_id) {
        log::info!("calendar link: meeting {} was relinked or unlinked; leaving it", meeting_id);
        return Ok(false);
    }
    if let Some(folder) = folder.filter(|f| !f.trim().is_empty()) {
        sync_snapshot(pool, meeting_id, &PathBuf::from(folder), Some(event_id)).await;
    }
    Ok(true)
}

/// Link a meeting whose recording just started to the event chosen by
/// [`super::autolink`], unless the meeting is already linked (by the user,
/// or an earlier auto-link). Returns whether a link was made.
pub async fn auto_link_meeting(pool: &SqlitePool, meeting_id: &str, event_id: &str) -> Result<bool, String> {
    let current: Option<(Option<String>,)> = sqlx::query_as("SELECT calendar_event_id FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
    match current {
        Some((None,)) => link_meeting_with_snapshot(pool, meeting_id, Some(event_id)).await,
        _ => Ok(false),
    }
}

/// Re-attach meetings whose calendar link points at an event row that no
/// longer exists.
///
/// Until calendar refreshes kept linked events (see
/// [`CalendarRepository::replace_events`]), every refresh deleted and
/// re-created a source's events under new ids, so most stored links
/// dangle. Each linked meeting's folder still has the event's
/// `calendar_event.json` snapshot, though, which carries its ICS UID and
/// attendees. For each dangling link this re-points the meeting at the
/// stored occurrence with the same source, UID and recurrence id, or, when
/// the feed no longer lists it, restores the event row from the snapshot.
/// Meetings whose source was removed, with no readable snapshot, or whose
/// snapshot describes a different event than the dangling link (left over
/// from an earlier link) are left alone. Snapshot files are only read,
/// never written.
///
/// Unlinked meetings (`calendar_event_id IS NULL`) are never considered,
/// whatever snapshot their folder holds, so an unlink sticks; unlinking
/// also deletes the snapshot (see [`link_meeting_with_snapshot`]).
///
/// Returns how many meetings were re-attached. Cheap when nothing dangles.
pub async fn repair_dangling_links(pool: &SqlitePool) -> Result<usize, String> {
    let dangling: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT m.id, m.calendar_event_id, m.folder_path FROM meetings m
         WHERE m.calendar_event_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM calendar_events ce WHERE ce.id = m.calendar_event_id)",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut repaired = 0;
    for (meeting_id, linked_id, folder) in dangling {
        let Some(folder) = folder.filter(|f| !f.trim().is_empty()) else { continue };
        let path = PathBuf::from(folder).join(SNAPSHOT_FILENAME);
        let Ok(json) = std::fs::read_to_string(&path) else { continue };
        let snapshot: CalendarEventSnapshot = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("calendar repair: unreadable {}: {}", path.display(), e);
                continue;
            }
        };
        // Linking writes the snapshot with the id it links to, so a
        // snapshot for some other id is left over from an earlier link
        // (the user relinked and the rewrite failed): it says nothing about
        // this one.
        if snapshot.event.id != linked_id {
            log::warn!("calendar repair: {} describes another event; skipping", path.display());
            continue;
        }
        match relink_from_snapshot(pool, &meeting_id, &snapshot).await {
            Ok(true) => repaired += 1,
            Ok(false) => {}
            Err(e) => log::warn!("calendar repair: meeting {}: {}", meeting_id, e),
        }
    }
    if repaired > 0 {
        log::info!("calendar repair: re-attached {} meeting(s) to their calendar events", repaired);
    }
    Ok(repaired)
}

async fn relink_from_snapshot(
    pool: &SqlitePool,
    meeting_id: &str,
    snapshot: &CalendarEventSnapshot,
) -> Result<bool, sqlx::Error> {
    let ev = &snapshot.event;
    let source_exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM calendar_sources WHERE id = ?")
        .bind(&ev.source_id)
        .fetch_optional(pool)
        .await?;
    if source_exists.is_none() {
        return Ok(false);
    }

    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM calendar_events
         WHERE source_id = ? AND ics_uid = ? AND COALESCE(recurrence_id, '') = ?",
    )
    .bind(&ev.source_id)
    .bind(&ev.ics_uid)
    .bind(ev.recurrence_id.clone().unwrap_or_default())
    .fetch_optional(pool)
    .await?;

    let event_id = match existing {
        Some((id,)) => id,
        None => {
            let id_taken: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM calendar_events WHERE id = ?")
                .bind(&ev.id)
                .fetch_optional(pool)
                .await?;
            let id = if id_taken.is_some() { uuid::Uuid::new_v4().to_string() } else { ev.id.clone() };
            let attendees_json = serde_json::to_string(&ev.attendees).unwrap_or_else(|_| "[]".to_string());
            sqlx::query(
                "INSERT INTO calendar_events (
                    id, source_id, ics_uid, recurrence_id, summary, description, location,
                    organizer_name, organizer_email, start_at, end_at, is_all_day,
                    attendees_json, raw_ics, fetched_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
            )
            .bind(&id)
            .bind(&ev.source_id)
            .bind(&ev.ics_uid)
            .bind(&ev.recurrence_id)
            .bind(&ev.summary)
            .bind(&ev.description)
            .bind(&ev.location)
            .bind(&ev.organizer.name)
            .bind(&ev.organizer.email)
            .bind(&ev.start_at)
            .bind(&ev.end_at)
            .bind(if ev.is_all_day { 1 } else { 0 })
            .bind(&attendees_json)
            .bind(&snapshot.linked_at)
            .execute(pool)
            .await?;
            id
        }
    };

    // Only the link changes: the meeting's updated_at is left alone.
    let res = sqlx::query("UPDATE meetings SET calendar_event_id = ? WHERE id = ?")
        .bind(&event_id)
        .bind(meeting_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::models::Attendee;
    use crate::calendar::snapshot::{SnapshotEvent, SnapshotOrganizer};
    use crate::database::repositories::meeting::MeetingsRepository;
    use sqlx::sqlite::SqlitePoolOptions;

    fn snapshot(id: &str, uid: &str, recurrence: Option<&str>) -> CalendarEventSnapshot {
        CalendarEventSnapshot {
            version: "1.0".into(),
            linked_at: "2026-09-01T10:00:00+00:00".into(),
            event: SnapshotEvent {
                id: id.into(),
                source_id: "src".into(),
                ics_uid: uid.into(),
                recurrence_id: recurrence.map(str::to_string),
                summary: Some("Weekly sync".into()),
                description: None,
                location: None,
                organizer: SnapshotOrganizer { name: None, email: None },
                start_at: "2026-09-01T10:00:00+00:00".into(),
                end_at: "2026-09-01T11:00:00+00:00".into(),
                is_all_day: false,
                attendees: vec![Attendee { name: Some("Ann".into()), ..Default::default() }],
            },
        }
    }

    #[tokio::test]
    async fn reattaches_dangling_links_from_snapshots() {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("INSERT INTO calendar_sources (id, url, created_at, updated_at) VALUES ('src', 'https://x', 'x', 'x')")
            .execute(&pool)
            .await
            .unwrap();
        // The occurrence is still in the feed, under a new id.
        sqlx::query(
            "INSERT INTO calendar_events (id, source_id, ics_uid, recurrence_id, start_at, end_at, fetched_at)
             VALUES ('new-id', 'src', 'weekly', '2026-09-08T10:00:00+00:00', '2026-09-08T10:00:00+00:00', '2026-09-08T11:00:00+00:00', 'x')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("m-listed", snapshot("old-1", "weekly", Some("2026-09-08T10:00:00+00:00"))),
            ("m-gone", snapshot("old-2", "weekly", Some("2026-09-01T10:00:00+00:00"))),
        ];
        for (meeting, snap) in &cases {
            let folder = dir.path().join(meeting);
            std::fs::create_dir_all(&folder).unwrap();
            snapshot::write_snapshot(&folder, snap).unwrap();
            MeetingsRepository::create_recording_meeting(&pool, meeting, meeting, Some(folder.to_str().unwrap()))
                .await
                .unwrap();
            sqlx::query("UPDATE meetings SET calendar_event_id = ? WHERE id = ?")
                .bind(&snap.event.id)
                .bind(meeting)
                .execute(&pool)
                .await
                .unwrap();
        }
        // No folder: left alone.
        MeetingsRepository::create_recording_meeting(&pool, "m-nofolder", "x", None).await.unwrap();
        sqlx::query("UPDATE meetings SET calendar_event_id = 'old-3' WHERE id = 'm-nofolder'")
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(repair_dangling_links(&pool).await.unwrap(), 2);
        let listed = CalendarRepository::get_event_for_meeting(&pool, "m-listed").await.unwrap().unwrap();
        assert_eq!(listed.id, "new-id");
        let gone = CalendarRepository::get_event_for_meeting(&pool, "m-gone").await.unwrap().unwrap();
        assert_eq!(gone.id, "old-2", "restored under its original id");
        assert_eq!(gone.ics_uid, "weekly");
        assert_eq!(gone.attendees[0].name.as_deref(), Some("Ann"));
        // Idempotent.
        assert_eq!(repair_dangling_links(&pool).await.unwrap(), 0);
    }

    async fn pool_with_events(ids: &[&str]) -> SqlitePool {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("INSERT INTO calendar_sources (id, url, created_at, updated_at) VALUES ('src', 'https://x', 'x', 'x')")
            .execute(&pool)
            .await
            .unwrap();
        for id in ids {
            sqlx::query(
                "INSERT INTO calendar_events (id, source_id, ics_uid, start_at, end_at, attendees_json, fetched_at)
                 VALUES (?, 'src', ?, '2026-09-01T10:00:00+00:00', '2026-09-01T11:00:00+00:00', '[]', 'x')",
            )
            .bind(id)
            .bind(format!("{id}-uid"))
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    async fn meeting_in(pool: &SqlitePool, dir: &std::path::Path, id: &str) -> PathBuf {
        let folder = dir.join(id);
        std::fs::create_dir_all(&folder).unwrap();
        MeetingsRepository::create_recording_meeting(pool, id, id, Some(folder.to_str().unwrap())).await.unwrap();
        folder
    }

    async fn linked_id(pool: &SqlitePool, meeting: &str) -> Option<String> {
        sqlx::query_scalar("SELECT calendar_event_id FROM meetings WHERE id = ?")
            .bind(meeting)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn snapshot_id(folder: &std::path::Path) -> Option<String> {
        let json = std::fs::read_to_string(folder.join(SNAPSHOT_FILENAME)).ok()?;
        Some(serde_json::from_str::<CalendarEventSnapshot>(&json).unwrap().event.id)
    }

    #[tokio::test]
    async fn unlink_is_not_undone_by_repair() {
        let pool = pool_with_events(&["lunch"]).await;
        let dir = tempfile::tempdir().unwrap();
        let folder = meeting_in(&pool, dir.path(), "m").await;

        link_meeting_with_snapshot(&pool, "m", Some("lunch")).await.unwrap();
        assert_eq!(snapshot_id(&folder).as_deref(), Some("lunch"));

        link_meeting_with_snapshot(&pool, "m", None).await.unwrap();
        assert_eq!(linked_id(&pool, "m").await, None);
        assert!(snapshot_id(&folder).is_none(), "unlink deletes the snapshot");

        // Even if a snapshot survived the unlink (delete failed, or a
        // restore from backup), an unlinked meeting is never repaired.
        let stale = CalendarEventSnapshot::from_event(&CalendarRepository::get_event(&pool, "lunch").await.unwrap().unwrap());
        snapshot::write_snapshot(&folder, &stale).unwrap();
        sqlx::query("DELETE FROM calendar_events WHERE id = 'lunch'").execute(&pool).await.unwrap();
        assert_eq!(repair_dangling_links(&pool).await.unwrap(), 0);
        assert_eq!(linked_id(&pool, "m").await, None);
    }

    #[tokio::test]
    async fn relink_repairs_to_the_new_event_not_the_old_one() {
        let pool = pool_with_events(&["lunch", "review"]).await;
        let dir = tempfile::tempdir().unwrap();
        let folder = meeting_in(&pool, dir.path(), "m").await;

        link_meeting_with_snapshot(&pool, "m", Some("lunch")).await.unwrap();
        link_meeting_with_snapshot(&pool, "m", Some("review")).await.unwrap();
        assert_eq!(snapshot_id(&folder).as_deref(), Some("review"));

        // The new event's row goes missing: repair restores it, not lunch.
        sqlx::query("DELETE FROM calendar_events WHERE id = 'review'").execute(&pool).await.unwrap();
        assert_eq!(repair_dangling_links(&pool).await.unwrap(), 1);
        assert_eq!(linked_id(&pool, "m").await.as_deref(), Some("review"));
        assert_eq!(
            CalendarRepository::get_event_for_meeting(&pool, "m").await.unwrap().unwrap().ics_uid,
            "review-uid"
        );
    }

    #[tokio::test]
    async fn stale_snapshot_from_an_earlier_link_is_ignored() {
        let pool = pool_with_events(&["lunch", "review"]).await;
        let dir = tempfile::tempdir().unwrap();
        let folder = meeting_in(&pool, dir.path(), "m").await;
        link_meeting_with_snapshot(&pool, "m", Some("lunch")).await.unwrap();

        // Relinked in the DB, but the snapshot rewrite never happened.
        CalendarRepository::link_meeting(&pool, "m", Some("review")).await.unwrap();
        assert_eq!(snapshot_id(&folder).as_deref(), Some("lunch"));
        sqlx::query("DELETE FROM calendar_events WHERE id = 'review'").execute(&pool).await.unwrap();
        assert_eq!(repair_dangling_links(&pool).await.unwrap(), 0);
        assert_eq!(linked_id(&pool, "m").await.as_deref(), Some("review"), "left dangling, not sent back to lunch");
    }

    #[tokio::test]
    async fn relink_to_missing_event_drops_old_snapshot() {
        let pool = pool_with_events(&["lunch"]).await;
        let dir = tempfile::tempdir().unwrap();
        let folder = meeting_in(&pool, dir.path(), "m").await;
        link_meeting_with_snapshot(&pool, "m", Some("lunch")).await.unwrap();
        link_meeting_with_snapshot(&pool, "m", Some("gone")).await.unwrap();
        assert!(snapshot_id(&folder).is_none());
    }

    #[tokio::test]
    async fn stop_time_refresh_respects_a_manual_unlink_or_relink() {
        let pool = pool_with_events(&["lunch", "review"]).await;
        let dir = tempfile::tempdir().unwrap();
        let folder = meeting_in(&pool, dir.path(), "m").await;

        assert!(auto_link_meeting(&pool, "m", "lunch").await.unwrap());
        // The user unlinks mid-recording; the stop-time refresh leaves it.
        link_meeting_with_snapshot(&pool, "m", None).await.unwrap();
        assert!(!refresh_link_snapshot(&pool, "m", "lunch").await.unwrap());
        assert_eq!(linked_id(&pool, "m").await, None);
        assert!(snapshot_id(&folder).is_none());

        // A manual link is never replaced by auto-linking.
        link_meeting_with_snapshot(&pool, "m", Some("review")).await.unwrap();
        assert!(!auto_link_meeting(&pool, "m", "lunch").await.unwrap());
        assert!(!refresh_link_snapshot(&pool, "m", "lunch").await.unwrap());
        assert_eq!(linked_id(&pool, "m").await.as_deref(), Some("review"));
        assert_eq!(snapshot_id(&folder).as_deref(), Some("review"));

        // Still linked: the refresh rewrites the snapshot (e.g. into the
        // final folder).
        std::fs::remove_file(folder.join(SNAPSHOT_FILENAME)).unwrap();
        assert!(refresh_link_snapshot(&pool, "m", "review").await.unwrap());
        assert_eq!(snapshot_id(&folder).as_deref(), Some("review"));
    }
}
