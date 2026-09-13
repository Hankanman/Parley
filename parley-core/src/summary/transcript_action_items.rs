//! Transcript-sourced action-item extraction.
//!
//! Where `action_extraction` re-reads the finished *summary*, this reads the
//! timestamped transcript segments directly. Each item the model returns must
//! cite the sentence it came from (`quote`); we match that quote back to a real
//! segment and stamp the item with that segment's recording-relative time — so
//! the UI can replay the exact moment it was said — and we drop items whose
//! quote isn't anywhere in the transcript (a hallucination guard the
//! summary-based path can't offer).
//!
//! The transcript is windowed so long meetings stay within the model's context
//! (mirroring the summary's map step), with a few segments of overlap so an
//! item stated across a boundary is still seen whole by one window.

use crate::database::models::Transcript;
use crate::database::repositories::action_item::{
    normalize_text_key, ActionItemsRepository, NewActionItem,
};
use crate::events::{EventSink, EventSinkExt, SharedEventSink};
use crate::summary::action_extraction::parse_action_items;
use crate::summary::llm_client::{generate_summary, LlmConfig};
use sqlx::SqlitePool;
use tracing::{info, warn};

/// Character budget per window (~2k tokens of transcript), leaving room for the
/// prompt and JSON output inside a small local model's context.
const WINDOW_CHARS: usize = 8000;
/// Segments carried from the end of one window into the start of the next.
const WINDOW_OVERLAP_SEGMENTS: usize = 3;
/// Fraction of a quote's words that must appear in a segment for the item to be
/// anchored to that segment's timestamp.
const QUOTE_GROUND_MIN: f32 = 0.5;
/// Below this, a quote is considered absent from the transcript and the item is
/// dropped as a likely hallucination.
const QUOTE_HALLUCINATION_MAX: f32 = 0.2;
/// Final cap on stored items (matches the summary extractor's ceiling).
const MAX_ITEMS: usize = 50;

/// A transcript segment reduced to what extraction needs.
pub(crate) struct Segment {
    text: String,
    start: Option<f64>,
    end: Option<f64>,
    speaker: String,
}

impl Segment {
    /// Build from a live-recording in-memory segment (used by the live driver).
    pub(crate) fn from_common(t: &crate::audio::common::TranscriptSegment) -> Self {
        Segment {
            text: t.text.trim().to_string(),
            start: t.audio_start_time,
            end: t.audio_end_time,
            speaker: t
                .speaker
                .clone()
                .unwrap_or_else(|| "Speaker".to_string()),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn end_secs(&self) -> Option<f64> {
        self.end
    }

    pub(crate) fn start_secs(&self) -> Option<f64> {
        self.start
    }
}

/// Extract action items from `meeting_id`'s stored transcript segments and
/// replace the meeting's extractor-owned items. Emits `action-items-extracted`
/// and returns the number stored. Errors if the meeting has no transcript.
pub async fn extract_from_transcript(
    sink: &dyn EventSink,
    pool: &SqlitePool,
    meeting_id: &str,
    provider_name: &str,
    model_name: &str,
) -> Result<usize, String> {
    let rows = sqlx::query_as::<_, Transcript>(
        "SELECT * FROM transcripts WHERE meeting_id = ? \
         ORDER BY COALESCE(audio_start_time, 0.0), sequence_id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("failed to load transcript: {e}"))?;

    let segments: Vec<Segment> = rows
        .into_iter()
        .filter(|t| !t.transcript.trim().is_empty())
        .map(|t| Segment {
            text: t.transcript.trim().to_string(),
            start: t.audio_start_time,
            end: t.audio_end_time,
            speaker: t.speaker.unwrap_or_else(|| "Speaker".to_string()),
        })
        .collect();

    if segments.is_empty() {
        return Err("no transcript segments to extract from".to_string());
    }

    let app_data_dir = crate::paths::app_data_dir().ok();
    let config = LlmConfig::resolve(pool, provider_name, model_name, app_data_dir).await?;

    let windows = window_segments(&segments);
    info!(
        "Transcript extraction for {meeting_id}: {} segment(s) in {} window(s)",
        segments.len(),
        windows.len()
    );

    let mut collected: Vec<NewActionItem> = Vec::new();
    for window in &windows {
        collected.extend(extract_window(&config, window).await);
    }

    let deduped = dedupe(collected);
    let count = ActionItemsRepository::replace_summary_items(pool, meeting_id, &deduped)
        .await
        .map_err(|e| format!("failed to store action items: {e}"))?;

    info!("Transcript extraction stored {count} action item(s) for {meeting_id}");
    let _ = sink.emit_event(
        "action-items-extracted",
        &serde_json::json!({ "meeting_id": meeting_id, "count": count }),
    );
    Ok(count)
}

/// Fire-and-forget wrapper for the summary-completion path (see `service.rs`).
pub fn spawn_transcript_extraction(
    sink: SharedEventSink,
    pool: SqlitePool,
    meeting_id: String,
    provider_name: String,
    model_name: String,
) {
    tokio::spawn(async move {
        match extract_from_transcript(&sink, &pool, &meeting_id, &provider_name, &model_name).await {
            Ok(count) => info!("Transcript action-item extraction finished for {meeting_id}: {count}"),
            Err(e) => warn!("Transcript action-item extraction failed for {meeting_id}: {e}"),
        }
    });
}

/// Run one extraction pass over a single window: render the segments, call the
/// model, parse, and ground each item's quote to a segment's timestamp.
/// Returns grounded items — the caller dedupes across windows. Shared by the
/// post-meeting pass and the live driver.
pub(crate) async fn extract_window(config: &LlmConfig, window: &[Segment]) -> Vec<NewActionItem> {
    let user_prompt = format!("Transcript:\n\n{}", render_window(window));
    let response = match generate_summary(config, SYSTEM_PROMPT, &user_prompt, None, None).await {
        Ok(r) => r,
        Err(e) => {
            warn!("Action-item extraction window failed: {e}");
            return Vec::new();
        }
    };

    let Some(items) = parse_action_items(&response) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for mut item in items {
        // Drop items where the model echoed the transcript sentence verbatim as
        // the task instead of rewriting it into an imperative — a raw quote as a
        // "task" is noise ("let's talk about the list", "I don't know what more
        // I can do"). Only exact matches are dropped: a genuine rewrite reorders
        // words, so it never equals its quote, while a verbatim slice ("Amanda
        // updates this list") is a legitimate extraction and is kept.
        if let Some(quote) = item.source_quote.as_deref() {
            if is_echoed_quote(&item.text, quote) {
                debug_drop_echo(&item);
                continue;
            }
        }
        match ground(&item, window) {
            Grounding::Anchored { start, end } => {
                item.source_start_secs = start;
                item.source_end_secs = end;
                out.push(item);
            }
            Grounding::Kept => out.push(item),
            Grounding::Hallucinated => debug_drop(&item),
        }
    }
    out
}

/// True when `text` is the transcript sentence echoed back verbatim rather than
/// rewritten into a task. Compared after normalization (lowercased, punctuation
/// and spacing collapsed) so trivial formatting differences don't hide it.
fn is_echoed_quote(text: &str, quote: &str) -> bool {
    let normalized = normalize_phrase(text);
    !normalized.is_empty() && normalized == normalize_phrase(quote)
}

/// Lowercase, collapse every run of non-alphanumeric characters to a single
/// space, and trim — a punctuation-insensitive form for comparing two phrases.
fn normalize_phrase(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

const SYSTEM_PROMPT: &str = r#"You extract concrete action items from a raw meeting transcript.

An action item is a specific task someone committed to doing after the meeting — something with a clear, doable outcome. Be strict: in casual discussion most sentences are NOT action items.

Return ONLY a JSON array. No prose, no explanation, no markdown fences.

Each element is an object with these keys:
  "text":     (required) the task, rewritten as a short concrete imperative that starts with a verb (e.g. "Send the Q3 budget to Finance"). Rephrase it in your own words — NEVER copy a transcript sentence verbatim. Do not put the owner's name or the deadline in this field.
  "assignee": (optional) the real name of the person who will do it, if the transcript clearly says who. Never use a role, a team, or a diarization label like "Speaker 1". Omit if it is not clearly stated.
  "due_hint": (optional) the deadline exactly as spoken ("by Friday", "next sprint"). Do not convert it to a date. Omit if none.
  "quote":    (required) copy, verbatim, the single transcript sentence the task comes from.

INCLUDE only clear commitments to do a specific task in the future.

EXCLUDE (never output these):
- Opinions, feelings, and observations ("I don't know what more I can do", "I think this is hard").
- Topics or agenda pointers ("let's talk about the Power 25 list", "let's discuss pricing").
- Questions, hypotheticals, and vague aspirations ("we should grow the pipeline").
- Decisions already made and work already done.
- Anything with no concrete task or that just restates a sentence.

Prefer precision over completeness: a short list of real tasks is far more useful than a long list padded with chatter. If the transcript has no clear tasks, return exactly: [].

The "quote" MUST be copied word-for-word from the transcript lines provided; never invent it. Never invent an assignee or a deadline.

Example transcript:
[00:10] Speaker 1: Honestly, I don't know what more I can do to help here.
[00:20] Speaker 2: Okay. I'll send the Q3 budget over to Finance by Friday.
[00:31] Speaker 1: Sounds good. Let's talk about the offsite next time.

Example output:
[{"text":"Send the Q3 budget to Finance","due_hint":"by Friday","quote":"I'll send the Q3 budget over to Finance by Friday."}]"#;

/// Group segments into overlapping, character-bounded windows.
fn window_segments(segments: &[Segment]) -> Vec<&[Segment]> {
    let mut windows = Vec::new();
    let mut start = 0usize;
    while start < segments.len() {
        let mut chars = 0usize;
        let mut end = start;
        while end < segments.len() {
            chars += segments[end].text.len() + 16; // + line prefix overhead
            end += 1;
            if chars >= WINDOW_CHARS {
                break;
            }
        }
        windows.push(&segments[start..end]);
        if end >= segments.len() {
            break;
        }
        // Step forward, retaining a few segments of overlap.
        start = end.saturating_sub(WINDOW_OVERLAP_SEGMENTS).max(start + 1);
    }
    windows
}

/// Render a window as timestamped, speaker-tagged lines the model can quote from.
fn render_window(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| format!("[{}] {}: {}", fmt_ts(s.start), s.speaker, s.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn fmt_ts(secs: Option<f64>) -> String {
    let total = secs.unwrap_or(0.0).max(0.0) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

enum Grounding {
    Anchored { start: Option<f64>, end: Option<f64> },
    Kept,
    Hallucinated,
}

/// Match an item's quote to the best segment in its window.
fn ground(item: &NewActionItem, window: &[Segment]) -> Grounding {
    let Some(quote) = item.source_quote.as_deref() else {
        // Model gave no quote despite the instruction — keep the item but leave
        // it unanchored rather than punishing a formatting slip.
        return Grounding::Kept;
    };
    let quote_words = normalize_words(quote);
    if quote_words.is_empty() {
        return Grounding::Kept;
    }

    let mut best = 0.0f32;
    let mut best_seg: Option<&Segment> = None;
    for seg in window {
        let score = coverage(&quote_words, &seg.text);
        if score > best {
            best = score;
            best_seg = Some(seg);
        }
    }

    if best >= QUOTE_GROUND_MIN {
        let seg = best_seg.expect("best set with score");
        Grounding::Anchored {
            start: seg.start,
            end: seg.end,
        }
    } else if best <= QUOTE_HALLUCINATION_MAX {
        Grounding::Hallucinated
    } else {
        Grounding::Kept
    }
}

/// Fraction of `quote_words` that appear in `segment`.
fn coverage(quote_words: &[String], segment: &str) -> f32 {
    let seg: std::collections::HashSet<String> = normalize_words(segment).into_iter().collect();
    if quote_words.is_empty() {
        return 0.0;
    }
    let hits = quote_words.iter().filter(|w| seg.contains(*w)).count();
    hits as f32 / quote_words.len() as f32
}

/// Lowercase, drop punctuation, split into words.
fn normalize_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// De-duplicate across windows by the repository's identity key, keeping the
/// first (earliest) occurrence — which is also the one anchored to the earliest
/// timestamp, since windows are processed in order.
fn dedupe(items: Vec<NewActionItem>) -> Vec<NewActionItem> {
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in items {
        let key = normalize_text_key(&item.text);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(item);
        if out.len() >= MAX_ITEMS {
            warn!("Transcript extraction hit the {MAX_ITEMS}-item cap; ignoring the rest");
            break;
        }
    }
    out
}

fn debug_drop(item: &NewActionItem) {
    tracing::debug!(
        "Dropping ungrounded action item (quote not in transcript): {}",
        item.text
    );
}

fn debug_drop_echo(item: &NewActionItem) {
    tracing::debug!(
        "Dropping echoed action item (text is the verbatim quote): {}",
        item.text
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, start: f64) -> Segment {
        Segment {
            text: text.to_string(),
            start: Some(start),
            end: Some(start + 3.0),
            speaker: "Seb".to_string(),
        }
    }

    #[test]
    fn grounds_a_verbatim_quote_to_its_segment() {
        let window = vec![
            seg("Let's talk about the budget for next quarter.", 10.0),
            seg("You need to send the Q3 budget to finance by Friday.", 20.0),
        ];
        let item = NewActionItem {
            text: "Send the Q3 budget to finance".to_string(),
            source_quote: Some("send the Q3 budget to finance by Friday".to_string()),
            ..Default::default()
        };
        match ground(&item, &window) {
            Grounding::Anchored { start, .. } => assert_eq!(start, Some(20.0)),
            _ => panic!("expected anchored to the 20s segment"),
        }
    }

    #[test]
    fn drops_a_quote_absent_from_the_window() {
        let window = vec![seg("We discussed the weather and had coffee.", 5.0)];
        let item = NewActionItem {
            text: "Deploy the new billing service".to_string(),
            source_quote: Some("deploy the new billing service to production tonight".to_string()),
            ..Default::default()
        };
        assert!(matches!(ground(&item, &window), Grounding::Hallucinated));
    }

    #[test]
    fn keeps_an_item_with_no_quote_unanchored() {
        let window = vec![seg("Anything at all here.", 1.0)];
        let item = NewActionItem {
            text: "Do the thing".to_string(),
            ..Default::default()
        };
        assert!(matches!(ground(&item, &window), Grounding::Kept));
    }

    #[test]
    fn windows_cover_all_segments_with_overlap() {
        let long = "word ".repeat(400); // ~2000 chars each
        let segments: Vec<Segment> = (0..10).map(|i| seg(&long, i as f64)).collect();
        let windows = window_segments(&segments);
        assert!(windows.len() > 1, "should split into multiple windows");
        // Every segment index appears in at least one window.
        let covered: std::collections::HashSet<usize> = windows
            .iter()
            .flat_map(|w| w.iter().map(|s| s.start.unwrap() as usize))
            .collect();
        assert_eq!(covered.len(), 10);
    }

    #[test]
    fn detects_a_verbatim_echo_ignoring_punctuation_and_case() {
        // The model copied the sentence into `text` instead of rewriting it.
        assert!(is_echoed_quote(
            "let's talk about Power 25 lists",
            "Let's talk about Power 25 lists.",
        ));
        assert!(is_echoed_quote(
            "I don't know what more I can do to help",
            "I don't know what more I can do to help",
        ));
    }

    #[test]
    fn keeps_a_genuine_rewrite_and_a_verbatim_slice() {
        // A real rewrite reorders words — never equals its (different) quote.
        assert!(!is_echoed_quote(
            "Broaden the opportunity for services with Tesco.",
            "So what's happening? Have we managed to broaden Tesco?",
        ));
        // A clean verbatim slice of a longer quote is a legitimate extraction.
        assert!(!is_echoed_quote(
            "Amanda updates this list",
            "I'm going to ask that only Amanda updates this list.",
        ));
    }

    #[test]
    fn dedupe_keeps_first_occurrence() {
        let items = vec![
            NewActionItem {
                text: "Send the notes".to_string(),
                source_start_secs: Some(5.0),
                ..Default::default()
            },
            NewActionItem {
                text: "send the notes.".to_string(),
                source_start_secs: Some(9.0),
                ..Default::default()
            },
        ];
        let out = dedupe(items);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source_start_secs, Some(5.0));
    }
}
