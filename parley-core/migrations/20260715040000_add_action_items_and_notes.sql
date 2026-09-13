-- Structured work-tracking: action items and meeting notes.
--
-- Until now, action items existed only as free-text inside the markdown
-- summary (summary_processes.result). This makes them first-class rows so
-- they can be queried/completed via the MCP server, shown as checkable tasks
-- in the UI, and later pushed to an external tracker (TickTick) via
-- external_ref.

CREATE TABLE IF NOT EXISTS action_items (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL,
    -- The task text, e.g. "Send the Q3 budget to Finance".
    text TEXT NOT NULL,
    -- Extracted owner when the summary named one ("Seb to ..."); nullable.
    assignee TEXT,
    -- Free-text due hint as spoken ("by Friday", "next sprint"); nullable.
    -- Not a parsed date — kept as-is for the user / downstream tracker.
    due_hint TEXT,
    -- Lifecycle: 'open' | 'done'.
    status TEXT NOT NULL DEFAULT 'open',
    -- Provenance: 'summary' (auto-extracted), 'manual' (user), 'agent' (MCP).
    source TEXT NOT NULL DEFAULT 'summary',
    -- Opaque id of a linked task in an external tracker (e.g. TickTick),
    -- populated when the item is pushed there. NULL until then.
    external_ref TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    completed_at TEXT,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_action_items_meeting ON action_items(meeting_id);
CREATE INDEX IF NOT EXISTS idx_action_items_status ON action_items(status);

-- An earlier migration (20251223000000) created a meeting_notes table with a
-- different, one-row-per-meeting shape (notes_markdown / notes_json) for a
-- notes feature that was never wired to any reader or writer — no code path
-- references notes_markdown/notes_json, and it holds no data. Drop that dead
-- table so the CREATE below actually takes effect (a bare
-- CREATE TABLE IF NOT EXISTS would silently no-op against the old shape).
DROP INDEX IF EXISTS idx_meeting_notes_meeting_id;
DROP TABLE IF EXISTS meeting_notes;

CREATE TABLE meeting_notes (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL,
    body TEXT NOT NULL,
    -- 'manual' (user in the UI) | 'agent' (written via the MCP server).
    source TEXT NOT NULL DEFAULT 'manual',
    created_at TEXT NOT NULL,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX idx_meeting_notes_meeting ON meeting_notes(meeting_id);
