-- Drop transcript_chunks: it was a full-text mirror of each meeting's
-- transcript, written solely to match the deleted Python backend's
-- behavior. The canonical `transcripts` table already holds every
-- segment; nothing reads transcript_chunks for summary generation
-- (the summary flow receives transcript text directly from the caller).
DROP TABLE IF EXISTS transcript_chunks;

-- Drop the unused summary/action_items/key_points columns on
-- transcripts. They were never populated by any write path and never
-- read by any query (summaries live in summary_processes.result).
-- Requires SQLite >= 3.35 (bundled by sqlx 0.8); none of these columns
-- are indexed, part of a key, or referenced by a trigger.
ALTER TABLE transcripts DROP COLUMN summary;
ALTER TABLE transcripts DROP COLUMN action_items;
ALTER TABLE transcripts DROP COLUMN key_points;
