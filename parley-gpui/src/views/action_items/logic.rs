//! Pure logic for the Action Items page: status filtering and grouping by
//! meeting. Kept free of GPUI types so it's cheap to unit test; `mod.rs`
//! adapts `ActionItem`/`MeetingModel` rows and renders what this module
//! produces.
//!
//! Mirrors the frontend's `app/action-items/page.tsx`: within a meeting, open
//! items come before done ones; each group is ordered oldest-first; a
//! meeting whose id isn't in the known meeting list falls into a trailing
//! "Untitled meeting" bucket instead of being dropped.

use std::collections::HashMap;

use parley_core::database::models::ActionItem;

/// The minimal shape the grouping logic needs from a meeting row — just
/// enough to label and order groups, not the full `MeetingModel`.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingRef {
    pub id: String,
    pub title: String,
}

/// Which items to show. Mirrors the frontend's `Filter` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Open,
    Done,
    All,
}

/// Whether `item` should be shown under `filter`.
pub fn matches_filter(item: &ActionItem, filter: Filter) -> bool {
    match filter {
        Filter::All => true,
        Filter::Open => item.status != "done",
        Filter::Done => item.status == "done",
    }
}

/// One meeting's worth of items, in render order.
#[derive(Debug, Clone)]
pub struct Group<'a> {
    pub meeting_id: String,
    pub title: String,
    pub items: Vec<&'a ActionItem>,
}

/// Label used for a group whose `meeting_id` isn't in `meetings` (e.g. the
/// meeting was deleted, or a sync race). Matches the frontend's fallback.
pub const UNKNOWN_MEETING_TITLE: &str = "Untitled meeting";

/// Groups `items` by meeting, ordered the way `meetings` lists them (the
/// sidebar/meeting-list order — newest first), with any orphaned meeting ids
/// appended afterward in a stable (sorted) order. Within each group, open
/// items come first, then done; each half is ordered oldest-created-first.
pub fn group_by_meeting<'a>(items: &'a [ActionItem], meetings: &[MeetingRef]) -> Vec<Group<'a>> {
    let mut by_meeting: HashMap<&str, Vec<&ActionItem>> = HashMap::new();
    for item in items {
        by_meeting.entry(item.meeting_id.as_str()).or_default().push(item);
    }
    for list in by_meeting.values_mut() {
        list.sort_by(|a, b| {
            let a_done = a.status == "done";
            let b_done = b.status == "done";
            a_done.cmp(&b_done).then_with(|| a.created_at.cmp(&b.created_at))
        });
    }

    let mut groups = Vec::new();
    for m in meetings {
        if let Some(items) = by_meeting.remove(m.id.as_str()) {
            groups.push(Group {
                meeting_id: m.id.clone(),
                title: m.title.clone(),
                items,
            });
        }
    }

    let mut orphan_ids: Vec<&str> = by_meeting.keys().copied().collect();
    orphan_ids.sort_unstable();
    for id in orphan_ids {
        if let Some(items) = by_meeting.remove(id) {
            groups.push(Group {
                meeting_id: id.to_string(),
                title: UNKNOWN_MEETING_TITLE.to_string(),
                items,
            });
        }
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, meeting_id: &str, status: &str, created_at: &str) -> ActionItem {
        ActionItem {
            id: id.to_string(),
            meeting_id: meeting_id.to_string(),
            text: format!("do {id}"),
            assignee: None,
            due_hint: None,
            status: status.to_string(),
            source: "manual".to_string(),
            external_ref: None,
            source_start_secs: None,
            source_end_secs: None,
            source_quote: None,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            completed_at: None,
        }
    }

    fn meeting(id: &str, title: &str) -> MeetingRef {
        MeetingRef { id: id.to_string(), title: title.to_string() }
    }

    #[test]
    fn matches_filter_open_excludes_done() {
        assert!(matches_filter(&item("1", "m1", "open", "t1"), Filter::Open));
        assert!(!matches_filter(&item("1", "m1", "done", "t1"), Filter::Open));
    }

    #[test]
    fn matches_filter_done_excludes_open() {
        assert!(matches_filter(&item("1", "m1", "done", "t1"), Filter::Done));
        assert!(!matches_filter(&item("1", "m1", "open", "t1"), Filter::Done));
    }

    #[test]
    fn matches_filter_all_matches_everything() {
        assert!(matches_filter(&item("1", "m1", "done", "t1"), Filter::All));
        assert!(matches_filter(&item("1", "m1", "open", "t1"), Filter::All));
    }

    #[test]
    fn groups_follow_meeting_order() {
        let items = vec![item("1", "m2", "open", "t1"), item("2", "m1", "open", "t1")];
        let meetings = vec![meeting("m1", "Standup"), meeting("m2", "Planning")];
        let groups = group_by_meeting(&items, &meetings);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].meeting_id, "m1");
        assert_eq!(groups[1].meeting_id, "m2");
    }

    #[test]
    fn open_items_sort_before_done_within_a_group() {
        let items = vec![
            item("1", "m1", "done", "t1"),
            item("2", "m1", "open", "t2"),
            item("3", "m1", "open", "t1"),
        ];
        let meetings = vec![meeting("m1", "Standup")];
        let groups = group_by_meeting(&items, &meetings);
        let ids: Vec<&str> = groups[0].items.iter().map(|i| i.id.as_str()).collect();
        // open items first (oldest-created-first: "3" before "2"), done last.
        assert_eq!(ids, ["3", "2", "1"]);
    }

    #[test]
    fn orphan_meeting_falls_into_a_trailing_bucket() {
        let items = vec![item("1", "unknown", "open", "t1"), item("2", "m1", "open", "t1")];
        let meetings = vec![meeting("m1", "Standup")];
        let groups = group_by_meeting(&items, &meetings);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].meeting_id, "m1");
        assert_eq!(groups[1].meeting_id, "unknown");
        assert_eq!(groups[1].title, UNKNOWN_MEETING_TITLE);
    }

    #[test]
    fn empty_items_produce_no_groups() {
        let meetings = vec![meeting("m1", "Standup")];
        assert!(group_by_meeting(&[], &meetings).is_empty());
    }
}
