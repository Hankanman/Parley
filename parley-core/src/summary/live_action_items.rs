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

use crate::database::repositories::action_item::normalize_text_key;
use crate::events::{EventSinkExt, SharedEventSink};
use crate::summary::llm_client::LlmConfig;
use crate::summary::transcript_action_items::{extract_window, Segment};

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

        while GENERATION.load(Ordering::SeqCst) == generation {
            interval.tick().await;

            let segments: Vec<Segment> =
                crate::audio::recording_service::snapshot_segments()
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
                    let key = normalize_text_key(&item.text);
                    if seen.contains(&key) {
                        return None;
                    }
                    seen.push(key);
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

/// Stop the live extractor (supersedes any running loop).
pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}
