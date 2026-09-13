//! Pure logic for the sidebar's meeting list: date-bucket grouping and
//! title/search filtering. Kept free of GPUI/sqlx types so it's cheap to
//! unit test; [`super::AppShell`] adapts `MeetingModel` rows into
//! [`MeetingRow`] and renders the buckets this module produces.

use chrono::{DateTime, Duration, Local, Utc};

/// The minimal shape the grouping/filtering logic needs from a meeting row.
/// A plain struct (not `parley_core::database::models::MeetingModel`) so
/// this module has no dependency on the database layer.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingRow {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

/// Sidebar date buckets, in display order.
pub const GROUP_ORDER: [&str; 4] = ["Today", "Yesterday", "This week", "Older"];

/// Which bucket a meeting's `created_at` falls into, evaluated against `now`
/// in the local timezone (so "Today" matches the user's calendar day).
pub fn group_label(now: DateTime<Local>, created_at: DateTime<Utc>) -> &'static str {
    let created_local = created_at.with_timezone(&Local);
    let today = now.date_naive();
    let created_date = created_local.date_naive();

    if created_date == today {
        "Today"
    } else if created_date == today - Duration::days(1) {
        "Yesterday"
    } else if today - created_date < Duration::days(7) {
        "This week"
    } else {
        "Older"
    }
}

/// Groups `meetings` (assumed already ordered newest-first, as the
/// repository returns them) into date buckets, preserving that order within
/// each bucket and only including buckets that have at least one meeting.
pub fn group_meetings<'a>(
    now: DateTime<Local>,
    meetings: &'a [MeetingRow],
) -> Vec<(&'static str, Vec<&'a MeetingRow>)> {
    let mut buckets: Vec<(&'static str, Vec<&'a MeetingRow>)> =
        GROUP_ORDER.iter().map(|label| (*label, Vec::new())).collect();

    for meeting in meetings {
        let label = group_label(now, meeting.created_at);
        if let Some((_, bucket)) = buckets.iter_mut().find(|(l, _)| *l == label) {
            bucket.push(meeting);
        }
    }

    buckets.retain(|(_, bucket)| !bucket.is_empty());
    buckets
}

/// Case-insensitive title filter. An empty/whitespace-only query matches
/// everything.
pub fn filter_by_title<'a>(meetings: &'a [MeetingRow], query: &str) -> Vec<&'a MeetingRow> {
    let query = query.trim();
    if query.is_empty() {
        return meetings.iter().collect();
    }
    let query = query.to_lowercase();
    meetings
        .iter()
        .filter(|m| m.title.to_lowercase().contains(&query))
        .collect()
}

/// One transcript-content search hit, already reduced to the shape the
/// sidebar renders: which meeting, its title, and a snippet of context
/// around the first match (mirrors the Tauri transcript-search command's
/// `TranscriptSearchResult`, kept as a local plain type so this module has
/// no dependency on the database layer — see [`MeetingRow`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ContentMatch {
    pub meeting_id: String,
    pub title: String,
    pub snippet: String,
}

/// Reduces raw search rows (one per matching transcript segment, as
/// `TranscriptsRepository::search_transcripts` returns them) to at most one
/// [`ContentMatch`] per meeting — the first hit, preserving row order — and
/// drops any meeting whose title already matched the same query (so the
/// sidebar doesn't show a meeting twice).
pub fn build_content_matches(
    rows: impl IntoIterator<Item = ContentMatch>,
    already_shown_ids: &[String],
) -> Vec<ContentMatch> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for row in rows {
        if already_shown_ids.contains(&row.meeting_id) || seen.contains(&row.meeting_id) {
            continue;
        }
        seen.push(row.meeting_id.clone());
        out.push(row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    // `now` is built directly in `Local` (not derived from a UTC instant),
    // and every case keeps `now`/`created` a safe distance from local
    // midnight, so these are correct under whatever timezone the test
    // machine runs in — not just UTC.

    fn utc(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn local(y: i32, m: u32, d: u32, h: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn same_calendar_day_is_today() {
        let now = local(2026, 9, 11, 18);
        let created = now.with_hour(2).unwrap().with_timezone(&Utc);
        assert_eq!(group_label(now, created), "Today");
    }

    #[test]
    fn previous_calendar_day_is_yesterday() {
        let now = local(2026, 9, 11, 12);
        let created = (now - Duration::days(1)).with_timezone(&Utc);
        assert_eq!(group_label(now, created), "Yesterday");
    }

    #[test]
    fn within_a_week_but_not_yesterday_is_this_week() {
        let now = local(2026, 9, 11, 12);
        let created = (now - Duration::days(4)).with_timezone(&Utc);
        assert_eq!(group_label(now, created), "This week");
    }

    #[test]
    fn a_week_or_more_ago_is_older() {
        let now = local(2026, 9, 11, 12);
        let created = (now - Duration::days(10)).with_timezone(&Utc);
        assert_eq!(group_label(now, created), "Older");
    }

    #[test]
    fn exactly_seven_days_ago_is_older_not_this_week() {
        let now = local(2026, 9, 11, 12);
        let created = (now - Duration::days(7)).with_timezone(&Utc);
        assert_eq!(group_label(now, created), "Older");
    }

    fn row(id: &str, title: &str, y: i32, m: u32, d: u32) -> MeetingRow {
        MeetingRow {
            id: id.to_string(),
            title: title.to_string(),
            created_at: utc(y, m, d, 10),
        }
    }

    #[test]
    fn groups_preserve_input_order_within_bucket_and_skip_empty_buckets() {
        let now = local(2026, 9, 11, 12);
        let meetings = vec![
            row("1", "Standup", 2026, 9, 11),
            row("2", "Planning", 2026, 9, 11),
            row("3", "Retro", 2026, 8, 1),
        ];
        let grouped = group_meetings(now, &meetings);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, "Today");
        assert_eq!(grouped[0].1.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["1", "2"]);
        assert_eq!(grouped[1].0, "Older");
        assert_eq!(grouped[1].1[0].id, "3");
    }

    #[test]
    fn filter_by_title_is_case_insensitive_substring() {
        let meetings = vec![row("1", "Team Standup", 2026, 9, 11), row("2", "1:1 with Sam", 2026, 9, 11)];
        let hits = filter_by_title(&meetings, "stand");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "1");
    }

    #[test]
    fn filter_by_title_empty_query_matches_all() {
        let meetings = vec![row("1", "Team Standup", 2026, 9, 11), row("2", "1:1 with Sam", 2026, 9, 11)];
        assert_eq!(filter_by_title(&meetings, "   ").len(), 2);
    }

    fn content_match(id: &str) -> ContentMatch {
        ContentMatch { meeting_id: id.to_string(), title: format!("Meeting {id}"), snippet: "…hit…".to_string() }
    }

    #[test]
    fn build_content_matches_dedupes_by_meeting_keeping_first_hit() {
        let rows = vec![content_match("1"), content_match("1"), content_match("2")];
        let matches = build_content_matches(rows, &[]);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].meeting_id, "1");
        assert_eq!(matches[1].meeting_id, "2");
    }

    #[test]
    fn build_content_matches_excludes_already_shown_meetings() {
        let rows = vec![content_match("1"), content_match("2")];
        let matches = build_content_matches(rows, &["1".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].meeting_id, "2");
    }

    #[test]
    fn build_content_matches_empty_input_is_empty() {
        assert!(build_content_matches(Vec::new(), &[]).is_empty());
    }
}
