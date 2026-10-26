use crate::database::repositories::{
    action_item::ActionItemsRepository, meeting::MeetingsRepository,
    meeting_note::MeetingNotesRepository, summary::SummaryProcessesRepository,
};
use crate::events::{EventSinkExt, SharedEventSink};
use crate::summary::llm_client::LlmConfig;
use crate::summary::processor::{
    extract_meeting_name_from_markdown, generate_meeting_summary_with_language,
    strip_first_heading_line, SummaryStream,
};
use crate::summary::structured::StructuredSummary;
use once_cell::sync::Lazy;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Cancellation tokens for in-flight generations, keyed by meeting id. Each
/// entry also carries the registration id it was created with, so a stale
/// run's cleanup can never remove the token belonging to a newer run on the
/// same meeting (start → start again → first cleanup used to strand the
/// second run uncancellable).
static CANCELLATION_REGISTRY: Lazy<Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

static REGISTRATION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Forwards the final summary's live preview to the UI as `summary-stream`
/// events. A `reset: true` event tells the UI to discard what it has
/// streamed so far (a retry or the Markdown fallback is about to re-stream).
struct EventSummaryStream {
    sink: SharedEventSink,
    meeting_id: String,
}

impl SummaryStream for EventSummaryStream {
    fn delta(&self, text: &str) {
        let _ = self.sink.emit_event(
            "summary-stream",
            &serde_json::json!({
                "meeting_id": self.meeting_id,
                "delta": text,
            }),
        );
    }

    fn reset(&self) {
        let _ = self.sink.emit_event(
            "summary-stream",
            &serde_json::json!({
                "meeting_id": self.meeting_id,
                "delta": "",
                "reset": true,
            }),
        );
    }
}

/// Summary service - handles all summary generation logic
pub struct SummaryService;

impl SummaryService {
    /// Registers a new cancellation token for a meeting. Returns the token
    /// and the registration id to pass back to [`Self::cleanup_cancellation_token`].
    fn register_cancellation_token(meeting_id: &str) -> (u64, CancellationToken) {
        let token = CancellationToken::new();
        let registration_id = REGISTRATION_COUNTER.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut registry) = CANCELLATION_REGISTRY.lock() {
            registry.insert(meeting_id.to_string(), (registration_id, token.clone()));
            info!("Registered cancellation token for meeting: {}", meeting_id);
        }
        (registration_id, token)
    }

    /// Cancels the summary generation for a meeting
    pub fn cancel_summary(meeting_id: &str) -> bool {
        if let Ok(registry) = CANCELLATION_REGISTRY.lock() {
            if let Some((_, token)) = registry.get(meeting_id) {
                info!("Cancelling summary generation for meeting: {}", meeting_id);
                token.cancel();
                return true;
            }
        }
        warn!(
            "No active summary generation found for meeting: {}",
            meeting_id
        );
        false
    }

    /// Cleans up the cancellation token after processing completes — but only
    /// if the registered token still belongs to this run (see registry docs).
    fn cleanup_cancellation_token(meeting_id: &str, registration_id: u64) {
        if let Ok(mut registry) = CANCELLATION_REGISTRY.lock() {
            if registry
                .get(meeting_id)
                .map_or(false, |(id, _)| *id == registration_id)
            {
                registry.remove(meeting_id);
                info!("Cleaned up cancellation token for meeting: {}", meeting_id);
            }
        }
    }

    /// Processes transcript in the background and generates summary
    ///
    /// This function is designed to be spawned as an async task and does not block
    /// the main thread. It updates the database with progress and results.
    ///
    /// # Arguments
    /// * `sink` - Event sink events are emitted through
    /// * `pool` - SQLx connection pool
    /// * `meeting_id` - Unique identifier for the meeting
    /// * `text` - Full transcript text
    /// * `model_provider` - LLM provider name (e.g., "ollama", "openai")
    /// * `model_name` - Specific model (e.g., "gpt-4", "llama3.2:latest")
    /// * `custom_prompt` - Optional user-provided context
    /// * `template_id` - Template identifier (e.g., "daily_standup", "standard_meeting")
    pub async fn process_transcript_background(
        sink: SharedEventSink,
        pool: SqlitePool,
        meeting_id: String,
        text: String,
        model_provider: String,
        model_name: String,
        custom_prompt: String,
        template_id: String,
    ) {
        let start_time = Instant::now();
        info!(
            "Starting background processing for meeting_id: {}",
            meeting_id
        );

        // Register cancellation token for this meeting
        let (registration_id, cancellation_token) = Self::register_cancellation_token(&meeting_id);

        // Register the run in `summary_processes` first: the completed /
        // failed / cancelled updates below only UPDATE an existing row, so
        // without this a summary for a meeting with no prior row is lost.
        if let Err(e) =
            SummaryProcessesRepository::create_or_reset_process(&pool, &meeting_id).await
        {
            error!(
                "Failed to register summary process for {}: {}",
                meeting_id, e
            );
        }

        // Resolve provider, credentials and endpoints from settings — the
        // same resolution every other LLM caller uses.
        let app_data_dir = crate::paths::app_data_dir().ok();
        let config =
            match LlmConfig::resolve(&pool, &model_provider, &model_name, app_data_dir).await {
                Ok(config) => config,
                Err(e) => {
                    Self::update_process_failed(&pool, &meeting_id, &e).await;
                    Self::cleanup_cancellation_token(&meeting_id, registration_id);
                    return;
                }
            };

        let token_threshold = config.token_threshold().await;

        // Forward the final pass's streamed output to the UI so the summary
        // renders progressively instead of appearing all at once. Providers
        // that don't stream (everything except built-in AI today) simply
        // never invoke the sink and the UI keeps its spinner.
        let stream = EventSummaryStream {
            sink: sink.clone(),
            meeting_id: meeting_id.clone(),
        };

        // The user's own notes ride along as priorities (#82). A failure to
        // read them shouldn't cost the user their summary.
        let notes: Vec<String> =
            match MeetingNotesRepository::list_by_meeting(&pool, &meeting_id).await {
                Ok(notes) => notes.into_iter().map(|n| n.body).collect(),
                Err(e) => {
                    warn!("Failed to load notes for {}: {}", meeting_id, e);
                    Vec::new()
                }
            };

        // Bookmarked moments (issue #87) ride along as emphasis context.
        let custom_prompt =
            crate::bookmarks::with_bookmark_context(&pool, &meeting_id, custom_prompt).await;

        let language = match super::language::load(&pool).await {
            Ok(language) => language,
            Err(error) => {
                Self::update_process_failed(&pool, &meeting_id, &error).await;
                Self::cleanup_cancellation_token(&meeting_id, registration_id);
                return;
            }
        };
        let result = generate_meeting_summary_with_language(
            &config,
            &text,
            &custom_prompt,
            &template_id,
            &notes,
            token_threshold,
            Some(&cancellation_token),
            Some(&stream),
            &language,
        )
        .await;

        let duration = start_time.elapsed().as_secs_f64();

        // Clean up cancellation token regardless of outcome
        Self::cleanup_cancellation_token(&meeting_id, registration_id);

        match result {
            Ok(output) => {
                let mut final_markdown = output.markdown;
                if output.chunks_processed == 0 && final_markdown.is_empty() {
                    Self::update_process_failed(
                        &pool,
                        &meeting_id,
                        "Summary generation failed: No content was processed.",
                    )
                    .await;
                    return;
                }

                info!(
                    "✓ Successfully processed {} chunks for meeting_id: {} ({} chars of markdown). Duration: {:.2}s",
                    output.chunks_processed,
                    meeting_id,
                    final_markdown.len(),
                    duration
                );

                // Adopt the summary's title as the meeting name, but only
                // over a generated placeholder ("Meeting 08_10_26_…") — a
                // title the user typed or a calendar event supplied stays.
                // Structured output carries it as an explicit (already
                // sanitized) field; free-form Markdown in its first heading.
                let title = match &output.structured {
                    Some(structured) => Some(structured.title.clone()),
                    None => extract_meeting_name_from_markdown(&final_markdown),
                };
                if let Some(name) = title {
                    if !name.is_empty() {
                        match MeetingsRepository::update_placeholder_title(
                            &pool,
                            &meeting_id,
                            &name,
                        )
                        .await
                        {
                            Ok(true) => {
                                info!("Named meeting {} '{}' from its summary", meeting_id, name)
                            }
                            Ok(false) => {}
                            Err(e) => {
                                error!("Failed to update meeting name for {}: {}", meeting_id, e)
                            }
                        }

                        // The title now lives on the meeting row; drop the
                        // heading line so it isn't duplicated in the body.
                        final_markdown = strip_first_heading_line(&final_markdown);
                    }
                }

                // Create result JSON with markdown only (summary_json will be
                // added on first edit). `failed_chunks` flags a summary that's
                // missing content because map-step calls failed — the UI can
                // tell the user rather than presenting it as complete.
                let mut result_json = serde_json::json!({
                    "markdown": final_markdown,
                });
                if let Some(structured) = &output.structured {
                    result_json["structured"] =
                        serde_json::to_value(structured).unwrap_or(serde_json::Value::Null);
                }
                if output.failed_chunks > 0 {
                    warn!(
                        "Summary for {} is partial: {} chunk(s) failed",
                        meeting_id, output.failed_chunks
                    );
                    result_json["failed_chunks"] = serde_json::json!(output.failed_chunks);
                }

                // Update database with completed status
                if let Err(e) = SummaryProcessesRepository::update_process_completed(
                    &pool,
                    &meeting_id,
                    result_json.clone(),
                    output.chunks_processed,
                    duration,
                )
                .await
                {
                    error!("Failed to save completed process for {}: {}", meeting_id, e);
                } else {
                    info!("Summary saved successfully for meeting_id: {}", meeting_id);

                    // Best-effort: drop a portable summary.md alongside
                    // metadata.json / transcripts.json in the meeting
                    // folder. Failure is non-fatal — the DB row is the
                    // source of truth; the .md is a convenience export.
                    if let Err(e) = crate::summary::markdown_export::write_summary_md(
                        &pool,
                        &meeting_id,
                        &result_json,
                    )
                    .await
                    {
                        warn!(
                            "Failed to write summary.md sidecar for {}: {}",
                            meeting_id, e
                        );
                    }

                    match &output.structured {
                        // The structured summary already carries its action
                        // items: store them directly, no extra LLM pass.
                        Some(structured) => {
                            Self::store_structured_action_items(
                                &sink,
                                &pool,
                                &meeting_id,
                                structured,
                            )
                            .await
                        }
                        // Free-form fallback: extract structured action
                        // items from the *transcript* (grounded to their
                        // timestamps). Spawned rather than awaited — the
                        // summary is already saved and on screen; a slow or
                        // failing extraction must not delay or affect it.
                        // Only runs once the summary is committed, so a
                        // failure here can never roll one back.
                        None => {
                            crate::summary::transcript_action_items::spawn_transcript_extraction(
                                sink.clone(),
                                pool.clone(),
                                meeting_id.clone(),
                                model_provider.clone(),
                                model_name.clone(),
                            )
                        }
                    }

                    // Best-effort, no-op unless the user turned on export
                    // folder sync (Settings → Integrations); logs its own
                    // errors. Runs after the structured action items are
                    // stored so the note includes them; writes the stored
                    // (final rendered) Markdown on both paths.
                    crate::export::folder_sync::sync_after_summary(&pool, &meeting_id).await;
                }
            }
            Err(e) => {
                // Check if error is due to cancellation
                if e.contains("cancelled") {
                    info!(
                        "Summary generation was cancelled for meeting_id: {}",
                        meeting_id
                    );
                    if let Err(db_err) =
                        SummaryProcessesRepository::update_process_cancelled(&pool, &meeting_id)
                            .await
                    {
                        error!(
                            "Failed to update DB status to cancelled for {}: {}",
                            meeting_id, db_err
                        );
                    }
                } else {
                    Self::update_process_failed(&pool, &meeting_id, &e).await;
                }
            }
        }
    }

    /// Replace the meeting's extractor-owned action items with the ones the
    /// structured summary returned — the same slot (and the same cleanup and
    /// carry-over rules) the transcript extraction pass writes to, so ticked
    /// items survive a regeneration. Emits `action-items-extracted`.
    async fn store_structured_action_items(
        sink: &SharedEventSink,
        pool: &SqlitePool,
        meeting_id: &str,
        structured: &StructuredSummary,
    ) {
        let items = crate::summary::action_extraction::normalize_structured_items(
            structured
                .action_items
                .iter()
                .map(|a| (a.text.clone(), a.owner.clone(), a.due.clone())),
        );
        match ActionItemsRepository::replace_summary_items(pool, meeting_id, &items).await {
            Ok(count) => {
                info!(
                    "Stored {} action item(s) from the structured summary for {}",
                    count, meeting_id
                );
                let _ = sink.emit_event(
                    "action-items-extracted",
                    &serde_json::json!({ "meeting_id": meeting_id, "count": count }),
                );
            }
            Err(e) => error!(
                "Failed to store structured action items for {}: {}",
                meeting_id, e
            ),
        }
    }

    /// Updates the summary process status to failed with error message
    ///
    /// # Arguments
    /// * `pool` - SQLx connection pool
    /// * `meeting_id` - Meeting identifier
    /// * `error_msg` - Error message to store
    async fn update_process_failed(pool: &SqlitePool, meeting_id: &str, error_msg: &str) {
        error!(
            "Processing failed for meeting_id {}: {}",
            meeting_id, error_msg
        );
        if let Err(e) =
            SummaryProcessesRepository::update_process_failed(pool, meeting_id, error_msg).await
        {
            error!(
                "Failed to update DB status to failed for {}: {}",
                meeting_id, e
            );
        }
    }
}
