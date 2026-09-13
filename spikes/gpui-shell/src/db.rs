//! Read-only access to Parley's real SQLite database, for the summary-editor
//! check. Opens `~/.local/share/io.github.hankanman.Parley/meeting_minutes.sqlite` with
//! `?mode=ro` and never writes to it.

use anyhow::{Context as _, Result, anyhow};
use rusqlite::Connection;
use serde::Deserialize;

#[derive(Deserialize)]
struct SummaryResult {
    markdown: Option<String>,
}

/// Locate the production Parley SQLite DB in the user's data dir.
fn db_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home)
        .join(".local/share/io.github.hankanman.Parley/meeting_minutes.sqlite");
    path.exists().then_some(path)
}

/// Try to read the most recently completed meeting summary's markdown body
/// out of the real database, read-only. Returns `Err` (never panics) on any
/// failure — missing DB, schema drift, no completed summaries, etc. — so the
/// caller can fall back to the bundled fixture.
pub fn read_latest_summary_markdown() -> Result<(String, String)> {
    let path = db_path().ok_or_else(|| anyhow!("meeting_minutes.sqlite not found under HOME"))?;

    // Read-only URI open: never mutates the live app's database, and works
    // even while the real Parley app has it open (shared read lock).
    let uri = format!("file:{}?mode=ro", path.display());
    let conn = Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {uri}"))?;

    // Schema (frontend/src-tauri/migrations/20250916100000_initial_schema.sql):
    //   summary_processes(meeting_id, status, created_at, updated_at, error,
    //                      result, start_time, end_time, chunk_count,
    //                      processing_time, metadata)
    // `result` is JSON with a `markdown` field (see
    // 20251101000000_add_summary_backup.sql for the backup-column sibling).
    let mut stmt = conn
        .prepare(
            "SELECT sp.meeting_id, sp.result, m.title \
             FROM summary_processes sp \
             LEFT JOIN meetings m ON m.id = sp.meeting_id \
             WHERE sp.status = 'completed' AND sp.result IS NOT NULL \
             ORDER BY sp.updated_at DESC LIMIT 1",
        )
        .context("preparing summary_processes query")?;

    let row = stmt
        .query_row([], |row| {
            let meeting_id: String = row.get(0)?;
            let result: String = row.get(1)?;
            let title: Option<String> = row.get(2)?;
            Ok((meeting_id, result, title))
        })
        .context("no completed summary_processes row")?;

    let (meeting_id, result_json, title) = row;
    let parsed: SummaryResult =
        serde_json::from_str(&result_json).context("parsing summary_processes.result JSON")?;
    let markdown = parsed
        .markdown
        .ok_or_else(|| anyhow!("result JSON had no `markdown` field"))?;

    let label = title.unwrap_or(meeting_id);
    Ok((label, markdown))
}
