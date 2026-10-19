//! Live (in-meeting) action-item extraction.
//!
//! During a recording there is no `meeting_id` and no DB rows yet (transcripts
//! are persisted only on stop), so this works entirely from the in-memory
//! segment buffer and emits *provisional* items to the UI — it never writes to
//! the database. It's the "streaming preview" half of the pattern the transcript
//! partials already use: on stop the authoritative
//! [`super::transcript_action_items`] pass produces the committed, grounded,
//! deduped rows and the live preview is discarded.
//!
//! Because it runs the summary model on the GPU while Whisper is transcribing,
//! it's opt-in (a Beta feature) and paced conservatively.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::database::repositories::action_item::{normalize_text_key, NewActionItem};
use crate::events::{EventSinkExt, SharedEventSink};
use crate::summary::llm_client::LlmConfig;
use crate::summary::transcript_action_items::{
    extract_window, names_task, states_commitment, Segment,
};

/// How often to run a live extraction pass. A compromise between responsiveness
/// and not stealing the GPU from transcription too often.
const INTERVAL: Duration = Duration::from_secs(75);
/// Seconds of already-seen audio to re-include each pass, so an item spoken
/// right at the previous boundary isn't missed.
const OVERLAP_SECS: f64 = 20.0;
/// Minimum new audio (past the watermark) before a pass is worth running.
const MIN_NEW_SECS: f64 = 30.0;

/// Event carrying newly-found provisional items to the recording UI.
pub const LIVE_EVENT: &str = "live-action-items";

/// Bumped on every start/stop; a running loop exits when its generation is
/// superseded (same approach as the enrollment/level-monitor tasks).
static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Clone)]
struct LiveItem {
    text: String,
    assignee: Option<String>,
    due_hint: Option<String>,
    start_secs: Option<f64>,
    end_secs: Option<f64>,
}

/// Start the live extractor for the current recording. Safe to call twice — the
/// previous loop is superseded. Resolves the model from settings; on failure it
/// logs and simply doesn't run (the feature is best-effort).
pub fn start(sink: SharedEventSink, pool: SqlitePool, provider_name: String, model_name: String) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    tokio::spawn(async move {
        let app_data_dir = crate::paths::app_data_dir().ok();
        let config =
            match LlmConfig::resolve(&pool, &provider_name, &model_name, app_data_dir).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Live action items: cannot resolve model ({e}); not running");
                    return;
                }
            };

        info!("Live action-item extraction started (generation {generation})");
        let mut interval = tokio::time::interval(INTERVAL);
        // Skip the immediate first tick — wait a full interval so there's
        // something to extract from.
        interval.tick().await;

        let mut watermark = 0.0f64;
        let mut seen: Vec<String> = Vec::new();
        let mut seen_lines: Vec<u64> = Vec::new();

        while GENERATION.load(Ordering::SeqCst) == generation {
            interval.tick().await;

            let segments: Vec<Segment> = crate::audio::recording_service::snapshot_segments()
                .iter()
                .map(Segment::from_common)
                .filter(|s| !s.is_empty())
                .collect();
            if segments.is_empty() {
                continue;
            }

            let latest = segments
                .iter()
                .filter_map(|s| s.end_secs())
                .fold(0.0f64, f64::max);
            if latest - watermark < MIN_NEW_SECS {
                continue; // not enough new speech yet
            }

            // The recent slice: everything past the watermark, with look-back.
            // Filtered from the snapshot already taken above — a second
            // snapshot could disagree with the `latest` just computed.
            let cutoff = (watermark - OVERLAP_SECS).max(0.0);
            let window: Vec<Segment> = segments
                .into_iter()
                .filter(|s| s.start_secs().unwrap_or(0.0) >= cutoff)
                .collect();
            if window.is_empty() {
                watermark = latest;
                continue;
            }

            let items = extract_window(&config, &window).await;
            watermark = latest;

            // Superseded while we were awaiting the model? Drop the result.
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }

            let fresh: Vec<LiveItem> = items
                .into_iter()
                .filter_map(|item| {
                    if !is_live_worthy(&item) {
                        return None;
                    }
                    // The look-back means the same sentence is offered again on
                    // the next pass, usually reworded — so one item per line.
                    let anchor = item.source_start_secs.map(f64::to_bits);
                    let key = normalize_text_key(&item.text);
                    if seen.contains(&key) || anchor.is_some_and(|a| seen_lines.contains(&a)) {
                        return None;
                    }
                    seen.push(key);
                    seen_lines.extend(anchor);
                    Some(LiveItem {
                        text: item.text,
                        assignee: item.assignee,
                        due_hint: item.due_hint,
                        start_secs: item.source_start_secs,
                        end_secs: item.source_end_secs,
                    })
                })
                .collect();

            if !fresh.is_empty() {
                let _ = sink.emit_event(LIVE_EVENT, &serde_json::json!({ "items": fresh }));
                info!("Live action items: emitted {} new item(s)", fresh.len());
            }
        }
        info!("Live action-item extraction stopped (generation {generation})");
    });
}

/// Longest task text the live preview shows. A rewritten task is a short
/// imperative; anything longer is a stretch of transcript pasted back.
const MAX_LIVE_WORDS: usize = 25;
/// Shortest task text the live preview shows ("Call Dave back").
const MIN_LIVE_WORDS: usize = 3;

/// Opening words of a transcript fragment rather than a task. The prompt asks
/// for an imperative that starts with a verb, so text opening like this is the
/// model passing a line of chat through.
const FRAGMENT_OPENERS: &[&str] = &[
    "yeah", "yes", "yep", "no", "okay", "ok", "oh", "um", "uh", "so", "and", "but", "or", "if",
    "well", "right", "because", "cause", "it", "it's", "that", "that's", "this", "there",
    "there's",
];

/// Whether an item is solid enough to show mid-meeting. The preview is held to
/// a higher bar than the post-meeting pass, because a small model flags every
/// other sentence whatever the prompt says:
/// - the cited sentence must be found in the transcript, not just partly matched;
/// - the task must be one short sentence that doesn't open like chat ("Yeah,
///   ...", "If ..."), not a pasted run of transcript;
/// - the cited sentence or the task must take on, hand out or set a deadline
///   for something (the model's quote is often a stray "Yeah." beside a real
///   task, so either side can carry it).
fn is_live_worthy(item: &NewActionItem) -> bool {
    let text = item.text.trim();
    let one_sentence = !text
        .trim_end_matches(['.', '!', '?'])
        .contains(['.', '!', '?']);
    let words: Vec<&str> = text.split_whitespace().collect();
    let opener = words
        .first()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                .to_lowercase()
                .replace('\u{2019}', "'")
        })
        .unwrap_or_default();
    item.source_start_secs.is_some()
        && one_sentence
        && (MIN_LIVE_WORDS..=MAX_LIVE_WORDS).contains(&words.len())
        && !FRAGMENT_OPENERS.contains(&opener.as_str())
        && !text.ends_with('?')
        && (names_task(text) || item.source_quote.as_deref().is_some_and(states_commitment))
}

/// Stop the live extractor (supersedes any running loop).
pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, quote: &str) -> NewActionItem {
        NewActionItem {
            text: text.to_string(),
            source_quote: Some(quote.to_string()),
            source_start_secs: Some(12.0),
            ..Default::default()
        }
    }

    #[test]
    fn keeps_a_short_task_with_a_commitment_on_either_side() {
        assert!(is_live_worthy(&item(
            "Book the call with IBM",
            "I'll book the call with IBM."
        )));
        // Real gemma3:1b output: right task, stray quote.
        assert!(is_live_worthy(&item(
            "Chase an outcome on that and try and get a call together by Wednesday afternoon.",
            "out of time.",
        )));
    }

    #[test]
    fn drops_chatter_pasted_transcript_and_unfound_quotes() {
        assert!(!is_live_worthy(&item(
            "We would still expect to get paid on that as a gainshare.",
            "Yeah."
        )));
        assert!(!is_live_worthy(&item(
            "So that's my update. Any questions?",
            "Any questions?"
        )));
        assert!(!is_live_worthy(&item("We'll see.", "Yes.")));
        assert!(!is_live_worthy(&item("Okay.", "I'll call you.")));
        assert!(!is_live_worthy(&item(
            "Yeah, we need to start now.",
            "Yeah, that's 100,000."
        )));
        assert!(!is_live_worthy(&item(
            "If you pardon the expression, I would run it up your flagpole.",
            "and I'll get back to the yes, no or maybe by",
        )));
        assert!(!is_live_worthy(&item(
            "Straightforward. Oh, I forgot to record this. Hang on, let me record and then I'll tell Craig that.",
            "Uh, hang on, let me record and then I'll tell Craig that.",
        )));
        let mut unfound = item(
            "Send the Q3 budget to Finance by Friday.",
            "I'll send the Q3 budget over to Finance by Friday.",
        );
        unfound.source_start_secs = None;
        assert!(!is_live_worthy(&unfound));
    }
}
