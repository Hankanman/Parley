-- Calendar sources: each row is one public ICS URL the user has registered.
-- We support multiple sources (work + personal, etc) up front so we don't
-- repaint the schema later if a second is added.
CREATE TABLE IF NOT EXISTS calendar_sources (
    id              TEXT PRIMARY KEY,
    url             TEXT NOT NULL,
    label           TEXT,
    last_fetched_at TEXT,
    last_error      TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_sources_url
    ON calendar_sources(url);

-- Calendar events parsed from each source. Recurring events are expanded
-- into one row per occurrence and identified by (source_id, ics_uid,
-- recurrence_id) so re-fetches upsert cleanly.
CREATE TABLE IF NOT EXISTS calendar_events (
    id               TEXT PRIMARY KEY,
    source_id        TEXT NOT NULL,
    ics_uid          TEXT NOT NULL,
    recurrence_id    TEXT,            -- NULL for non-recurring; ISO-8601 of occurrence start otherwise
    summary          TEXT,
    description      TEXT,
    location         TEXT,
    organizer_name   TEXT,
    organizer_email  TEXT,
    start_at         TEXT NOT NULL,   -- ISO-8601 UTC
    end_at           TEXT NOT NULL,   -- ISO-8601 UTC
    is_all_day       INTEGER NOT NULL DEFAULT 0,
    attendees_json   TEXT NOT NULL DEFAULT '[]',
    raw_ics          TEXT,
    fetched_at       TEXT NOT NULL,
    FOREIGN KEY (source_id) REFERENCES calendar_sources(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_events_source_uid_recur
    ON calendar_events(source_id, ics_uid, COALESCE(recurrence_id, ''));

CREATE INDEX IF NOT EXISTS idx_calendar_events_start
    ON calendar_events(start_at);

-- Link a meeting to the calendar event it was recorded for.
ALTER TABLE meetings ADD COLUMN calendar_event_id TEXT;

CREATE INDEX IF NOT EXISTS idx_meetings_calendar_event
    ON meetings(calendar_event_id);
