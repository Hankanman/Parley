-- Issue #79: full-text search over transcript segments with SQLite FTS5.
--
-- Design: an *external-content* FTS5 table. The index stores only the
-- tokenised postings; the text itself stays in `transcripts.transcript`, and
-- FTS5 reads it back by rowid when it needs a column value (snippet(),
-- highlight()). That avoids duplicating every transcript on disk.
--
-- Keying: `transcripts` has a TEXT primary key, so FTS5 is tied to the
-- table's implicit `rowid` (content_rowid='rowid'). A hit's rowid joins
-- straight back to the segment row, which carries meeting_id, timestamp and
-- audio_start_time. Caveat: SQLite's VACUUM may renumber implicit rowids. The
-- app never runs VACUUM; if it ever does, follow it with
-- `INSERT INTO transcripts_fts(transcripts_fts) VALUES('rebuild')`
-- (see `database::search::rebuild_index`).
--
-- Tokeniser: unicode61 with diacritics folded, so "cafe" finds "café".
-- No porter stemming: the search box searches as you type, using a prefix
-- query on the last word, and stemmed postings make prefixes of a
-- half-typed word miss (e.g. "discussi" would not match the stem "discuss").
CREATE VIRTUAL TABLE IF NOT EXISTS transcripts_fts USING fts5(
    transcript,
    content = 'transcripts',
    content_rowid = 'rowid',
    tokenize = 'unicode61 remove_diacritics 2'
);

-- Keep the index in sync. External-content tables have to be told about the
-- old value on delete/update ('delete' command), because FTS5 can't read it
-- back from a row that has already changed.
CREATE TRIGGER IF NOT EXISTS transcripts_fts_ai AFTER INSERT ON transcripts BEGIN
    INSERT INTO transcripts_fts (rowid, transcript) VALUES (new.rowid, new.transcript);
END;

CREATE TRIGGER IF NOT EXISTS transcripts_fts_ad AFTER DELETE ON transcripts BEGIN
    INSERT INTO transcripts_fts (transcripts_fts, rowid, transcript)
        VALUES ('delete', old.rowid, old.transcript);
END;

-- Only text changes touch the index: speaker refinement and voice-profile
-- relinking update other columns on many rows and don't need re-indexing.
-- The live-recording upsert (ON CONFLICT ... DO UPDATE) fires this trigger.
CREATE TRIGGER IF NOT EXISTS transcripts_fts_au AFTER UPDATE OF transcript ON transcripts BEGIN
    INSERT INTO transcripts_fts (transcripts_fts, rowid, transcript)
        VALUES ('delete', old.rowid, old.transcript);
    INSERT INTO transcripts_fts (rowid, transcript) VALUES (new.rowid, new.transcript);
END;

-- Backfill every existing segment.
INSERT INTO transcripts_fts (transcripts_fts) VALUES ('rebuild');
