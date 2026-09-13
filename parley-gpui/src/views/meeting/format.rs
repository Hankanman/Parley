//! Pure formatting helpers for the meeting page: transcript timestamps and
//! the flattened transcript text sent to summary generation. Mirrors
//! `frontend/src/hooks/meeting-details/useSummaryGeneration.ts`'s
//! `formatTime`/`fullTranscript` so a generated summary reads the same
//! whether it was requested from the Tauri UI or this one.

use chrono::{DateTime, Utc};
use parley_core::database::models::MeetingTranscript;

/// Recording-relative `[MM:SS]` timestamp when `audio_start_time` is known,
/// otherwise the transcript's wall-clock `timestamp` string as-is (older
/// rows / imports predate audio-relative timestamps).
pub fn segment_timestamp(audio_start_time: Option<f64>, fallback_timestamp: &str) -> String {
    match audio_start_time {
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
            let total = seconds.floor() as u64;
            format!("[{:02}:{:02}]", total / 60, total % 60)
        }
        _ => fallback_timestamp.to_string(),
    }
}

/// Flattens transcript segments into the plain-text transcript summary
/// generation consumes: one `<timestamp> <text>` line per segment.
pub fn build_transcript_text(transcripts: &[MeetingTranscript]) -> String {
    transcripts
        .iter()
        .map(|t| format!("{} {}", segment_timestamp(t.audio_start_time, &t.timestamp), t.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Header date, e.g. "11 Sep 2026, 14:03" in the meeting's local timezone.
pub fn format_header_date(created_at: DateTime<Utc>) -> String {
    created_at.with_timezone(&chrono::Local).format("%-d %b %Y, %H:%M").to_string()
}

/// Clipboard text for "Copy transcript". Mirrors
/// `frontend/src/hooks/meeting-details/useCopyOperations.ts`'s
/// `handleCopyTranscript`: a `#`/`##` header, then one `[MM:SS] Speaker: text`
/// line per segment with a trailing two-space markdown line break.
pub fn copy_transcript_text(
    meeting_id: &str,
    title: &str,
    created_at: Option<DateTime<Utc>>,
    transcripts: &[MeetingTranscript],
) -> String {
    let header = format!("# Transcript of the Meeting: {meeting_id} - {title}\n\n");
    let date = format!(
        "## Date: {}\n\n",
        created_at.map(|d| d.with_timezone(&chrono::Local).format("%-m/%-d/%Y").to_string()).unwrap_or_default()
    );
    let lines = transcripts
        .iter()
        .map(|t| {
            let time = segment_timestamp(t.audio_start_time, &t.timestamp);
            let who = t.speaker.as_ref().map(|s| format!("{s}: ")).unwrap_or_default();
            format!("{time} {who}{}  ", t.text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}{date}{lines}")
}

/// Clipboard text for "Copy summary". Mirrors `useCopyOperations.ts`'s
/// `handleCopySummary`: a title, a metadata block (meeting id, meeting date,
/// copy time), a rule, then the summary markdown as-is.
pub fn copy_summary_text(
    meeting_id: &str,
    title: &str,
    created_at: Option<DateTime<Utc>>,
    summary_markdown: &str,
) -> String {
    let header = format!("# Meeting Summary: {title}\n\n");
    let fmt = |d: DateTime<Utc>| d.with_timezone(&chrono::Local).format("%B %-d, %Y, %I:%M %p").to_string();
    let created = created_at.map(fmt).unwrap_or_default();
    let copied = fmt(Utc::now());
    let metadata =
        format!("**Meeting ID:** {meeting_id}\n**Date:** {created}\n**Copied on:** {copied}\n\n---\n\n");
    format!("{header}{metadata}{summary_markdown}")
}

/// Picks the default summary template for the generate/regenerate picker:
/// the stored default setting if it names a template that actually exists,
/// else `"standard_meeting"` if that exists, else the first available
/// template, else `"standard_meeting"` regardless (so a caller always gets
/// *some* id to pass to summary generation even if the template list
/// couldn't be loaded yet).
pub fn resolve_default_template(configured: Option<&str>, available: &[String]) -> String {
    if let Some(configured) = configured {
        if available.iter().any(|id| id == configured) {
            return configured.to_string();
        }
    }
    if available.iter().any(|id| id == "standard_meeting") {
        return "standard_meeting".to_string();
    }
    available.first().cloned().unwrap_or_else(|| "standard_meeting".to_string())
}

/// Whether the summary editor is open (`editing`) with text that differs
/// from the last-saved markdown — the condition the navigation-away guard
/// checks before letting the user leave the meeting page or switch to a
/// different meeting.
pub fn has_unsaved_summary_edits(editing: bool, editor_text: &str, saved_markdown: &str) -> bool {
    editing && editor_text != saved_markdown
}

// ---- Confidence indicator -------------------------------------------------
// Mirrors `frontend/src/components/ConfidenceIndicator.tsx`'s thresholds and
// labels exactly (including its `>= 0.4` "Medium" cutoff, despite the
// component's own comment claiming "below 50%").

/// One of the four confidence bands `ConfidenceIndicator.tsx` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfidenceLevel {
    High,
    Good,
    Medium,
    Low,
}

impl ConfidenceLevel {
    /// `conf` is the raw 0.0-1.0 Whisper confidence score.
    pub fn for_confidence(conf: f32) -> Self {
        if conf >= 0.8 {
            ConfidenceLevel::High
        } else if conf >= 0.7 {
            ConfidenceLevel::Good
        } else if conf >= 0.4 {
            ConfidenceLevel::Medium
        } else {
            ConfidenceLevel::Low
        }
    }

    /// Mirrors `getConfidenceLabel`.
    pub fn label(self) -> &'static str {
        match self {
            ConfidenceLevel::High => "High confidence",
            ConfidenceLevel::Good => "Good confidence",
            ConfidenceLevel::Medium => "Medium confidence",
            ConfidenceLevel::Low => "Low confidence",
        }
    }
}

/// Tooltip text, e.g. "87% confidence - High confidence" — mirrors the
/// `title` attribute `ConfidenceIndicator.tsx` sets on its dot.
pub fn confidence_tooltip(conf: f32) -> String {
    let percent = (conf * 100.0).round() as i64;
    format!("{percent}% confidence - {}", ConfidenceLevel::for_confidence(conf).label())
}

// ---- Meeting notes ---------------------------------------------------------
// Mirrors `MeetingNotesPanel.tsx`'s `formatNoteTime`.

/// "Sep 11, 2:30 PM"-style note timestamp, or `""` for an unparseable
/// timestamp (matches the React component's `Number.isNaN` guard).
pub fn format_note_time(created_at: &str) -> String {
    match DateTime::parse_from_rfc3339(created_at) {
        Ok(dt) => dt.with_timezone(&chrono::Local).format("%b %-d, %-I:%M %p").to_string(),
        Err(_) => String::new(),
    }
}

// ---- Summary typewriter reveal ---------------------------------------------
// There's no existing React "typewriter" for `summary-stream` specifically
// (see `useSummaryStream.ts`, which appends deltas as-is) — this adapts the
// pacing algorithm `useTranscriptStreaming.ts` uses for live transcript
// segments (50ms tick, `max(2, ceil(remaining / ticks))` chars per tick)
// into a delta-buffering model, since the summary case streams over an
// unbounded duration rather than one fixed-length segment.

/// Reveal tick interval, matching `useTranscriptStreaming.ts`'s `INTERVAL_MS`.
pub const SUMMARY_REVEAL_INTERVAL_MS: u64 = 50;

/// How many characters to reveal per tick for a buffer of `pending_len`
/// characters not yet shown. Always reveals at least 1 char/tick so a small
/// trailing buffer still drains instead of stalling.
pub fn summary_reveal_chars_per_tick(pending_len: usize) -> usize {
    if pending_len == 0 {
        return 0;
    }
    // Same shape as useTranscriptStreaming's `Math.max(2, Math.ceil(remaining / totalTicks))`,
    // sized against one tick's worth of the *current* pending buffer so a
    // burst of deltas (e.g. after a network stall) catches up within a
    // handful of ticks instead of trickling out at a fixed rate forever.
    const TARGET_TICKS: usize = 8;
    (pending_len.div_ceil(TARGET_TICKS)).max(2).min(pending_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_from_audio_start_time() {
        assert_eq!(segment_timestamp(Some(65.0), "2026-09-11T00:00:00Z"), "[01:05]");
        assert_eq!(segment_timestamp(Some(0.0), "x"), "[00:00]");
        assert_eq!(segment_timestamp(Some(3599.9), "x"), "[59:59]");
    }

    #[test]
    fn falls_back_to_wall_clock_timestamp() {
        assert_eq!(segment_timestamp(None, "2026-09-11T00:00:00Z"), "2026-09-11T00:00:00Z");
        assert_eq!(segment_timestamp(Some(-1.0), "fallback"), "fallback");
    }

    fn seg(text: &str, audio_start_time: Option<f64>) -> MeetingTranscript {
        MeetingTranscript {
            id: "1".into(),
            text: text.into(),
            timestamp: "2026-09-11T00:00:00Z".into(),
            audio_start_time,
            audio_end_time: None,
            duration: None,
            speaker: None,
            voice_profile_id: None,
            source: None,
            confidence: None,
        }
    }

    #[test]
    fn builds_one_line_per_segment() {
        let transcripts = vec![seg("Hello", Some(0.0)), seg("World", Some(65.0))];
        assert_eq!(build_transcript_text(&transcripts), "[00:00] Hello\n[01:05] World");
    }

    #[test]
    fn empty_transcript_list_is_empty_string() {
        assert_eq!(build_transcript_text(&[]), "");
    }

    #[test]
    fn copy_transcript_includes_header_date_and_speaker_lines() {
        let mut a = seg("Hello", Some(0.0));
        a.speaker = Some("Alice".to_string());
        let transcripts = vec![a, seg("World", Some(65.0))];
        let created = DateTime::parse_from_rfc3339("2026-07-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let text = copy_transcript_text("meeting-1", "Weekly Sync", Some(created), &transcripts);
        assert!(text.starts_with("# Transcript of the Meeting: meeting-1 - Weekly Sync\n\n"));
        assert!(text.contains("## Date:"));
        assert!(text.contains("[00:00] Alice: Hello  "));
        assert!(text.contains("[01:05] World  "));
    }

    #[test]
    fn copy_summary_includes_metadata_and_body_verbatim() {
        let created = DateTime::parse_from_rfc3339("2026-07-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let text = copy_summary_text("meeting-1", "Weekly Sync", Some(created), "## Key Points\n\n- Shipped");
        assert!(text.starts_with("# Meeting Summary: Weekly Sync\n\n"));
        assert!(text.contains("**Meeting ID:** meeting-1\n"));
        assert!(text.contains("**Date:**"));
        assert!(text.contains("**Copied on:**"));
        assert!(text.ends_with("---\n\n## Key Points\n\n- Shipped"));
    }

    #[test]
    fn resolve_default_template_uses_configured_when_available() {
        let available = vec!["standard_meeting".to_string(), "one_on_one".to_string()];
        assert_eq!(resolve_default_template(Some("one_on_one"), &available), "one_on_one");
    }

    #[test]
    fn resolve_default_template_falls_back_to_standard_meeting() {
        let available = vec!["standard_meeting".to_string(), "one_on_one".to_string()];
        assert_eq!(resolve_default_template(Some("missing"), &available), "standard_meeting");
        assert_eq!(resolve_default_template(None, &available), "standard_meeting");
    }

    #[test]
    fn resolve_default_template_falls_back_to_first_available() {
        let available = vec!["one_on_one".to_string(), "retro".to_string()];
        assert_eq!(resolve_default_template(None, &available), "one_on_one");
    }

    #[test]
    fn resolve_default_template_falls_back_to_standard_meeting_id_when_nothing_loaded() {
        assert_eq!(resolve_default_template(None, &[]), "standard_meeting");
    }

    #[test]
    fn no_unsaved_edits_when_not_editing() {
        assert!(!has_unsaved_summary_edits(false, "changed", "original"));
    }

    #[test]
    fn no_unsaved_edits_when_text_matches_saved() {
        assert!(!has_unsaved_summary_edits(true, "same", "same"));
    }

    #[test]
    fn unsaved_edits_when_editing_and_text_differs() {
        assert!(has_unsaved_summary_edits(true, "changed", "original"));
    }

    #[test]
    fn confidence_level_thresholds_match_react() {
        assert_eq!(ConfidenceLevel::for_confidence(1.0), ConfidenceLevel::High);
        assert_eq!(ConfidenceLevel::for_confidence(0.8), ConfidenceLevel::High);
        assert_eq!(ConfidenceLevel::for_confidence(0.79), ConfidenceLevel::Good);
        assert_eq!(ConfidenceLevel::for_confidence(0.7), ConfidenceLevel::Good);
        assert_eq!(ConfidenceLevel::for_confidence(0.69), ConfidenceLevel::Medium);
        assert_eq!(ConfidenceLevel::for_confidence(0.4), ConfidenceLevel::Medium);
        assert_eq!(ConfidenceLevel::for_confidence(0.39), ConfidenceLevel::Low);
        assert_eq!(ConfidenceLevel::for_confidence(0.0), ConfidenceLevel::Low);
    }

    #[test]
    fn confidence_tooltip_matches_react_format() {
        assert_eq!(confidence_tooltip(0.873), "87% confidence - High confidence");
        assert_eq!(confidence_tooltip(0.35), "35% confidence - Low confidence");
    }

    #[test]
    fn format_note_time_renders_valid_rfc3339() {
        let formatted = format_note_time("2026-09-11T14:30:00Z");
        // Exact wall-clock text depends on the local timezone offset, but it
        // must parse to something non-empty in the expected shape.
        assert!(!formatted.is_empty());
    }

    #[test]
    fn format_note_time_is_empty_for_unparseable_input() {
        assert_eq!(format_note_time("not-a-date"), "");
    }

    #[test]
    fn summary_reveal_chars_per_tick_drains_small_buffers_fully() {
        assert_eq!(summary_reveal_chars_per_tick(0), 0);
        assert_eq!(summary_reveal_chars_per_tick(1), 1);
        assert_eq!(summary_reveal_chars_per_tick(2), 2);
    }

    #[test]
    fn summary_reveal_chars_per_tick_scales_with_backlog() {
        // 80 pending chars / 8 target ticks = 10 chars/tick.
        assert_eq!(summary_reveal_chars_per_tick(80), 10);
        // Below the floor of 2 chars/tick, still never exceeds what's pending.
        assert_eq!(summary_reveal_chars_per_tick(3), 2);
        assert_eq!(summary_reveal_chars_per_tick(1), 1);
    }
}
