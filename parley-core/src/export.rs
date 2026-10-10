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
//! Formats, all built from the same collected [`ExportedMeeting`]:
//! - **markdown** — for pasting into a chat window or a doc. Readable by a
//!   person, and the structure (headings, checkboxes, `[hh:mm:ss] Speaker:`
//!   lines) is exactly what an LLM parses most reliably.
//! - **json** — for programmatic consumers that want fields, not prose.
//! - **srt** / **vtt** — transcript-only subtitles ([`subtitles`]).
//! - **docx** / **pdf** — formatted documents of summary, action items,
//!   notes and transcript, rendered from one shared block model
//!   ([`document`]) so the two can't drift apart.
//!
//! [`folder_sync`] reuses the markdown renderer to keep a front-mattered
//! note per meeting in a user-chosen folder (e.g. an Obsidian vault).

mod document;
mod docx;
pub mod folder_sync;
mod pdf;
mod subtitles;

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
    /// `"markdown"` | `"json"` | `"srt"` | `"vtt"`.
    pub format: String,
    /// Suggested filename, slugified from the title and dated.
    pub filename: String,
    pub content: String,
}

/// A rendered export of any format, text or binary, ready to write to disk.
#[derive(Debug)]
pub struct ExportFile {
    pub meeting_id: String,
    pub title: String,
    pub format: ExportFormat,
    /// Suggested filename, slugified from the title and dated.
    pub filename: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Json,
    Srt,
    Vtt,
    Docx,
    Pdf,
}

impl ExportFormat {
    /// Parse a format name (case-insensitive). `"md"` is accepted for
    /// markdown and `"webvtt"` for vtt.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name.trim().to_lowercase().as_str() {
            "markdown" | "md" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            "srt" => Ok(Self::Srt),
            "vtt" | "webvtt" => Ok(Self::Vtt),
            "docx" => Ok(Self::Docx),
            "pdf" => Ok(Self::Pdf),
            other => Err(format!(
                "Unsupported export format '{other}': expected markdown, json, srt, vtt, docx or pdf"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::Docx => "docx",
            Self::Pdf => "pdf",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            other => other.name(),
        }
    }

    /// Human label for menus and toasts.
    pub fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Json => "JSON",
            Self::Srt => "SRT subtitles",
            Self::Vtt => "WebVTT subtitles",
            Self::Docx => "Word (DOCX)",
            Self::Pdf => "PDF",
        }
    }

    /// Whether the export is UTF-8 text (copyable to the clipboard).
    pub fn is_text(self) -> bool {
        !matches!(self, Self::Docx | Self::Pdf)
    }
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
    /// Seconds from recording start at which the segment ends, when known.
    end_seconds: Option<f64>,
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

/// Build a clean, self-contained text export of a meeting.
///
/// `format` is a text format — `"markdown"`, `"json"`, `"srt"` or `"vtt"`
/// (case-insensitive). Use [`build_export_file`] for DOCX/PDF.
pub async fn build_export(
    pool: &SqlitePool,
    meeting_id: &str,
    format: &str,
) -> Result<ExportResult, String> {
    let format = ExportFormat::parse(format)?;
    if !format.is_text() {
        return Err(format!(
            "'{}' is a binary format; use build_export_file",
            format.name()
        ));
    }
    let file = build_export_file(pool, meeting_id, format).await?;
    Ok(ExportResult {
        meeting_id: file.meeting_id,
        title: file.title,
        format: format.name().to_string(),
        filename: file.filename,
        content: String::from_utf8(file.bytes)
            .map_err(|e| format!("Export produced invalid UTF-8: {e}"))?,
    })
}

/// Build an export of a meeting in any [`ExportFormat`], as bytes.
///
/// Rendering (DOCX packing, PDF layout and font loading) is CPU-bound, so it
/// runs on the blocking pool rather than an async worker.
pub async fn build_export_file(
    pool: &SqlitePool,
    meeting_id: &str,
    format: ExportFormat,
) -> Result<ExportFile, String> {
    let data = collect(pool, meeting_id).await?;
    let filename = export_filename(&data.title, &data.created_at, format);
    let title = data.title.clone();

    let bytes = tokio::task::spawn_blocking(move || render(&data, format))
        .await
        .map_err(|e| format!("Export task failed: {e}"))??;

    Ok(ExportFile {
        meeting_id: meeting_id.to_string(),
        title,
        format,
        filename,
        bytes,
    })
}

fn render(data: &ExportedMeeting, format: ExportFormat) -> Result<Vec<u8>, String> {
    Ok(match format {
        ExportFormat::Markdown => render_markdown(data).into_bytes(),
        ExportFormat::Json => serde_json::to_string_pretty(data)
            .map_err(|e| format!("Failed to serialize export: {e}"))?
            .into_bytes(),
        ExportFormat::Srt => {
            subtitles::render_srt(&subtitles::build_cues(&data.transcript)?).into_bytes()
        }
        ExportFormat::Vtt => {
            subtitles::render_vtt(&subtitles::build_cues(&data.transcript)?).into_bytes()
        }
        ExportFormat::Docx => {
            docx::render_docx(&document::build_document(data), &data.created_at)?
        }
        ExportFormat::Pdf => pdf::render_pdf(&document::build_document(data), &data.title)?,
    })
}

fn export_filename(title: &str, created_at: &str, format: ExportFormat) -> String {
    format!(
        "{}-{}.{}",
        slugify(title),
        created_at.chars().take(10).collect::<String>(),
        format.extension()
    )
}

/// `created_at` (RFC 3339) as a readable local date-time, falling back to
/// the raw string if it doesn't parse.
fn human_date(created_at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%-d %B %Y, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| created_at.to_string())
}

/// Calendar attendees by display name (email when unnamed), in invite order.
fn attendee_names(m: &ExportedMeeting) -> Vec<String> {
    let Some(event) = &m.calendar_event else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for a in &event.attendees {
        let label = a
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .or_else(|| a.email.as_deref().map(str::trim).filter(|e| !e.is_empty()));
        if let Some(label) = label {
            if !names.iter().any(|n| n == label) {
                names.push(label.to_string());
            }
        }
    }
    names
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
            end_seconds: s.audio_end_time,
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

    out.push_str(&render_markdown_body(m));
    out
}

/// Everything after the title and metadata list: calendar event, summary,
/// action items, notes, transcript. Shared with [`folder_sync`], whose notes
/// carry the metadata as YAML front-matter instead.
fn render_markdown_body(m: &ExportedMeeting) -> String {
    let mut out = String::new();

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

    pub(super) fn sample() -> ExportedMeeting {
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
                end_seconds: Some(7.5),
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

    #[test]
    fn format_parsing_and_filenames() {
        assert_eq!(ExportFormat::parse(" MD ").unwrap(), ExportFormat::Markdown);
        assert_eq!(ExportFormat::parse("WebVTT").unwrap(), ExportFormat::Vtt);
        assert!(ExportFormat::parse("rtf").is_err());
        assert!(ExportFormat::Srt.is_text());
        assert!(!ExportFormat::Pdf.is_text());
        assert_eq!(
            export_filename("Weekly Sync", "2026-07-15T10:00:00+00:00", ExportFormat::Vtt),
            "weekly-sync-2026-07-15.vtt"
        );
        assert_eq!(
            export_filename("Weekly Sync", "2026-07-15T10:00:00+00:00", ExportFormat::Markdown),
            "weekly-sync-2026-07-15.md"
        );
    }

    #[test]
    fn every_format_renders_from_the_sample() {
        let m = sample();
        let srt = String::from_utf8(render(&m, ExportFormat::Srt).unwrap()).unwrap();
        assert_eq!(srt, "1\n00:00:05,000 --> 00:00:07,500\nMe: Hello there\n\n");
        let vtt = String::from_utf8(render(&m, ExportFormat::Vtt).unwrap()).unwrap();
        assert!(vtt.starts_with("WEBVTT\n\n00:00:05.000 --> 00:00:07.500\n<v Me>Hello there</v>"));
        let docx = render(&m, ExportFormat::Docx).unwrap();
        assert!(docx.starts_with(b"PK"));
        let json = docx_rs::read_docx(&docx).unwrap().json();
        assert!(json.contains("Send the budget"));
        assert!(json.contains("Shipped the thing"));
    }

    /// Visual check, not an assertion: writes `sample.{pdf,docx,srt,vtt,md}`
    /// for a summary using every markdown feature plus non-Latin scripts.
    /// `PARLEY_EXPORT_SAMPLE_DIR=/tmp/x cargo test -p parley-core
    /// export_visual_sample -- --ignored`
    #[test]
    #[ignore]
    fn export_visual_sample() {
        let dir = std::env::var("PARLEY_EXPORT_SAMPLE_DIR").expect("set PARLEY_EXPORT_SAMPLE_DIR");
        let mut m = sample();
        m.summary_markdown = Some(
            "## Key Decisions\n\n\
             - Ship **v2** on *Friday* — ~~Thursday~~ moved\n  \
               - Nested: see [the doc](https://example.com) and `config.toml`\n\
             - Budget approved\n\n\
             ## Next Steps\n\n\
             1. Draft the plan\n2. Review with the team\n\n\
             - [x] Book the room\n- [ ] Send invites\n\n\
             | **Owner** | **Action** | **Due Date** |\n| --- | --- | --- |\n\
             | Seb | Write the long migration guide that wraps across lines | Fri |\n\
             | Aiko | 会議の議事録を共有する | 月曜日 |\n\n\
             > Customer quote: \"this is exactly what we needed\"\n\n\
             ```\nfn main() {\n    println!(\"hi\");\n}\n```\n\n---\n\n\
             Multilingual: 中文会议记录, مرحبا بالعالم, Ελληνικά, Привет, emoji ✅ 🎉"
                .to_string(),
        );
        m.transcript.push(ExportedSegment {
            start_seconds: Some(9.0),
            end_seconds: Some(12.0),
            start_label: Some(format_timestamp(9.0)),
            speaker: Some("Speaker 1".to_string()),
            text: "Sounds good — let's <ship> it & move on.".to_string(),
        });
        for format in [
            ExportFormat::Pdf,
            ExportFormat::Docx,
            ExportFormat::Srt,
            ExportFormat::Vtt,
            ExportFormat::Markdown,
        ] {
            let bytes = render(&m, format).unwrap();
            std::fs::write(format!("{dir}/sample.{}", format.extension()), bytes).unwrap();
        }
    }

    #[test]
    fn document_model_covers_every_section() {
        let blocks = document::build_document(&sample());
        assert_eq!(blocks[0], document::Block::Title("Weekly Sync".to_string()));
        let headings: Vec<String> = blocks
            .iter()
            .filter_map(|b| match b {
                document::Block::Heading { level: 1, spans } => Some(document::spans_text(spans)),
                _ => None,
            })
            .collect();
        assert_eq!(headings, ["Summary", "Action Items", "Transcript"]);
        assert!(blocks.iter().any(|b| matches!(
            b,
            document::Block::ListItem { marker: document::ListMarker::Task(true), .. }
        )));
    }
}
