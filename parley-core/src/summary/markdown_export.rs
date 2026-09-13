use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::calendar::snapshot::lookup_meeting_folder;

const SUMMARY_FILENAME: &str = "summary.md";

/// Pull the markdown body out of a stored summary `result` JSON. Both the
/// auto-generation path and the manual save path write `{ "markdown":
/// "...", "summary_json": [...] }`, so this is the single shape we read.
pub fn extract_markdown(value: &Value) -> Option<String> {
    value
        .get("markdown")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Write `summary.md` to the meeting's recording folder atomically. No-op
/// when the meeting has no folder (e.g. recordings made with auto_save
/// disabled) or when the folder doesn't exist on disk.
pub fn write_summary_md_to_folder(folder: &Path, markdown: &str) -> Result<()> {
    if !folder.exists() {
        return Err(anyhow::anyhow!(
            "meeting folder does not exist: {}",
            folder.display()
        ));
    }
    let target = folder.join(SUMMARY_FILENAME);
    let tmp = folder.join(format!(".{}.tmp", SUMMARY_FILENAME));
    std::fs::write(&tmp, markdown).context("write summary.md tmp file")?;
    std::fs::rename(&tmp, &target).context("rename summary.md tmp into place")?;
    Ok(())
}

/// Convenience wrapper used by both the auto-generation completion path
/// and the manual-save command. Looks up the meeting folder, extracts
/// markdown from `result_value`, writes it to disk. All failures are
/// returned to the caller — they're expected to log and continue (the
/// DB stays the source of truth).
pub async fn write_summary_md(
    pool: &SqlitePool,
    meeting_id: &str,
    result_value: &Value,
) -> Result<()> {
    let Some(markdown) = extract_markdown(result_value) else {
        return Err(anyhow::anyhow!(
            "summary result has no `markdown` field; nothing to write"
        ));
    };

    let folder = lookup_meeting_folder(pool, meeting_id)
        .await
        .with_context(|| format!("lookup folder for meeting {}", meeting_id))?;

    let Some(folder) = folder else {
        return Err(anyhow::anyhow!(
            "meeting {} has no folder_path (auto_save disabled?)",
            meeting_id
        ));
    };

    write_summary_md_to_folder(&PathBuf::from(folder), &markdown)
}
