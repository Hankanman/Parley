//! Self-contained meeting export, in markdown or JSON. Tauri-free: the
//! Tauri shell's `export_meeting`/`export_meeting_to_file` commands
//! (`frontend/src-tauri/src/api/export.rs`) are thin wrappers around
//! [`build_export`] plus (for the "to file" variant) a save-file dialog;
//! the GPUI shell calls [`build_export`] directly.
//!
//! The point is a single document you can hand to another tool — an LLM, a
//! ticket tracker, a teammate — without it needing access to this app's
//! database. So everything the app knows about a meeting goes in one payload:
//! title, when it happened, who was there (calendar attendees *and* the
//! speakers actually heard), the summary, the tracked action items with their
//! status, the notes, and the full speaker-attributed transcript.
//!
//! Two formats, same content:
//! - **markdown** — for pasting into a chat window or a doc. Readable by a
//!   person, and the structure (headings, checkboxes, `[hh:mm:ss] Speaker:`
//!   lines) is exactly what an LLM parses most reliably.
//! - **json** — for programmatic consumers that want fields, not prose.

use crate::calendar::repository::CalendarRepository;
use crate::database::models::{ActionItem, MeetingNote};
use crate::database::repositories::action_item::ActionItemsRepository;
use crate::database::repositories::meeting::MeetingsRepository;
use crate::database::repositories::meeting_note::MeetingNotesRepository;
use crate::database::repositories::summary::SummaryProcessesRepository;
use crate::summary::markdown_export::extract_markdown;
use crate::utils::format_timestamp;
use serde::Serialize;
use sqlx::SqlitePool;

/// Upper bound on transcript rows pulled into one export. Far above any real
/// meeting (a 3-hour recording lands in the low thousands of segments); exists
/// so a corrupt row count can't try to materialize an unbounded string.
const MAX_TRANSCRIPT_SEGMENTS: i64 = 100_000;

#[derive(Debug, Serialize)]
pub struct ExportResult {
    pub meeting_id: String,
    pub title: String,
    /// `"markdown"` | `"json"`.
    pub format: String,
    /// Suggested filename, slugified from the title and dated.
    pub filename: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
struct ExportedAttendee {
    name: Option<String>,
    email: Option<String>,
    status: Option<String>,
    is_organizer: bool,
}

#[derive(Debug, Serialize)]
struct ExportedCalendarEvent {
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    organizer_name: Option<String>,
    organizer_email: Option<String>,
    start_at: String,
    end_at: String,
    attendees: Vec<ExportedAttendee>,
}

#[derive(Debug, Serialize)]
struct ExportedActionItem {
    id: String,
    text: String,
    status: String,
    assignee: Option<String>,
    due_hint: Option<String>,
    source: String,
    completed_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExportedSegment {
    /// Seconds from recording start. Absent on imported/legacy transcripts.
    start_seconds: Option<f64>,
    /// `start_seconds` as `hh:mm:ss`, via the same formatter the UI uses.
    start_label: Option<String>,
    speaker: Option<String>,
    text: String,
}

#[derive(Debug, Serialize)]
struct ExportedNote {
    body: String,
    source: String,
    created_at: String,
}

/// The complete JSON export. Also the intermediate the markdown renderer walks,
/// so the two formats can't drift apart in content.
#[derive(Debug, Serialize)]
struct ExportedMeeting {
    id: String,
    title: String,
    created_at: String,
    updated_at: String,
    exported_at: String,
    /// Recording length in seconds, derived from the last segment's end time.
    duration_seconds: Option<f64>,
    calendar_event: Option<ExportedCalendarEvent>,
    /// Distinct speaker labels heard in the recording, in first-appearance
    /// order. Complements calendar attendees: who *spoke*, not who was invited.
    speakers: Vec<String>,
    summary_markdown: Option<String>,
    action_items: Vec<ExportedActionItem>,
    notes: Vec<ExportedNote>,
    transcript: Vec<ExportedSegment>,
}

/// Build a clean, self-contained export of a meeting.
///
/// `format` is `"markdown"` or `"json"` (case-insensitive).
pub async fn build_export(
    pool: &SqlitePool,
    meeting_id: &str,
    format: &str,
) -> Result<ExportResult, String> {
    let format = format.trim().to_lowercase();
    if format != "markdown" && format != "json" {
        return Err(format!(
            "Unsupported export format '{format}': expected 'markdown' or 'json'"
        ));
    }

    let data = collect(pool, meeting_id).await?;

    let content = if format == "json" {
        serde_json::to_string_pretty(&data)
            .map_err(|e| format!("Failed to serialize export: {e}"))?
    } else {
        render_markdown(&data)
    };

    let extension = if format == "json" { "json" } else { "md" };
    let filename = format!(
        "{}-{}.{}",
        slugify(&data.title),
        &data.created_at.chars().take(10).collect::<String>(),
        extension
    );

    Ok(ExportResult {
        meeting_id: meeting_id.to_string(),
        title: data.title,
        format,
        filename,
        content,
    })
}

/// Gather every piece of the meeting. Each optional source (summary, calendar
/// link, notes) degrades to absent rather than failing the export — a meeting
/// with no summary should still export its transcript.
async fn collect(pool: &SqlitePool, meeting_id: &str) -> Result<ExportedMeeting, String> {
    let meeting = MeetingsRepository::get_meeting_metadata(pool, meeting_id)
        .await
        .map_err(|e| format!("Failed to load meeting: {e}"))?
        .ok_or_else(|| format!("Meeting not found: {meeting_id}"))?;

    let (segments, _total) =
        MeetingsRepository::get_meeting_transcripts_paginated(pool, meeting_id, MAX_TRANSCRIPT_SEGMENTS, 0)
            .await
            .map_err(|e| format!("Failed to load transcripts: {e}"))?;

    let action_items = ActionItemsRepository::list_by_meeting(pool, meeting_id)
        .await
        .map_err(|e| format!("Failed to load action items: {e}"))?;

    let notes = MeetingNotesRepository::list_by_meeting(pool, meeting_id)
        .await
        .map_err(|e| format!("Failed to load notes: {e}"))?;

    let summary_markdown = SummaryProcessesRepository::get_summary_data_for_meeting(pool, meeting_id)
        .await
        .ok()
        .flatten()
        .and_then(|p| p.result)
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| extract_markdown(&v))
        .filter(|m| !m.trim().is_empty());

    let calendar_event = CalendarRepository::get_event_for_meeting(pool, meeting_id)
        .await
        .ok()
        .flatten()
        .map(|e| ExportedCalendarEvent {
            summary: e.summary,
            description: e.description,
            location: e.location,
            organizer_name: e.organizer_name,
            organizer_email: e.organizer_email,
            start_at: e.start_at.to_rfc3339(),
            end_at: e.end_at.to_rfc3339(),
            attendees: e
                .attendees
                .into_iter()
                .map(|a| ExportedAttendee {
                    name: a.name,
                    email: a.email,
                    status: a.status,
                    is_organizer: a.is_organizer,
                })
                .collect(),
        });

    // First-appearance order, not alphabetical: it reads as "who showed up".
    let mut speakers: Vec<String> = Vec::new();
    for s in &segments {
        if let Some(name) = s.speaker.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if !speakers.iter().any(|existing| existing.as_str() == name) {
                speakers.push(name.to_string());
            }
        }
    }

    let duration_seconds = segments
        .iter()
        .filter_map(|s| s.audio_end_time)
        .fold(None::<f64>, |acc, t| Some(acc.map_or(t, |a: f64| a.max(t))));

    let transcript = segments
        .into_iter()
        .map(|s| ExportedSegment {
            start_seconds: s.audio_start_time,
            start_label: s.audio_start_time.map(format_timestamp),
            speaker: s.speaker,
            text: s.transcript,
        })
        .collect();

    Ok(ExportedMeeting {
        id: meeting.id,
        title: meeting.title,
        created_at: meeting.created_at.0.to_rfc3339(),
        updated_at: meeting.updated_at.0.to_rfc3339(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        duration_seconds,
        calendar_event,
        speakers,
        summary_markdown,
        action_items: action_items.into_iter().map(to_exported_item).collect(),
        notes: notes.into_iter().map(to_exported_note).collect(),
        transcript,
    })
}

fn to_exported_item(i: ActionItem) -> ExportedActionItem {
    ExportedActionItem {
        id: i.id,
        text: i.text,
        status: i.status,
        assignee: i.assignee,
        due_hint: i.due_hint,
        source: i.source,
        completed_at: i.completed_at,
    }
}

fn to_exported_note(n: MeetingNote) -> ExportedNote {
    ExportedNote {
        body: n.body,
        source: n.source,
        created_at: n.created_at,
    }
}

/// Render the export as markdown. Sections with nothing in them are omitted
/// entirely rather than left as empty headings — an "## Action Items" followed
/// by nothing reads as data loss to both a person and a model.
fn render_markdown(m: &ExportedMeeting) -> String {
    let mut out = String::new();

    out.push_str(&format!("# {}\n\n", m.title));

    out.push_str(&format!("- **Date:** {}\n", m.created_at));
    if let Some(d) = m.duration_seconds {
        out.push_str(&format!("- **Duration:** {}\n", format_timestamp(d)));
    }
    if !m.speakers.is_empty() {
        out.push_str(&format!("- **Speakers:** {}\n", m.speakers.join(", ")));
    }
    out.push_str(&format!("- **Meeting ID:** {}\n", m.id));
    out.push_str(&format!("- **Exported:** {}\n", m.exported_at));

    if let Some(event) = &m.calendar_event {
        out.push_str("\n## Calendar Event\n\n");
        if let Some(s) = &event.summary {
            out.push_str(&format!("- **Event:** {s}\n"));
        }
        out.push_str(&format!("- **When:** {} → {}\n", event.start_at, event.end_at));
        if let Some(l) = &event.location {
            out.push_str(&format!("- **Location:** {l}\n"));
        }
        if let Some(o) = &event.organizer_name {
            out.push_str(&format!("- **Organizer:** {}\n", with_email(o, &event.organizer_email)));
        }
        if !event.attendees.is_empty() {
            out.push_str("\n### Invited\n\n");
            for a in &event.attendees {
                let label = a
                    .name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| a.email.clone())
                    .unwrap_or_else(|| "Unknown".to_string());
                let mut line = format!("- {}", with_email(&label, &a.email));
                if a.is_organizer {
                    line.push_str(" _(organizer)_");
                }
                if let Some(s) = &a.status {
                    line.push_str(&format!(" — {}", s.to_lowercase()));
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
        if let Some(d) = &event.description {
            let d = d.trim();
            if !d.is_empty() {
                out.push_str(&format!("\n### Event Description\n\n{d}\n"));
            }
        }
    }

    if let Some(summary) = &m.summary_markdown {
        out.push_str("\n## Summary\n\n");
        out.push_str(summary.trim());
        out.push('\n');
    }

    if !m.action_items.is_empty() {
        out.push_str("\n## Action Items\n\n");
        for item in &m.action_items {
            let checkbox = if item.status == "done" { "[x]" } else { "[ ]" };
            let mut line = format!("- {} {}", checkbox, item.text);
            if let Some(a) = &item.assignee {
                line.push_str(&format!(" — **{a}**"));
            }
            if let Some(d) = &item.due_hint {
                line.push_str(&format!(" _({d})_"));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }

    if !m.notes.is_empty() {
        out.push_str("\n## Notes\n\n");
        for note in &m.notes {
            out.push_str(&format!("- {}\n", note.body.replace('\n', "\n  ")));
        }
    }

    if !m.transcript.is_empty() {
        out.push_str("\n## Transcript\n\n");
        for seg in &m.transcript {
            let mut prefix = String::new();
            if let Some(label) = &seg.start_label {
                prefix.push_str(&format!("[{label}] "));
            }
            if let Some(speaker) = &seg.speaker {
                prefix.push_str(&format!("**{speaker}:** "));
            }
            out.push_str(&format!("{}{}\n\n", prefix, seg.text.trim()));
        }
    }

    out
}

/// `Name <email>` when an email is known and isn't already the label.
fn with_email(label: &str, email: &Option<String>) -> String {
    match email {
        Some(e) if !e.trim().is_empty() && e != label => format!("{label} <{e}>"),
        _ => label.to_string(),
    }
}

/// Filesystem-safe stem from a meeting title. Non-alphanumerics collapse to
/// single dashes; the result is capped so a rambling auto-generated title can't
/// blow past filename length limits.
fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true; // leading dashes suppressed
    for c in title.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "meeting".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ExportedMeeting {
        ExportedMeeting {
            id: "meeting-1".to_string(),
            title: "Weekly Sync".to_string(),
            created_at: "2026-07-15T10:00:00+00:00".to_string(),
            updated_at: "2026-07-15T11:00:00+00:00".to_string(),
            exported_at: "2026-07-15T12:00:00+00:00".to_string(),
            duration_seconds: Some(125.0),
            calendar_event: None,
            speakers: vec!["Me".to_string(), "Speaker 1".to_string()],
            summary_markdown: Some("## Key Points\n\n- Shipped the thing".to_string()),
            action_items: vec![
                ExportedActionItem {
                    id: "action-1".to_string(),
                    text: "Send the budget".to_string(),
                    status: "open".to_string(),
                    assignee: Some("Seb".to_string()),
                    due_hint: Some("by Friday".to_string()),
                    source: "summary".to_string(),
                    completed_at: None,
                },
                ExportedActionItem {
                    id: "action-2".to_string(),
                    text: "Book the room".to_string(),
                    status: "done".to_string(),
                    assignee: None,
                    due_hint: None,
                    source: "manual".to_string(),
                    completed_at: Some("2026-07-15T11:30:00+00:00".to_string()),
                },
            ],
            notes: vec![],
            transcript: vec![ExportedSegment {
                start_seconds: Some(5.0),
                start_label: Some(format_timestamp(5.0)),
                speaker: Some("Me".to_string()),
                text: "Hello there".to_string(),
            }],
        }
    }

    #[test]
    fn markdown_includes_every_section_with_content() {
        let md = render_markdown(&sample());
        assert!(md.starts_with("# Weekly Sync"));
        assert!(md.contains("- **Speakers:** Me, Speaker 1"));
        assert!(md.contains("## Summary"));
        assert!(md.contains("- Shipped the thing"));
        assert!(md.contains("## Action Items"));
        assert!(md.contains("- [ ] Send the budget — **Seb** _(by Friday)_"));
        assert!(md.contains("- [x] Book the room"));
        assert!(md.contains("## Transcript"));
        assert!(md.contains("[00:00:05] **Me:** Hello there"));
    }

    #[test]
    fn markdown_omits_empty_sections() {
        let mut m = sample();
        m.summary_markdown = None;
        m.action_items.clear();
        m.transcript.clear();
        let md = render_markdown(&m);
        assert!(!md.contains("## Summary"));
        assert!(!md.contains("## Action Items"));
        assert!(!md.contains("## Transcript"));
        assert!(!md.contains("## Notes"));
        // Metadata still present — an empty meeting is still identifiable.
        assert!(md.contains("- **Meeting ID:** meeting-1"));
    }

    #[test]
    fn slugify_handles_punctuation_and_emptiness() {
        assert_eq!(slugify("Weekly Sync"), "weekly-sync");
        assert_eq!(slugify("  Q3 // Budget: review!  "), "q3-budget-review");
        assert_eq!(slugify("***"), "meeting");
        assert_eq!(slugify(""), "meeting");
    }

    #[test]
    fn slugify_caps_length() {
        let long = "a very long meeting title ".repeat(20);
        assert!(slugify(&long).len() <= 60);
    }

    #[test]
    fn with_email_avoids_duplicating_the_label() {
        assert_eq!(
            with_email("Seb", &Some("seb@example.com".to_string())),
            "Seb <seb@example.com>"
        );
        assert_eq!(
            with_email("seb@example.com", &Some("seb@example.com".to_string())),
            "seb@example.com"
        );
        assert_eq!(with_email("Seb", &None), "Seb");
    }

    #[test]
    fn json_export_round_trips() {
        let json = serde_json::to_string(&sample()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["title"], "Weekly Sync");
        assert_eq!(parsed["action_items"][0]["assignee"], "Seb");
        assert_eq!(parsed["transcript"][0]["speaker"], "Me");
    }
}
