pub mod fetcher;
pub mod models;
pub mod parser;
pub mod repository;
pub mod service;
pub mod snapshot;

use repository::CalendarRepository;
use sqlx::SqlitePool;

/// Re-fetch one calendar source's ICS feed, replace its stored events, and
/// record the outcome (`last_fetched_at`/`last_error`) on the source row.
/// Returns the number of events stored on success.
///
/// Shared by the Tauri `calendar_refresh_source` command
/// (`frontend/src-tauri/src/commands/calendar/commands.rs`) and the GPUI
/// Settings → Calendar page (`parley-gpui/src/views/settings/calendar.rs`)
/// so both surfaces refresh a source the same way.
pub async fn refresh_source(pool: &SqlitePool, source_id: &str) -> Result<usize, String> {
    let source = CalendarRepository::get_source(pool, source_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Calendar source {} not found", source_id))?;

    match fetcher::fetch_and_expand(&source.url).await {
        Ok(occurrences) => {
            let count = CalendarRepository::replace_events(pool, source_id, &occurrences)
                .await
                .map_err(|e| e.to_string())?;
            CalendarRepository::mark_source_fetched(pool, source_id, None)
                .await
                .map_err(|e| e.to_string())?;
            Ok(count)
        }
        Err(e) => {
            let msg = e.to_string();
            CalendarRepository::mark_source_fetched(pool, source_id, Some(&msg))
                .await
                .map_err(|e| e.to_string())?;
            Err(msg)
        }
    }
}

/// Validate and normalize a calendar source URL: trims whitespace, rejects
/// empty/non-`http(s)`/`webcal` URLs, and rewrites `webcal://` to
/// `https://` (the scheme calendar publishers use to mean "open in your
/// calendar app", which just maps to an https fetch here).
///
/// Shared by the Tauri `calendar_add_source` command
/// (`frontend/src-tauri/src/commands/calendar/commands.rs`) and the GPUI
/// Settings → Calendar page (`parley-gpui/src/views/settings/calendar.rs`)
/// so both surfaces accept/reject the same URLs.
pub fn normalize_calendar_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("Calendar URL cannot be empty".to_string());
    }
    if !(trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("webcal://"))
    {
        return Err("Calendar URL must start with http(s):// or webcal://".to_string());
    }
    let normalized = if let Some(rest) = trimmed.strip_prefix("webcal://") {
        format!("https://{}", rest)
    } else {
        trimmed.to_string()
    };
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert!(normalize_calendar_url("").is_err());
        assert!(normalize_calendar_url("   ").is_err());
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(normalize_calendar_url("ftp://example.com/cal.ics").is_err());
        assert!(normalize_calendar_url("example.com/cal.ics").is_err());
    }

    #[test]
    fn passes_through_http_https() {
        assert_eq!(
            normalize_calendar_url("https://example.com/cal.ics").unwrap(),
            "https://example.com/cal.ics"
        );
        assert_eq!(
            normalize_calendar_url("http://example.com/cal.ics").unwrap(),
            "http://example.com/cal.ics"
        );
    }

    #[test]
    fn rewrites_webcal_to_https() {
        assert_eq!(
            normalize_calendar_url("webcal://example.com/cal.ics").unwrap(),
            "https://example.com/cal.ics"
        );
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            normalize_calendar_url("  https://example.com/cal.ics  ").unwrap(),
            "https://example.com/cal.ics"
        );
    }
}
