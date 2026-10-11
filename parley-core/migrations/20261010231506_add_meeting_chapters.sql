-- Issue #87: LLM-generated topic chapters for a meeting.
--
-- One row per chapter, `position` 0-based; `start_seconds` is
-- recording-relative and strictly increasing within a meeting (validated
-- before insert). A regeneration replaces the whole set. Derived data owned
-- by the meeting: ON DELETE CASCADE, plus an explicit delete in
-- MeetingsRepository::delete_meeting.
CREATE TABLE IF NOT EXISTS meeting_chapters (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    start_seconds REAL NOT NULL,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_meeting_chapters_meeting
    ON meeting_chapters(meeting_id, position);
