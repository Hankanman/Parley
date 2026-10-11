-- Issue #87: bookmarks marked during a recording ("this moment matters").
--
-- `offset_seconds` is recording-relative (same clock as
-- transcripts.audio_start_time). `label` is an optional short user note,
-- editable later. Rows are owned by the meeting: ON DELETE CASCADE, plus an
-- explicit delete in MeetingsRepository::delete_meeting.
CREATE TABLE IF NOT EXISTS meeting_bookmarks (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL,
    offset_seconds REAL NOT NULL,
    label TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_meeting_bookmarks_meeting
    ON meeting_bookmarks(meeting_id, offset_seconds);
