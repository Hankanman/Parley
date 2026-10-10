-- "Ask Parley": per-meeting chat history (issue #80).
--
-- One row per turn. `role` is 'user' or 'assistant'. Rows are owned by the
-- meeting and go with it: ON DELETE CASCADE (foreign_keys is on for every
-- pool), plus an explicit delete in MeetingsRepository::delete_meeting.
CREATE TABLE IF NOT EXISTS meeting_chat_messages (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    content TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_meeting_chat_messages_meeting
    ON meeting_chat_messages(meeting_id, created_at);
