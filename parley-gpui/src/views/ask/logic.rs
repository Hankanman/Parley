//! Pure helpers for the cross-meeting Ask view: date-filter presets and
//! status text.

use chrono::{DateTime, Datelike, Days, Local, NaiveDate, TimeZone, Utc};
use parley_core::chat::across::{SemanticAvailability, SemanticUse};

/// Date-range presets offered in the filter bar: (key, label).
pub const DATE_PRESETS: &[(&str, &str)] = &[
    ("any", "Any time"),
    ("7", "Past week"),
    ("30", "Past month"),
    ("90", "Past 3 months"),
    ("365", "Past year"),
    ("year", "This year"),
    ("custom", "Custom range…"),
];

/// Local midnight at the start of `day`, as UTC.
fn local_start_of(day: NaiveDate) -> Option<DateTime<Utc>> {
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .earliest()
        .map(|d| d.with_timezone(&Utc))
}

/// `YYYY-MM-DD`, or `None` for blank/invalid input.
pub fn parse_day(input: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(input.trim(), "%Y-%m-%d").ok()
}

/// (from, to) instants for a preset, relative to `today` (local). `to` is
/// exclusive. For `custom`, `from_text`/`to_text` are inclusive local days;
/// unparsable ones are ignored.
pub fn date_range(
    key: &str,
    today: NaiveDate,
    from_text: &str,
    to_text: &str,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let days_back = |n: u64| today.checked_sub_days(Days::new(n)).and_then(local_start_of);
    match key {
        "7" => (days_back(7), None),
        "30" => (days_back(30), None),
        "90" => (days_back(90), None),
        "365" => (days_back(365), None),
        "year" => (NaiveDate::from_ymd_opt(today.year(), 1, 1).and_then(local_start_of), None),
        "custom" => (
            parse_day(from_text).and_then(local_start_of),
            parse_day(to_text)
                .and_then(|d| d.checked_add_days(Days::new(1)))
                .and_then(local_start_of),
        ),
        _ => (None, None),
    }
}

/// One-line description of the semantic stage for the view's header.
pub fn semantic_status_text(availability: &SemanticAvailability, progress: Option<(i64, i64)>) -> String {
    match availability {
        SemanticAvailability::Ready { model_id } => {
            let model = model_id.split_once(':').map_or(model_id.as_str(), |(_, m)| m);
            match progress {
                Some((done, total)) if done < total => {
                    format!("Keyword + semantic search ({model}) · indexing {done}/{total} meetings")
                }
                _ => format!("Keyword + semantic search ({model})"),
            }
        }
        SemanticAvailability::Failing { error, .. } => {
            format!("Keyword search only — embeddings unavailable: {}", truncate(error, 120))
        }
        SemanticAvailability::Off(reason) => format!("Keyword search only ({reason})"),
    }
}

/// Note shown under an answer about how it was retrieved.
pub fn answer_note(semantic: &SemanticUse, fallback: bool, sources: usize) -> Option<String> {
    let mut parts = Vec::new();
    if fallback && sources > 0 {
        parts.push("Nothing matched directly; answered from your most recent meetings.".to_string());
    }
    if let SemanticUse::Unavailable(reason) = semantic {
        if reason.contains("failed") || reason.contains("returned") || reason.contains("backend") {
            parts.push(format!("Semantic search unavailable ({}).", truncate(reason, 100)));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut t: String = text.chars().take(max).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_map_to_ranges() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        assert_eq!(date_range("any", today, "", ""), (None, None));
        let (from, to) = date_range("7", today, "", "");
        assert!(to.is_none());
        let from_local = from.unwrap().with_timezone(&Local);
        assert_eq!(from_local.date_naive(), NaiveDate::from_ymd_opt(2026, 10, 3).unwrap());
        let (from, _) = date_range("year", today, "", "");
        assert_eq!(from.unwrap().with_timezone(&Local).date_naive(), NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    }

    #[test]
    fn custom_range_is_inclusive_and_tolerant() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let (from, to) = date_range("custom", today, " 2026-08-01 ", "2026-08-31");
        assert_eq!(from.unwrap().with_timezone(&Local).date_naive(), NaiveDate::from_ymd_opt(2026, 8, 1).unwrap());
        // The "to" day is included: the bound is the next local midnight.
        assert_eq!(to.unwrap().with_timezone(&Local).date_naive(), NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
        assert_eq!(date_range("custom", today, "August", ""), (None, None));
        assert_eq!(parse_day("2026-02-30"), None);
    }

    #[test]
    fn status_text() {
        let ready = SemanticAvailability::Ready { model_id: "ollama:nomic-embed-text".into() };
        assert_eq!(
            semantic_status_text(&ready, Some((3, 10))),
            "Keyword + semantic search (nomic-embed-text) · indexing 3/10 meetings"
        );
        assert_eq!(semantic_status_text(&ready, Some((10, 10))), "Keyword + semantic search (nomic-embed-text)");
        assert!(semantic_status_text(&SemanticAvailability::Off("x".into()), None).starts_with("Keyword search only"));
    }

    #[test]
    fn notes() {
        assert_eq!(answer_note(&SemanticUse::Used { model_id: "m".into() }, false, 2), None);
        assert!(answer_note(&SemanticUse::Used { model_id: "m".into() }, true, 2).unwrap().contains("recent"));
        assert!(answer_note(&SemanticUse::Unavailable("Ollama embeddings returned 404".into()), false, 1).is_some());
        // "Off" reasons are shown in the header, not under every answer.
        assert_eq!(answer_note(&SemanticUse::Unavailable("turned off in settings".into()), false, 1), None);
    }
}
