//! Follow-up email drafts and pre-meeting briefs (issue #84).
//!
//! - [`email`]: a ready-to-send recap email for one meeting, drafted from its
//!   structured summary (or its edited Markdown summary, or the transcript)
//!   in a chosen [`email::Tone`]. Plain text only; the latest draft per
//!   meeting is stored in `meeting_followup_drafts`. Nothing here ever sends
//!   mail or builds a `mailto:` link: copying is the only export.
//! - [`brief`]: before an upcoming calendar event, gathers past meetings in
//!   the same series (ICS UID) and with the same people (calendar attendees
//!   and voice profiles), their open action items and their latest decisions
//!   and open questions, and asks the model for a short brief citing those
//!   meetings. Cached per event instance in `meeting_briefs`.
//!
//! Both use the summary provider/model, and both treat model output as
//! untrusted: the email is plain text, the brief is rendered with the same
//! escape-then-link approach as "Ask Parley".

pub mod brief;
pub mod email;

use crate::calendar::models::Attendee;
use serde::Deserialize;
use sqlx::SqlitePool;

/// The local user, as far as Parley knows them: the `is_self` voice profile.
/// Used to leave the user out of "who else is in this meeting".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SelfIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl SelfIdentity {
    pub async fn load(pool: &SqlitePool) -> Result<Self, sqlx::Error> {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT name, email FROM voice_profiles WHERE is_self = 1 LIMIT 1")
                .fetch_optional(pool)
                .await?;
        Ok(row
            .map(|(name, email)| Self {
                name: Some(name.trim().to_string()).filter(|n| !n.is_empty()),
                email: email
                    .map(|e| e.trim().to_lowercase())
                    .filter(|e| !e.is_empty()),
            })
            .unwrap_or_default())
    }

    /// Whether `attendee` is the user (by email, else by name).
    pub fn is(&self, attendee: &Attendee) -> bool {
        if let (Some(mine), Some(theirs)) = (&self.email, attendee.email.as_deref()) {
            if mine.eq_ignore_ascii_case(theirs.trim()) {
                return true;
            }
        }
        match (&self.name, attendee.name.as_deref()) {
            (Some(mine), Some(theirs)) => mine.eq_ignore_ascii_case(theirs.trim()),
            _ => false,
        }
    }
}

/// Whether a calendar attendee is a person, rather than a calendar, room or
/// mailbox the invite was sent to: Google lists the calendar an event lives
/// in (`…@group.calendar.google.com`) and booked rooms
/// (`…@resource.calendar.google.com`) as attendees, and those would make
/// every meeting on that calendar look related.
pub fn is_person(attendee: &Attendee) -> bool {
    if attendee
        .role
        .as_deref()
        .is_some_and(|r| r.trim().eq_ignore_ascii_case("NON-PARTICIPANT"))
    {
        return false;
    }
    let Some(email) = attendee
        .email
        .as_deref()
        .map(|e| e.trim().to_ascii_lowercase())
    else {
        return true;
    };
    let local = email.split('@').next().unwrap_or_default();
    !(email.ends_with("calendar.google.com")
        || email.ends_with(".calendar.icloud.com")
        || local.starts_with("noreply")
        || local.starts_with("no-reply"))
}

/// How to refer to an attendee: their name, else their email address.
pub fn attendee_label(attendee: &Attendee) -> Option<String> {
    attendee
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or_else(|| {
            attendee
                .email
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
        })
        .map(|s| one_line(s, 80))
}

/// What a meeting's stored summary offers, read from
/// `summary_processes.result` (`{"markdown", "structured"?}`).
///
/// An edited summary is saved as Markdown only, so `structured` is present
/// exactly when it still matches what the user sees.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoredSummary {
    pub structured: Option<StoredStructured>,
    pub markdown: Option<String>,
}

/// The core fields of a stored structured summary (see
/// [`crate::summary::structured::StructuredSummary`]). Raw model output:
/// untrusted text.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct StoredStructured {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub overview: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub action_items: Vec<StoredActionItem>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct StoredActionItem {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub due: Option<String>,
}

impl StoredStructured {
    /// True when nothing in it is worth building on.
    pub fn is_empty(&self) -> bool {
        self.overview.trim().is_empty()
            && self.decisions.iter().all(|d| d.trim().is_empty())
            && self.action_items.iter().all(|a| a.text.trim().is_empty())
            && self.open_questions.iter().all(|q| q.trim().is_empty())
    }
}

/// Parse a `summary_processes.result` JSON string. Anything malformed reads
/// as "no summary".
pub fn parse_summary_result(result: &str) -> StoredSummary {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(result) else {
        return StoredSummary::default();
    };
    let markdown = value
        .get("markdown")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let structured = value
        .get("structured")
        .filter(|s| s.is_object())
        .and_then(|s| serde_json::from_value::<StoredStructured>(s.clone()).ok())
        .filter(|s| !s.is_empty());
    StoredSummary {
        structured,
        markdown,
    }
}

/// Load a meeting's stored summary (empty when it has none).
pub async fn load_summary(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<StoredSummary, sqlx::Error> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT result FROM summary_processes WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;
    Ok(row
        .and_then(|(result,)| result)
        .map(|r| parse_summary_result(&r))
        .unwrap_or_default())
}

/// One line of untrusted text for a prompt or a plain-text output: control
/// characters dropped, whitespace collapsed, capped at `max_chars`.
pub fn one_line(text: &str, max_chars: usize) -> String {
    let collapsed = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let mut out: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        out.push('…');
        out
    }
}

/// Stable 64-bit FNV-1a hash, hex-encoded. Used as a cache fingerprint (not
/// security): unlike `DefaultHasher` it doesn't change between Rust releases.
pub fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The summary provider/model's [`LlmConfig`](crate::summary::llm_client::LlmConfig),
/// or a user-facing error when none is configured.
pub(crate) async fn resolve_llm(
    pool: &SqlitePool,
) -> Result<crate::summary::llm_client::LlmConfig, String> {
    use crate::database::repositories::setting::SettingsRepository;
    let model = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| format!("failed to read model settings: {e}"))?
        .filter(|m| !m.provider.is_empty() && !m.model.is_empty())
        .ok_or_else(|| "No model configured — set one up in Settings first.".to_string())?;
    crate::summary::llm_client::LlmConfig::resolve(
        pool,
        &model.provider,
        &model.model,
        crate::paths::app_data_dir().ok(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_and_markdown() {
        let s = parse_summary_result(
            r###"{"markdown":"## Summary\nok","structured":{"title":"T","overview":"O",
                "decisions":["Ship"],"action_items":[{"text":"Send deck","owner":"Ann","due":null}],
                "open_questions":[],"sections":[],"notes":[]}}"###,
        );
        assert_eq!(s.markdown.as_deref(), Some("## Summary\nok"));
        let st = s.structured.unwrap();
        assert_eq!(st.decisions, vec!["Ship".to_string()]);
        assert_eq!(st.action_items[0].owner.as_deref(), Some("Ann"));
    }

    #[test]
    fn edited_or_legacy_summaries_have_no_structured_part() {
        let s = parse_summary_result(r#"{"markdown":"edited"}"#);
        assert!(s.structured.is_none());
        assert_eq!(s.markdown.as_deref(), Some("edited"));
        let empty = parse_summary_result(r#"{"markdown":"  ","structured":{"overview":""}}"#);
        assert_eq!(empty, StoredSummary::default());
        assert_eq!(parse_summary_result("not json"), StoredSummary::default());
    }

    #[test]
    fn calendars_rooms_and_mailboxes_are_not_people() {
        let a = |email: &str, role: Option<&str>| Attendee {
            name: Some("X".into()),
            email: Some(email.into()),
            role: role.map(str::to_string),
            ..Default::default()
        };
        assert!(is_person(&a("ann@example.com", Some("REQ-PARTICIPANT"))));
        assert!(!is_person(&a("abc@group.calendar.google.com", None)));
        assert!(!is_person(&a("room1@resource.calendar.google.com", None)));
        assert!(!is_person(&a("noreply@example.com", None)));
        assert!(!is_person(&a("ann@example.com", Some("non-participant"))));
        assert!(is_person(&Attendee {
            name: Some("Ann".into()),
            ..Default::default()
        }));
    }

    #[test]
    fn one_line_collapses_and_caps() {
        assert_eq!(one_line("a\n\tb\u{7}  c", 50), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(""), "cbf29ce484222325");
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }
}
