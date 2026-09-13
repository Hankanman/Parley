-- Generic key-value store for namespaced JSON settings blobs. Replaces the
-- tauri-plugin-store JSON files (onboarding-status.json,
-- recording_preferences.json) and the hand-rolled
-- ~/.config/meetily/notifications.json file with a single SQLite-backed
-- store, consistent with the existing `settings` / `transcript_settings`
-- tables. Frontend UI config (language, confidence indicator toggle, etc.)
-- also lives here instead of localStorage.
CREATE TABLE IF NOT EXISTS app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
