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
    pub async fn replace_events(
        pool: &SqlitePool,
        source_id: &str,
        occurrences: &[OccurrenceForUpsert],
    ) -> Result<usize, sqlx::Error> {
        let mut tx = pool.begin().await?;

        sqlx::query("DELETE FROM calendar_events WHERE source_id = ?")
            .bind(source_id)
            .execute(&mut *tx)
            .await?;

        let now = Utc::now().to_rfc3339();
        let mut inserted = 0usize;
        for occ in occurrences {
            let id = Uuid::new_v4().to_string();
            let attendees_json =
                serde_json::to_string(&occ.master.attendees).unwrap_or_else(|_| "[]".to_string());
            let recurrence_id = occ.recurrence_id.map(|dt| dt.to_rfc3339());

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

    pub async fn find_event_for_now(
        pool: &SqlitePool,
    ) -> Result<Option<CalendarEvent>, sqlx::Error> {
        let now = Utc::now();
        // Tolerate +/- 5 minutes so a slightly-late record-start still picks
        // up the current event.
        let from = now - chrono::Duration::minutes(15);
        let to = now + chrono::Duration::minutes(5);

        let row = sqlx::query_as::<_, CalendarEventRow>(
            "SELECT id, source_id, ics_uid, recurrence_id, summary, description,
                    location, organizer_name, organizer_email, start_at, end_at,
                    is_all_day, attendees_json, raw_ics, fetched_at
             FROM calendar_events
             WHERE is_all_day = 0
               AND start_at <= ?
               AND end_at >= ?
             ORDER BY start_at DESC
             LIMIT 1",
        )
        .bind(to.to_rfc3339())
        .bind(from.to_rfc3339())
        .fetch_optional(pool)
        .await?;

        Ok(row.and_then(|r| CalendarEvent::try_from(r).ok()))
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
