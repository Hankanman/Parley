-- Persist the audio-stream source ("mic" | "system") on each transcript
-- segment. The recording now stores mic and system on separate channels
-- (mic = left, system = right), and per-segment playback needs to know which
-- stream a segment came from so it can play the clean source channel instead
-- of the mixed-down mono. Nullable: older rows (and imports) leave it NULL and
-- fall back to a mono downmix on playback.
ALTER TABLE transcripts ADD COLUMN source TEXT;
