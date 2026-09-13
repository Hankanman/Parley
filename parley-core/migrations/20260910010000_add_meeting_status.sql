-- Issue #57 slice 2: Rust owns the meeting row lifecycle instead of the
-- frontend creating the row only after a live recording finishes.
--
-- status: "recording" (row created when `start_recording*` begins capture),
-- "completed" (normal stop finalised it), or "interrupted" (a fatal-error
-- stop, or a row still "recording" when the app starts back up after a
-- crash — see `MeetingsRepository::mark_stale_recording_meetings_interrupted`).
--
-- Existing rows predate lifecycle tracking entirely and are assumed to have
-- finished normally.
ALTER TABLE meetings ADD COLUMN status TEXT NOT NULL DEFAULT 'completed';
ALTER TABLE meetings ADD COLUMN completed_at TEXT;
ALTER TABLE meetings ADD COLUMN duration_seconds REAL;
ALTER TABLE meetings ADD COLUMN audio_path TEXT;

-- The recovery dialog lists rows by status; the crash-marker sweep at
-- startup updates every "recording" row.
CREATE INDEX IF NOT EXISTS idx_meetings_status ON meetings(status);
