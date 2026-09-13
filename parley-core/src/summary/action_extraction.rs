//! Turns a finished meeting summary into structured action-item rows.
//!
//! ## Where this runs
//!
//! As the *fallback* path of the on-demand `extract_action_items` command,
//! for meetings that have a summary but no stored transcript segments. The
//! automatic post-summary pass uses the transcript-sourced extractor
//! ([`super::transcript_action_items`]) instead — it grounds items to real
//! timestamps, which this summary-based path can't.
//!
//! ## Why a second LLM call
//!
//! The summary already *contains* action items, but as prose inside markdown
//! ("**Action Items**\n- Seb to send the budget by Friday"). Parsing that with
//! regexes means chasing every template and phrasing variation forever. Asking
//! the model to re-read its own summary and emit JSON costs one cheap call and
//! yields owner/due fields the markdown only implies.
//!
//! ## Trusting the output
//!
//! Not at all — [`parse_action_items`] treats the response as hostile text that
//! probably contains JSON somewhere. Models wrap arrays in prose, fence them,
//! return `{"action_items": [...]}` instead of a bare array, emit `"assignee":
//! "N/A"`, or hand back a list of plain strings. All of those are handled; what
//! can't be parsed produces zero items and a log line, never a partial write.

use crate::database::repositories::action_item::{
    normalize_text_key, ActionItemsRepository, NewActionItem,
};
use crate::events::{EventSink, EventSinkExt};
use crate::summary::llm_client::{generate_summary, LlmConfig};
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

/// Ceiling on items stored from one extraction. A well-behaved model returns a
/// handful; a looping one can emit hundreds of near-duplicates, and writing
/// those would turn the task list into garbage the user has to clean up by hand.
const MAX_ITEMS: usize = 50;

/// Shortest text we'll accept. Filters fragments like "Done" or "Yes" that
/// aren't actionable but occasionally slip through as list entries.
const MIN_TEXT_LEN: usize = 4;

/// Values models emit to mean "no value here". Compared case-insensitively
/// after trimming; anything matching becomes NULL rather than a literal
/// assignee named "unassigned".
const NULL_SENTINELS: &[&str] = &[
    "none",
    "n/a",
    "na",
    "null",
    "unassigned",
    "unknown",
    "tbd",
    "tba",
    "not specified",
    "unspecified",
    "not mentioned",
    "everyone",
    "-",
];

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You extract action items from meeting summaries.

Return ONLY a JSON array. No prose, no explanation, no markdown fences.

Each element is an object with these keys:
  "text":     (required) the task, as a short imperative phrase. Do not include the owner's name or the deadline in this field.
  "assignee": (optional) the person responsible, exactly as named in the summary. Omit if the summary does not say who owns it.
  "due_hint": (optional) the deadline exactly as phrased in the summary ("by Friday", "before the next sprint"). Do not convert it to a date. Omit if no deadline is mentioned.

Rules:
- Only include commitments to do something in the future. Ignore decisions already made, background discussion, and things already completed.
- One element per task. Do not merge two tasks into one, and do not split one task into steps.
- Never invent an assignee or a deadline. If the summary does not state one, omit the key.
- If the summary contains no action items, return exactly: []

Example output:
[{"text":"Send the Q3 budget to Finance","assignee":"Seb","due_hint":"by Friday"},{"text":"Book the venue for the offsite"}]"#;

/// One item as it comes off the model, before validation.
///
/// Every field is `Option` + `#[serde(default)]` so a missing key and an
/// explicit `null` both land as `None` — models produce both, and a `null` for
/// a non-Option field would fail the whole array. `alias` covers the naming the
/// model reaches for when it ignores the prompt's exact keys.
#[derive(Debug, Deserialize)]
struct RawActionItem {
    #[serde(
        default,
        alias = "task",
        alias = "action",
        alias = "item",
        alias = "description"
    )]
    text: Option<String>,
    #[serde(default, alias = "owner", alias = "responsible", alias = "who")]
    assignee: Option<String>,
    #[serde(
        default,
        alias = "dueHint",
        alias = "due",
        alias = "due_date",
        alias = "dueDate",
        alias = "deadline",
        alias = "when"
    )]
    due_hint: Option<String>,
    /// The transcript sentence the item was drawn from. Only the
    /// transcript-sourced extractor asks for this; the summary path leaves it
    /// unset. Used to ground the item to a real timestamp (see
    /// `transcript_action_items`).
    #[serde(
        default,
        alias = "evidence",
        alias = "source",
        alias = "sentence",
        alias = "supporting_quote"
    )]
    quote: Option<String>,
}

/// An array element: either a full object or a bare string. Models asked for
/// objects sometimes return `["do x", "do y"]`, which is unambiguous enough to
/// accept rather than discard.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawElement {
    Text(String),
    Object(RawActionItem),
}

/// Pull a JSON array of action items out of whatever the model returned.
///
/// Returns `None` when no array could be located or parsed at all (caller logs
/// and stores nothing); returns an empty vec when the model legitimately found
/// no action items. That distinction matters: "the model said there are none"
/// must clear stale items, while "we couldn't understand the response" must not.
pub(crate) fn parse_action_items(raw: &str) -> Option<Vec<NewActionItem>> {
    let cleaned = strip_code_fences(raw.trim());

    // Whole response is valid JSON: either the array itself, or an object
    // wrapping it under some key ({"action_items": [...]}, {"items": [...]}).
    if let Ok(value) = serde_json::from_str::<Value>(&cleaned) {
        if let Some(items) = elements_from_value(&value) {
            return Some(normalize_elements(items));
        }
    }

    // Otherwise the array is embedded in prose ("Here are the items: [...]").
    let slice = find_json_array(&cleaned)?;
    let elements = serde_json::from_str::<Vec<RawElement>>(slice).ok()?;
    Some(normalize_elements(elements))
}

/// Accept a bare array, or an object with exactly one array-valued field
/// (whatever it's named). Anything else isn't recognizably a list of items.
fn elements_from_value(value: &Value) -> Option<Vec<RawElement>> {
    match value {
        Value::Array(_) => serde_json::from_value::<Vec<RawElement>>(value.clone()).ok(),
        Value::Object(map) => {
            let arrays: Vec<&Value> = map.values().filter(|v| v.is_array()).collect();
            if arrays.len() != 1 {
                return None;
            }
            serde_json::from_value::<Vec<RawElement>>(arrays[0].clone()).ok()
        }
        _ => None,
    }
}

/// Strip a leading ```json / ``` fence and its closing counterpart. Only touches
/// text that actually starts with a fence, so unfenced JSON passes through.
fn strip_code_fences(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let after_open = match trimmed.find('\n') {
        Some(idx) => &trimmed[idx + 1..],
        // Single-line fence: no body to speak of.
        None => return trimmed.trim_matches('`').trim().to_string(),
    };
    match after_open.rfind("```") {
        Some(idx) => after_open[..idx].trim().to_string(),
        None => after_open.trim().to_string(),
    }
}

/// Locate the first complete top-level `[...]` span, tracking string literals
/// and escapes so a bracket inside a task's text ("Review [draft] doc") doesn't
/// end the scan early.
fn find_json_array(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('[')?;

    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let c = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Validate, clean and de-duplicate parsed elements into storable items.
fn normalize_elements(elements: Vec<RawElement>) -> Vec<NewActionItem> {
    let mut out: Vec<NewActionItem> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for element in elements {
        let (text, assignee, due_hint, quote) = match element {
            RawElement::Text(t) => (t, None, None, None),
            RawElement::Object(o) => {
                (o.text.unwrap_or_default(), o.assignee, o.due_hint, o.quote)
            }
        };

        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() < MIN_TEXT_LEN {
            continue;
        }

        // Guard against the model echoing the prompt's own example back at us
        // instead of reading the summary.
        if text.eq_ignore_ascii_case("Send the Q3 budget to Finance")
            || text.eq_ignore_ascii_case("Book the venue for the offsite")
        {
            debug!("Dropping action item that echoes the prompt example: {text}");
            continue;
        }

        // Same notion of sameness the repository uses for carry-over — see
        // `normalize_text_key`. Deduping more loosely here would let two items
        // claim one previous row's id and abort the replace.
        let key = normalize_text_key(&text);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);

        out.push(NewActionItem {
            text,
            assignee: clean_assignee(assignee),
            due_hint: clean_optional(due_hint),
            source_quote: clean_optional(quote),
            source_start_secs: None,
            source_end_secs: None,
        });

        if out.len() >= MAX_ITEMS {
            warn!("Action-item extraction hit the {MAX_ITEMS}-item cap; ignoring the rest");
            break;
        }
    }

    out
}

/// Trim an optional field and map blanks / "no value" sentinels to `None`.
fn clean_optional(value: Option<String>) -> Option<String> {
    let trimmed = value?.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_lowercase();
    if NULL_SENTINELS.contains(&lowered.as_str()) {
        return None;
    }
    Some(trimmed)
}

/// Like [`clean_optional`], but also drops diarization labels ("Speaker 1")
/// that the transcript extractor's model copies from the speaker-tagged lines.
/// They name a voice cluster, not a person, so surfacing one as the owner is
/// worse than showing no owner at all.
fn clean_assignee(value: Option<String>) -> Option<String> {
    let cleaned = clean_optional(value)?;
    if is_generic_speaker_label(&cleaned) {
        return None;
    }
    Some(cleaned)
}

/// True for "Speaker", "Speaker 1", "speaker_2", "Speaker #3" — a generic voice
/// label. A real name that merely starts with "Speaker" (e.g. "Speakerman", or
/// "Speaker One and Two") has trailing non-digits and is kept.
fn is_generic_speaker_label(s: &str) -> bool {
    let lower = s.trim().to_lowercase();
    let Some(rest) = lower.strip_prefix("speaker") else {
        return false;
    };
    let rest = rest.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit())
}

/// Extract action items from `summary_markdown` and replace the meeting's
/// extractor-owned items with them.
///
/// Emits `action-items-extracted { meeting_id, count }` on success so an open
/// meeting-details page refreshes without polling. Returns the number of items
/// stored.
///
/// The transcript is deliberately *not* sent: the summary is a distilled version
/// of it, fits in one call for every provider including the small built-in
/// models, and mentions the commitments in the exact words the user just read.
/// Feeding the raw transcript would multiply the token cost and reintroduce the
/// chunking problem the summary already solved.
pub async fn extract_for_meeting(
    sink: &dyn EventSink,
    pool: &SqlitePool,
    meeting_id: &str,
    summary_markdown: &str,
    provider_name: &str,
    model_name: &str,
) -> Result<usize, String> {
    if summary_markdown.trim().is_empty() {
        return Err("summary is empty; nothing to extract".to_string());
    }

    let app_data_dir = crate::paths::app_data_dir().ok();
    let config = LlmConfig::resolve(pool, provider_name, model_name, app_data_dir).await?;

    let user_prompt = format!(
        "Extract the action items from this meeting summary.\n\n---\n{}\n---",
        summary_markdown.trim()
    );

    let response = generate_summary(&config, EXTRACTION_SYSTEM_PROMPT, &user_prompt, None, None)
        .await
        .map_err(|e| format!("LLM call failed: {e}"))?;

    let items = parse_action_items(&response).ok_or_else(|| {
        format!(
            "could not parse action items from the model response (first 200 chars: {:?})",
            response.chars().take(200).collect::<String>()
        )
    })?;

    let count = ActionItemsRepository::replace_summary_items(pool, meeting_id, &items)
        .await
        .map_err(|e| format!("failed to store action items: {e}"))?;

    info!("Extracted {count} action item(s) for meeting {meeting_id}");

    let _ = sink.emit_event(
        "action-items-extracted",
        &serde_json::json!({ "meeting_id": meeting_id, "count": count }),
    );

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_array() {
        let items = parse_action_items(
            r#"[{"text":"Send the budget","assignee":"Seb","due_hint":"by Friday"}]"#,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Send the budget");
        assert_eq!(items[0].assignee.as_deref(), Some("Seb"));
        assert_eq!(items[0].due_hint.as_deref(), Some("by Friday"));
    }

    #[test]
    fn parses_a_fenced_array() {
        let items = parse_action_items("```json\n[{\"text\":\"Book the room\"}]\n```").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Book the room");
    }

    #[test]
    fn parses_an_array_wrapped_in_prose() {
        let raw = "Sure! Here are the action items:\n[{\"text\":\"Ship the release\"}]\nLet me know if you need more.";
        let items = parse_action_items(raw).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Ship the release");
    }

    #[test]
    fn parses_an_object_wrapper() {
        let items =
            parse_action_items(r#"{"action_items":[{"text":"Review the PR"}]}"#).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Review the PR");
    }

    #[test]
    fn parses_plain_string_elements() {
        let items = parse_action_items(r#"["Send the notes", "Update the roadmap"]"#).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "Send the notes");
        assert!(items[0].assignee.is_none());
    }

    #[test]
    fn handles_null_and_missing_optional_fields() {
        let items = parse_action_items(
            r#"[{"text":"Do the thing","assignee":null},{"text":"Do another thing"}]"#,
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].assignee.is_none());
        assert!(items[1].due_hint.is_none());
    }

    #[test]
    fn maps_sentinel_values_to_none() {
        let items = parse_action_items(
            r#"[{"text":"Do the thing","assignee":"N/A","due_hint":"Not specified"}]"#,
        )
        .unwrap();
        assert!(items[0].assignee.is_none());
        assert!(items[0].due_hint.is_none());
    }

    #[test]
    fn accepts_alias_keys() {
        let items =
            parse_action_items(r#"[{"task":"Fix the bug","owner":"Ana","deadline":"today"}]"#)
                .unwrap();
        assert_eq!(items[0].text, "Fix the bug");
        assert_eq!(items[0].assignee.as_deref(), Some("Ana"));
        assert_eq!(items[0].due_hint.as_deref(), Some("today"));
    }

    #[test]
    fn empty_array_is_a_valid_zero_item_result() {
        let items = parse_action_items("[]").unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn unparseable_response_returns_none() {
        assert!(parse_action_items("I could not find any action items.").is_none());
        assert!(parse_action_items("").is_none());
    }

    #[test]
    fn malformed_json_array_returns_none() {
        assert!(parse_action_items(r#"[{"text": "unterminated"#).is_none());
    }

    #[test]
    fn brackets_inside_strings_do_not_end_the_scan() {
        let items = parse_action_items(r#"[{"text":"Review the [draft] doc"}]"#).unwrap();
        assert_eq!(items[0].text, "Review the [draft] doc");
    }

    #[test]
    fn nested_arrays_are_spanned_correctly() {
        let raw = r#"prefix [{"text":"Outer task","tags":["a","b"]}] suffix"#;
        let items = parse_action_items(raw).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Outer task");
    }

    #[test]
    fn drops_too_short_and_duplicate_items() {
        let items =
            parse_action_items(r#"["ok", "Send the notes", "send the notes", "Real task here"]"#)
                .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "Send the notes");
        assert_eq!(items[1].text, "Real task here");
    }

    /// Dedup must use the repository's key, not a plain lowercase compare:
    /// otherwise "Send the notes." and "Send the notes" both survive here, then
    /// both match the same previous row in `replace_summary_items` and collide
    /// on its primary key, aborting the whole extraction.
    #[test]
    fn dedupes_using_the_repository_identity_key() {
        let items =
            parse_action_items(r#"["Send the notes.", "send the  notes", "Real task here"]"#)
                .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "Send the notes.");
        assert_eq!(items[1].text, "Real task here");
    }

    #[test]
    fn strips_generic_speaker_assignees_but_keeps_real_names() {
        let items = parse_action_items(
            r#"[
              {"text":"Introduce the new sellers","assignee":"Speaker 1"},
              {"text":"Update the account list","assignee":"speaker_2"},
              {"text":"Fix the admin issues","assignee":"Amanda"}
            ]"#,
        )
        .unwrap();
        assert_eq!(items.len(), 3);
        assert!(items[0].assignee.is_none(), "Speaker 1 should be dropped");
        assert!(items[1].assignee.is_none(), "speaker_2 should be dropped");
        assert_eq!(items[2].assignee.as_deref(), Some("Amanda"));
    }

    #[test]
    fn keeps_a_real_name_that_starts_with_speaker() {
        assert!(!is_generic_speaker_label("Speakerman"));
        assert!(is_generic_speaker_label("Speaker"));
        assert!(is_generic_speaker_label("Speaker 3"));
        assert!(is_generic_speaker_label("speaker_10"));
    }

    #[test]
    fn drops_echoed_prompt_examples() {
        let items = parse_action_items(
            r#"[{"text":"Send the Q3 budget to Finance","assignee":"Seb"},{"text":"A genuine task"}]"#,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "A genuine task");
    }

    #[test]
    fn caps_runaway_output() {
        let many: Vec<String> = (0..200).map(|i| format!(r#""Task number {i} here""#)).collect();
        let raw = format!("[{}]", many.join(","));
        let items = parse_action_items(&raw).unwrap();
        assert_eq!(items.len(), MAX_ITEMS);
    }

    #[test]
    fn normalizes_whitespace_in_text() {
        let items = parse_action_items("[{\"text\":\"  Send   the\\n budget  \"}]").unwrap();
        assert_eq!(items[0].text, "Send the budget");
    }
}
