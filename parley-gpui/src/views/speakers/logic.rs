//! Pure logic for the Speakers page: which stored voice profiles to show,
//! in what order, and who a given profile can be merged into. Mirrors
//! `frontend/src/components/SpeakerSettings.tsx`.
//!
//! Also covers the self-voice enrollment section ("Your voice"), which
//! mirrors `frontend/src/components/SelfVoiceEnrollment.tsx`.

use chrono::{DateTime, Utc};
use parley_core::database::models::VoiceProfile;
use parley_core::speaker_diarization::enrollment::EnrollmentProgress;

/// A passage to read aloud while enrolling. This is the opening of the
/// public-domain "Rainbow Passage", long used in speech work because it's
/// phonetically balanced — reading it exercises a broad range of sounds,
/// which gives the speaker-embedding model a fuller picture of the voice
/// than a few off-the-cuff words would. It also just gives the user
/// something to say for ~20s so they don't trail off into silence.
/// Mirrors the React component's `READING_PASSAGE` verbatim.
pub const SELF_VOICE_READING_PASSAGE: &str =
    "When the sunlight strikes raindrops in the air, they act as a prism and \
     form a rainbow. The rainbow is a division of white light into many \
     beautiful colors. These take the shape of a long round arch, with its path \
     high above, and its two ends apparently beyond the horizon.";

/// Profiles the page lists: every stored profile except the self-enrolled
/// one (that's owned by a separate "Your voice" enrollment flow this page
/// doesn't implement — see the module doc comment in `mod.rs`), sorted
/// alphabetically by name, case-insensitively.
pub fn visible_profiles(profiles: &[VoiceProfile]) -> Vec<&VoiceProfile> {
    let mut visible: Vec<&VoiceProfile> = profiles.iter().filter(|p| !p.is_self).collect();
    visible.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    visible
}

/// Everyone `loser_id` could be merged into: every visible profile except
/// itself.
pub fn merge_candidates<'a>(visible: &[&'a VoiceProfile], loser_id: &str) -> Vec<&'a VoiceProfile> {
    visible.iter().copied().filter(|p| p.id != loser_id).collect()
}

/// Best-effort relative date for the "Updated" column, e.g. "11 Sep 2026".
/// Falls back to the raw string on parse failure (matches the frontend's
/// `formatRelative`).
pub fn format_updated(iso: &str) -> String {
    match DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => dt.with_timezone(&Utc).format("%-d %b %Y").to_string(),
        Err(_) => iso.to_string(),
    }
}

/// Seconds remaining before the target enrollment duration is reached,
/// clamped at zero. Mirrors the React component's `remaining` calculation.
pub fn self_voice_remaining_secs(progress: &EnrollmentProgress) -> u32 {
    (progress.target_secs - progress.captured_secs).max(0.0).ceil() as u32
}

/// Whether the enrollment should auto-save now that enough audio has been
/// captured — the happy path needs one click (Record), not two. Mirrors the
/// React progress listener's `captured_secs >= target_secs` check.
pub fn self_voice_should_auto_save(progress: &EnrollmentProgress) -> bool {
    progress.captured_secs >= progress.target_secs
}

/// Whether the name field's "Save" button should show: only once enrolled,
/// and only when the trimmed field differs from the stored label. Before
/// enrolling, the name is applied when the recording is saved, so there's
/// nothing to save separately. Mirrors the React `nameChanged` calculation.
pub fn self_voice_name_changed(enrolled: bool, field_value: &str, stored_name: Option<&str>) -> bool {
    let trimmed = field_value.trim();
    enrolled && !trimmed.is_empty() && trimmed != stored_name.unwrap_or("")
}

/// The "N samples · recorded <date>" subtitle for an enrolled profile.
/// Mirrors the React component's enrolled-state subtitle.
pub fn self_voice_enrolled_subtitle(sample_count: Option<i64>, updated_at: Option<&str>) -> String {
    let count = sample_count.unwrap_or(0);
    let mut text = format!("{} sample{}", count, if count == 1 { "" } else { "s" });
    if let Some(updated_at) = updated_at {
        text.push_str(&format!(" · recorded {}", format_updated(updated_at)));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, name: &str, is_self: bool) -> VoiceProfile {
        VoiceProfile {
            id: id.to_string(),
            name: name.to_string(),
            email: None,
            embedding: Vec::new(),
            embedding_dim: 0,
            sample_count: 0,
            is_self,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn excludes_the_self_profile() {
        let profiles = vec![profile("1", "Me", true), profile("2", "Bob", false)];
        let visible = visible_profiles(&profiles);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "2");
    }

    #[test]
    fn sorts_case_insensitively_by_name() {
        let profiles = vec![profile("1", "bob", false), profile("2", "Alice", false)];
        let visible = visible_profiles(&profiles);
        assert_eq!(visible.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["2", "1"]);
    }

    #[test]
    fn merge_candidates_excludes_the_loser() {
        let profiles = vec![profile("1", "Alice", false), profile("2", "Bob", false)];
        let visible = visible_profiles(&profiles);
        let candidates = merge_candidates(&visible, "1");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "2");
    }

    #[test]
    fn format_updated_parses_rfc3339() {
        assert_eq!(format_updated("2026-07-15T10:30:00Z"), "15 Jul 2026");
    }

    #[test]
    fn format_updated_falls_back_on_parse_failure() {
        assert_eq!(format_updated("not-a-date"), "not-a-date");
    }

    fn progress(captured: f32, target: f32) -> EnrollmentProgress {
        EnrollmentProgress {
            rms_level: 0.0,
            peak_level: 0.0,
            captured_secs: captured,
            target_secs: target,
            can_save: captured >= 10.0,
        }
    }

    #[test]
    fn remaining_secs_counts_down_and_clamps_at_zero() {
        assert_eq!(self_voice_remaining_secs(&progress(5.0, 20.0)), 15);
        assert_eq!(self_voice_remaining_secs(&progress(20.0, 20.0)), 0);
        assert_eq!(self_voice_remaining_secs(&progress(25.0, 20.0)), 0);
    }

    #[test]
    fn should_auto_save_once_target_reached() {
        assert!(!self_voice_should_auto_save(&progress(19.9, 20.0)));
        assert!(self_voice_should_auto_save(&progress(20.0, 20.0)));
        assert!(self_voice_should_auto_save(&progress(21.0, 20.0)));
    }

    #[test]
    fn name_changed_requires_enrolled_and_a_real_difference() {
        assert!(!self_voice_name_changed(false, "Bob", Some("Me")));
        assert!(!self_voice_name_changed(true, "  ", Some("Me")));
        assert!(!self_voice_name_changed(true, "Me", Some("Me")));
        assert!(!self_voice_name_changed(true, "  Me  ", Some("Me")));
        assert!(self_voice_name_changed(true, "Seb", Some("Me")));
        assert!(self_voice_name_changed(true, "Seb", None));
    }

    #[test]
    fn enrolled_subtitle_pluralizes_and_appends_date() {
        assert_eq!(
            self_voice_enrolled_subtitle(Some(1), Some("2026-07-15T10:30:00Z")),
            "1 sample · recorded 15 Jul 2026"
        );
        assert_eq!(
            self_voice_enrolled_subtitle(Some(3), Some("2026-07-15T10:30:00Z")),
            "3 samples · recorded 15 Jul 2026"
        );
        assert_eq!(self_voice_enrolled_subtitle(None, None), "0 samples");
    }
}
