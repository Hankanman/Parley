-- Speaker attribution & voice profiles
--
-- The existing `transcripts.speaker` column (added in 20251110000001) holds
-- the human-readable speaker label assigned at transcription time
-- ("Me", "Speaker 1", "Speaker 2", or a stored profile name).
--
-- This migration adds the link from a transcript row to a stored voice
-- profile and creates the `voice_profiles` table that holds the speaker
-- embeddings used to recognize a returning speaker in future meetings.

ALTER TABLE transcripts ADD COLUMN voice_profile_id TEXT;

CREATE TABLE IF NOT EXISTS voice_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    -- Packed little-endian f32 speaker embedding (centroid across all samples).
    embedding BLOB NOT NULL,
    -- Dimensionality of `embedding` so we can validate at read time without
    -- baking the model choice into the schema.
    embedding_dim INTEGER NOT NULL,
    -- How many segments contributed to the centroid; used for incremental
    -- averaging when a new sample is added.
    sample_count INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_transcripts_voice_profile_id
    ON transcripts(voice_profile_id);
