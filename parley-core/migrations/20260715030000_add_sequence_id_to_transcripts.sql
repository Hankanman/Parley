-- Add sequence_id to transcripts.
--
-- Offline speaker refinement (see `speaker_diarization::refinement`) runs
-- after a meeting is saved and needs to update the speaker label of
-- individual rows. Its unit of work is the diarizer's `sequence_id` — the
-- monotonic per-recording counter assigned by the transcription worker and
-- recorded on every embedding — but until now that id was dropped at save
-- time: `save_transcript` mints a fresh `transcript-<uuid>` primary key and
-- never persisted the sequence the segment came from, leaving no way to
-- match an embedding back to the row it produced.
--
-- Nullable by design. Rows written before this migration have no sequence,
-- and the batch import / retranscription paths don't carry a meaningful one
-- either (their diarizer counter indexes pre-transcription audio segments,
-- which skip short/hallucinated chunks and so don't line up 1:1 with saved
-- rows). Refinement simply matches nothing for those rows and leaves them
-- alone, which is the intended degradation.
ALTER TABLE transcripts ADD COLUMN sequence_id INTEGER;

-- Refinement looks rows up strictly as (meeting_id, sequence_id).
CREATE INDEX IF NOT EXISTS idx_transcripts_meeting_sequence
    ON transcripts (meeting_id, sequence_id);
