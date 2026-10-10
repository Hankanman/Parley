-- Follow-up email drafts and pre-meeting briefs (issue #84).
--
-- meeting_followup_drafts: the latest follow-up email drafted for a meeting,
-- including the user's edits. One row per meeting; regenerating replaces it.
-- Owned by the meeting (ON DELETE CASCADE, plus an explicit delete in
-- MeetingsRepository::delete_meeting).
CREATE TABLE IF NOT EXISTS meeting_followup_drafts (
    meeting_id TEXT PRIMARY KEY,
    -- 'concise' | 'formal' | 'friendly'
    tone TEXT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL,
    -- What the draft was built from: 'structured' | 'summary' | 'transcript'
    source TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

-- meeting_briefs: cached pre-meeting brief per calendar event *instance*.
-- Keyed by source + ICS UID + occurrence start rather than the event row id,
-- so a brief survives the event being re-stored. `fingerprint` hashes the
-- context the brief was generated from; a changed context (a new meeting in
-- the series, an action item closed) makes the cached brief stale.
-- `sources_json` lists the cited meetings ([{meeting_id, title, created_at}]).
CREATE TABLE IF NOT EXISTS meeting_briefs (
    cache_key TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    ics_uid TEXT NOT NULL,
    start_at TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    brief TEXT NOT NULL,
    sources_json TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_meeting_briefs_start ON meeting_briefs(start_at);
