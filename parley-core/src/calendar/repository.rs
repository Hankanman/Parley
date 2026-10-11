use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::fetcher::OccurrenceForUpsert;
use super::models::{CalendarEvent, CalendarEventRow, CalendarSourceRow};

pub struct CalendarRepository;

impl CalendarRepository {
    pub async fn list_sources(pool: &SqlitePool) -> Result<Vec<CalendarSourceRow>, sqlx::Error> {
        sqlx::query_as::<_, CalendarSourceRow>(
            "SELECT id, url, label, last_fetched_at, last_error, created_at, updated_at
             FROM calendar_sources ORDER BY created_at ASC",
        )
        .fetch_all(pool)
        .await
    }

    pub async fn get_source(
        pool: &SqlitePool,
        id: &str,
    ) -> Result<Option<CalendarSourceRow>, sqlx::Error> {
        sqlx::query_as::<_, CalendarSourceRow>(
            "SELECT id, url, label, last_fetched_at, last_error, created_at, updated_at
             FROM calendar_sources WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
    }

    pub async fn add_source(
        pool: &SqlitePool,
        url: &str,
        label: Option<&str>,
    ) -> Result<CalendarSourceRow, sqlx::Error> {
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO calendar_sources (id, url, label, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(url)
        .bind(label)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Ok(CalendarSourceRow {
            id,
            url: url.to_string(),
            label: label.map(|s| s.to_string()),
            last_fetched_at: None,
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub async fn remove_source(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM calendar_sources WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn mark_source_fetched(
        pool: &SqlitePool,
        id: &str,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE calendar_sources
             SET last_fetched_at = ?, last_error = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&now)
        .bind(error)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Replace the full set of events for a source. Public ICS feeds are
    /// authoritative for the source, so a clean replace is safer than
    /// trying to diff individual occurrences (the ICS spec lets feeds
    /// rewrite UIDs / restructure recurrence sets at any time).
    ///
    /// Two things survive the replace:
    ///
    /// - **Events a meeting is linked to** (`meetings.calendar_event_id`)
    ///   are never deleted, even once they've aged out of the feed's
    ///   window (recurring occurrences older than a week do): they're the
    ///   meeting's record of its series (ICS UID) and attendees, which
    ///   pre-meeting briefs and the series filter rely on. Still-listed ones
    ///   are updated in place by the upsert below.
    /// - **Row ids**: an occurrence that was already stored keeps its id
    ///   (matched on UID + recurrence id), so links, reminder de-duplication
    ///   and an open brief don't break on every refresh.
    pub async fn replace_events(
        pool: &SqlitePool,
        source_id: &str,
        occurrences: &[OccurrenceForUpsert],
    ) -> Result<usize, sqlx::Error> {
        let mut tx = pool.begin().await?;

        let existing: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT id, ics_uid, recurrence_id FROM calendar_events WHERE source_id = ?")
                .bind(source_id)
                .fetch_all(&mut *tx)
                .await?;
        let existing_ids: std::collections::HashMap<(String, String), String> = existing
            .into_iter()
            .map(|(id, uid, recurrence)| ((uid, recurrence.unwrap_or_default()), id))
            .collect();

        sqlx::query(
            "DELETE FROM calendar_events WHERE source_id = ?
               AND id NOT IN (SELECT calendar_event_id FROM meetings WHERE calendar_event_id IS NOT NULL)",
        )
        .bind(source_id)
        .execute(&mut *tx)
        .await?;

        let now = Utc::now().to_rfc3339();
        let mut inserted = 0usize;
        for occ in occurrences {
            let recurrence_id = occ.recurrence_id.map(|dt| dt.to_rfc3339());
            let id = existing_ids
                .get(&(occ.master.ics_uid.clone(), recurrence_id.clone().unwrap_or_default()))
                .cloned()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let attendees_json =
                serde_json::to_string(&occ.master.attendees).unwrap_or_else(|_| "[]".to_string());

            sqlx::query(
                "INSERT INTO calendar_events (
                    id, source_id, ics_uid, recurrence_id, summary, description,
                    location, organizer_name, organizer_email, start_at, end_at,
                    is_all_day, attendees_json, raw_ics, fetched_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(source_id, ics_uid, COALESCE(recurrence_id, ''))
                 DO UPDATE SET
                    summary = excluded.summary,
                    description = excluded.description,
                    location = excluded.location,
                    organizer_name = excluded.organizer_name,
                    organizer_email = excluded.organizer_email,
                    start_at = excluded.start_at,
                    end_at = excluded.end_at,
                    is_all_day = excluded.is_all_day,
                    attendees_json = excluded.attendees_json,
                    raw_ics = excluded.raw_ics,
                    fetched_at = excluded.fetched_at",
            )
            .bind(&id)
            .bind(source_id)
            .bind(&occ.master.ics_uid)
            .bind(&recurrence_id)
            .bind(&occ.master.summary)
            .bind(&occ.master.description)
            .bind(&occ.master.location)
            .bind(&occ.master.organizer_name)
            .bind(&occ.master.organizer_email)
            .bind(occ.start_at.to_rfc3339())
            .bind(occ.end_at.to_rfc3339())
            .bind(if occ.master.is_all_day { 1 } else { 0 })
            .bind(&attendees_json)
            .bind(&occ.master.raw_ics)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            inserted += 1;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn list_events_in_range(
        pool: &SqlitePool,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, sqlx::Error> {
        let rows = sqlx::query_as::<_, CalendarEventRow>(
            "SELECT id, source_id, ics_uid, recurrence_id, summary, description,
                    location, organizer_name, organizer_email, start_at, end_at,
                    is_all_day, attendees_json, raw_ics, fetched_at
             FROM calendar_events
             WHERE start_at < ? AND end_at > ?
             ORDER BY start_at ASC",
        )
        .bind(to.to_rfc3339())
        .bind(from.to_rfc3339())
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| match CalendarEvent::try_from(r) {
                Ok(ev) => Some(ev),
                Err(e) => {
                    log::warn!("dropping malformed calendar_events row: {}", e);
                    None
                }
            })
            .collect())
    }

    pub async fn get_event(
        pool: &SqlitePool,
        event_id: &str,
    ) -> Result<Option<CalendarEvent>, sqlx::Error> {
        let row = sqlx::query_as::<_, CalendarEventRow>(
            "SELECT id, source_id, ics_uid, recurrence_id, summary, description,
                    location, organizer_name, organizer_email, start_at, end_at,
                    is_all_day, attendees_json, raw_ics, fetched_at
             FROM calendar_events WHERE id = ?",
        )
        .bind(event_id)
        .fetch_optional(pool)
        .await?;

        Ok(row.and_then(|r| CalendarEvent::try_from(r).ok()))
    }

    pub async fn link_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
        event_id: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();
        let res = sqlx::query(
            "UPDATE meetings SET calendar_event_id = ?, updated_at = ? WHERE id = ?",
        )
        .bind(event_id)
        .bind(now)
        .bind(meeting_id)
        .execute(pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn get_event_for_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<CalendarEvent>, sqlx::Error> {
        let row = sqlx::query_as::<_, CalendarEventRow>(
            "SELECT e.id, e.source_id, e.ics_uid, e.recurrence_id, e.summary, e.description,
                    e.location, e.organizer_name, e.organizer_email, e.start_at, e.end_at,
                    e.is_all_day, e.attendees_json, e.raw_ics, e.fetched_at
             FROM calendar_events e
             JOIN meetings m ON m.calendar_event_id = e.id
             WHERE m.id = ?",
        )
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?;

        Ok(row.and_then(|r| CalendarEvent::try_from(r).ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::models::ParsedEvent;
    use crate::database::repositories::meeting::MeetingsRepository;
    use chrono::TimeZone;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn occ(uid: &str, day: u32) -> OccurrenceForUpsert {
        let start = Utc.with_ymd_and_hms(2026, 9, day, 10, 0, 0).unwrap();
        OccurrenceForUpsert {
            master: ParsedEvent {
                ics_uid: uid.into(),
                recurrence_id: None,
                summary: Some(format!("Event {uid}")),
                description: None,
                location: None,
                organizer_name: None,
                organizer_email: None,
                start_at: start,
                end_at: start + chrono::Duration::hours(1),
                is_all_day: false,
                attendees: vec![],
                rrule_block: None,
                raw_ics: String::new(),
            },
            start_at: start,
            end_at: start + chrono::Duration::hours(1),
            recurrence_id: Some(start),
        }
    }

    async fn ids(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query_as("SELECT ics_uid, id FROM calendar_events ORDER BY start_at")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn refresh_keeps_ids_and_linked_events() {
        let pool = pool().await;
        let source = CalendarRepository::add_source(&pool, "https://example.com/a.ics", None)
            .await
            .unwrap();
        CalendarRepository::replace_events(&pool, &source.id, &[occ("old", 1), occ("standup", 20)])
            .await
            .unwrap();
        let before = ids(&pool).await;

        MeetingsRepository::create_recording_meeting(&pool, "m1", "Old", None)
            .await
            .unwrap();
        CalendarRepository::link_meeting(&pool, "m1", Some(&before[0].1))
            .await
            .unwrap();

        // The linked occurrence has aged out of the feed's window.
        CalendarRepository::replace_events(&pool, &source.id, &[occ("standup", 20), occ("new", 25)])
            .await
            .unwrap();
        let after = ids(&pool).await;
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], before[0], "linked event kept, with its id");
        assert_eq!(after[1], before[1], "re-listed event keeps its id");
        assert_eq!(after[2].0, "new");
        assert!(CalendarRepository::get_event_for_meeting(&pool, "m1")
            .await
            .unwrap()
            .is_some());

        // Unlinked events not in the feed are still dropped.
        CalendarRepository::replace_events(&pool, &source.id, &[]).await.unwrap();
        assert_eq!(ids(&pool).await, vec![before[0].clone()]);
    }
}
