//! Pure logic for the per-segment speaker chip's edit flow — mirrors
//! `frontend/src/components/EditableSpeakerChip.tsx` and
//! `frontend/src/lib/voice-profiles.ts`'s `isUnnamedSpeakerLabel`.
//!
//! Kept free of GPUI/entity types so it's unit-testable without a window.

/// Mirrors the React component's `/^Speaker\s+\d+$/` regex: "Speaker" then
/// one or more whitespace chars then one or more ASCII digits, and nothing
/// else (after trimming).
pub fn is_unnamed_speaker_label(label: &str) -> bool {
    let trimmed = label.trim();
    let Some(rest) = trimmed.strip_prefix("Speaker") else {
        return false;
    };
    // Require at least one whitespace char between "Speaker" and the digits
    // (rejects "Speaker1"), then the rest must be non-empty ASCII digits.
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let digits = rest.trim_start();
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// The local user's fixed, non-editable label (never a stored profile).
pub const ME_LABEL: &str = "Me";

/// Whether clicking this chip should open the edit panel at all — mirrors
/// `EditableSpeakerChip`'s branching: "Me" and anything that's neither a
/// named profile nor an unnamed "Speaker N" cluster render as static spans.
pub fn can_edit_speaker(speaker: &str, voice_profile_id: Option<&str>) -> bool {
    if speaker == ME_LABEL {
        return false;
    }
    let is_named_profile = voice_profile_id.is_some();
    let is_unnamed_cluster = voice_profile_id.is_none() && is_unnamed_speaker_label(speaker);
    is_named_profile || is_unnamed_cluster
}

/// Mirrors `canSave` in the React component: merging into an existing
/// profile needs no new name (the target's own name/email are used), but
/// creating a profile (or renaming an existing one) needs a non-empty name.
pub fn can_save_speaker_edit(merge_target: Option<&str>, name: &str) -> bool {
    merge_target.is_some() || !name.trim().is_empty()
}

/// Panel heading, mirroring the two copies in the React popover.
pub fn edit_panel_title(is_named_profile: bool) -> &'static str {
    if is_named_profile {
        "Edit speaker"
    } else {
        "Name this speaker"
    }
}

/// One calendar attendee reduced to what the rename/promote form needs —
/// mirrors `EditableSpeakerChip.tsx`'s `AttendeeSuggestion`.
#[derive(Clone, Debug, PartialEq)]
pub struct AttendeeSuggestion {
    pub label: String,
    pub email: Option<String>,
}

/// Build the de-duplicated suggestion list from a linked calendar event's
/// raw attendees — mirrors `EditableSpeakerChip.tsx`'s
/// `for (const a of event?.attendees ?? [])` loop: prefer the attendee's
/// name, fall back to their email, skip anyone with neither, and drop
/// case-insensitive duplicate labels.
pub fn attendee_suggestions(attendees: &[parley_core::calendar::models::Attendee]) -> Vec<AttendeeSuggestion> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for a in attendees {
        let label = a
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(a.email.as_deref())
            .unwrap_or("")
            .trim()
            .to_string();
        if label.is_empty() {
            continue;
        }
        if !seen.insert(label.to_lowercase()) {
            continue;
        }
        out.push(AttendeeSuggestion { label, email: a.email.clone() });
    }
    out
}

/// Filter suggestions by the rename form's current name-input text —
/// case-insensitive substring match; an empty (or whitespace-only) query
/// shows every suggestion.
pub fn filter_attendee_suggestions<'a>(
    suggestions: &'a [AttendeeSuggestion],
    query: &str,
) -> Vec<&'a AttendeeSuggestion> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return suggestions.iter().collect();
    }
    suggestions.iter().filter(|s| s.label.to_lowercase().contains(&q)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_unnamed_speaker_labels() {
        assert!(is_unnamed_speaker_label("Speaker 1"));
        assert!(is_unnamed_speaker_label("Speaker 42"));
        assert!(is_unnamed_speaker_label("  Speaker 3  "));
    }

    #[test]
    fn rejects_non_matching_labels() {
        assert!(!is_unnamed_speaker_label("Speaker"));
        assert!(!is_unnamed_speaker_label("Speaker1"));
        assert!(!is_unnamed_speaker_label("Speakers 1"));
        assert!(!is_unnamed_speaker_label("Alice"));
        assert!(!is_unnamed_speaker_label("Me"));
        assert!(!is_unnamed_speaker_label(""));
        assert!(!is_unnamed_speaker_label("Speaker 1a"));
    }

    #[test]
    fn me_is_never_editable() {
        assert!(!can_edit_speaker("Me", None));
        assert!(!can_edit_speaker("Me", Some("profile-1")));
    }

    #[test]
    fn named_profile_is_editable() {
        assert!(can_edit_speaker("Alice", Some("profile-1")));
    }

    #[test]
    fn unnamed_cluster_is_editable() {
        assert!(can_edit_speaker("Speaker 1", None));
    }

    #[test]
    fn stray_label_is_not_editable() {
        assert!(!can_edit_speaker("Unknown", None));
    }

    #[test]
    fn save_requires_name_unless_merging() {
        assert!(!can_save_speaker_edit(None, ""));
        assert!(!can_save_speaker_edit(None, "   "));
        assert!(can_save_speaker_edit(None, "Alice"));
        assert!(can_save_speaker_edit(Some("profile-1"), ""));
    }

    #[test]
    fn panel_title_depends_on_named_profile() {
        assert_eq!(edit_panel_title(true), "Edit speaker");
        assert_eq!(edit_panel_title(false), "Name this speaker");
    }

    fn attendee(name: Option<&str>, email: Option<&str>) -> parley_core::calendar::models::Attendee {
        parley_core::calendar::models::Attendee {
            name: name.map(str::to_string),
            email: email.map(str::to_string),
            role: None,
            status: None,
            is_organizer: false,
        }
    }

    #[test]
    fn attendee_suggestions_prefer_name_over_email() {
        let out = attendee_suggestions(&[attendee(Some("Alice Smith"), Some("alice@example.com"))]);
        assert_eq!(out, vec![AttendeeSuggestion { label: "Alice Smith".into(), email: Some("alice@example.com".into()) }]);
    }

    #[test]
    fn attendee_suggestions_fall_back_to_email_when_unnamed() {
        let out = attendee_suggestions(&[attendee(None, Some("bob@example.com"))]);
        assert_eq!(out, vec![AttendeeSuggestion { label: "bob@example.com".into(), email: Some("bob@example.com".into()) }]);
    }

    #[test]
    fn attendee_suggestions_skip_entries_with_neither_name_nor_email() {
        let out = attendee_suggestions(&[attendee(None, None), attendee(Some("  "), None)]);
        assert!(out.is_empty());
    }

    #[test]
    fn attendee_suggestions_dedupe_case_insensitively() {
        let out = attendee_suggestions(&[
            attendee(Some("Alice Smith"), Some("alice@example.com")),
            attendee(Some("alice smith"), Some("alice2@example.com")),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn filter_attendee_suggestions_empty_query_shows_all() {
        let all = vec![
            AttendeeSuggestion { label: "Alice".into(), email: None },
            AttendeeSuggestion { label: "Bob".into(), email: None },
        ];
        assert_eq!(filter_attendee_suggestions(&all, "").len(), 2);
        assert_eq!(filter_attendee_suggestions(&all, "   ").len(), 2);
    }

    #[test]
    fn filter_attendee_suggestions_matches_case_insensitive_substring() {
        let all = vec![
            AttendeeSuggestion { label: "Alice Smith".into(), email: None },
            AttendeeSuggestion { label: "Bob Jones".into(), email: None },
        ];
        let out = filter_attendee_suggestions(&all, "ali");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Alice Smith");
    }

    #[test]
    fn filter_attendee_suggestions_no_match_is_empty() {
        let all = vec![AttendeeSuggestion { label: "Alice".into(), email: None }];
        assert!(filter_attendee_suggestions(&all, "zzz").is_empty());
    }
}
