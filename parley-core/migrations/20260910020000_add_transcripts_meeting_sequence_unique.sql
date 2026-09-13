-- Issue #57 slice 2: the live-recording path now upserts transcript rows by
-- (meeting_id, sequence_id) as segments stream in, instead of bulk-inserting
-- once at the end of the meeting. `ON CONFLICT(meeting_id, sequence_id)`
-- needs a matching unique index to target.
--
-- SQLite treats each NULL as distinct in a UNIQUE index, so this never
-- collides for the batch/import/retranscription paths (which leave
-- sequence_id NULL) or for older rows saved before sequence tracking —
-- only the live-recording path, which always sets sequence_id, upserts
-- through it. The existing non-unique `idx_transcripts_meeting_sequence`
-- (added for refinement's read lookups) is left in place.
CREATE UNIQUE INDEX IF NOT EXISTS idx_transcripts_meeting_sequence_unique
    ON transcripts (meeting_id, sequence_id);
