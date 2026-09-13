-- Transcript-source anchoring for action items.
--
-- Action items are moving to a transcript-sourced extractor: each item is
-- grounded to the moment it was said, so the UI can replay that slice of the
-- recording (verify by ear) and later link back to the exact segment.
--
--   source_start_secs / source_end_secs: recording-relative seconds of the
--     transcript segment the item was extracted from (nullable — legacy items
--     and ungrounded extractions have none).
--   source_quote: the transcript sentence that triggered the item, kept for
--     display/verification.
ALTER TABLE action_items ADD COLUMN source_start_secs REAL;
ALTER TABLE action_items ADD COLUMN source_end_secs REAL;
ALTER TABLE action_items ADD COLUMN source_quote TEXT;
