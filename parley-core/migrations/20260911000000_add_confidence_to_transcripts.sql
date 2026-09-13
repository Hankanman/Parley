-- Persist the live-recording Whisper confidence score alongside each
-- transcript segment so it survives a save/reload, not just the in-memory
-- `transcript-update` event. Nullable: older rows, imports, and
-- retranscription leave it NULL (matches the frontend's "no dot when
-- confidence is undefined" behaviour in VirtualizedTranscriptView.tsx).
ALTER TABLE transcripts ADD COLUMN confidence REAL;
