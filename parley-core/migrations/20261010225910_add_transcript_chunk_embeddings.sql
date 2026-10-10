-- Issue #81: "Ask Parley across meetings" — semantic retrieval.
--
-- Transcripts are cut into chunks of consecutive segments (per meeting) and
-- each chunk's sentence embedding is stored here, so a question can be
-- matched by meaning as well as by FTS5 keywords. Vectors are L2-normalised
-- packed little-endian f32 (same packing as voice_profiles.embedding), so
-- cosine similarity is a plain dot product; retrieval is a brute-force scan,
-- which is fine for tens of thousands of chunks.
--
-- Rows are derived data: they're (re)built lazily in the background by
-- `chat::across::embeddings::backfill` and go with their meeting
-- (ON DELETE CASCADE). `model` records which embedding model produced the
-- vector; switching models simply re-indexes.
CREATE TABLE IF NOT EXISTS transcript_chunk_embeddings (
    meeting_id TEXT NOT NULL,
    chunk_ix INTEGER NOT NULL,
    -- First transcript segment of the chunk and how many consecutive
    -- segments (in transcript display order) it spans.
    first_transcript_id TEXT NOT NULL,
    segment_count INTEGER NOT NULL,
    model TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY (meeting_id, chunk_ix),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_transcript_chunk_embeddings_model
    ON transcript_chunk_embeddings(model);

-- One row per indexed meeting: which model indexed it and a cheap signature
-- of its transcript (segment count, max rowid, total text length) at the
-- time, so the backfill only re-embeds meetings whose transcript changed.
CREATE TABLE IF NOT EXISTS meeting_embedding_state (
    meeting_id TEXT PRIMARY KEY,
    model TEXT NOT NULL,
    signature TEXT NOT NULL,
    chunk_count INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);
