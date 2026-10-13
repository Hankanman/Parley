//! "Ask Parley" across all meetings (issue #81).
//!
//! [`ask_across`] answers a question from the whole meeting library with the
//! summary provider/model:
//!
//! 1. **Retrieve** ([`retrieval`]): FTS5 keyword candidates (query-expanded),
//!    fused with semantic candidates from local embeddings when available
//!    ([`embeddings`]), narrowed by the [`AskFilters`] (person, date range,
//!    calendar series) and cut down to a few passages that fit the model.
//! 2. **Prompt** ([`prompt`]): the passages grouped by meeting, each meeting
//!    labelled `M1…` with its title and date, lines stamped `[mm:ss]`.
//! 3. **Answer**: streamed as [`ASK_ACROSS_STREAM_EVENT`] where the provider
//!    streams; the result carries the [`SourceRef`]s, which the UI needs to
//!    resolve `[M1 12:34]` citations (see [`prompt::link_citations`]).
//!
//! When retrieval finds nothing, no model is called: the answer says so.
//!
//! ## History
//!
//! Cross-meeting chat history is **kept in memory by the UI** for the
//! session, not stored. Its answers quote many meetings, so a stored copy
//! would keep quoting a meeting after the user deleted it — per-meeting chat
//! can cascade-delete with its meeting, a global log can't cleanly. The UI
//! passes recent turns back in for follow-ups.

pub mod embeddings;
pub mod prompt;
pub mod retrieval;

use crate::chat::context::ChatTurn;
use crate::database::repositories::setting::SettingsRepository;
use crate::events::{EventSinkExt, SharedEventSink};
use crate::summary::llm_client::{generate_summary, LlmConfig};
use crate::summary::processor::clean_llm_markdown_output;
use once_cell::sync::Lazy;
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use embeddings::SemanticAvailability;
pub use prompt::{
    citation_link_target, format_meeting_date, link_citations, parse_citations, CrossSpan,
    SourceRef,
};
pub use retrieval::{
    list_people, list_series, AskFilters, PersonFilter, PersonOption, RetrievalStats,
    SelectionLimits, SeriesFilter, SeriesOption,
};

/// Streamed answer text: `{ "request_id": u64, "delta": String }`.
pub const ASK_ACROSS_STREAM_EVENT: &str = "ask-across-stream";

/// Semantic hits considered per question.
const SEMANTIC_LIMIT: usize = 40;
/// Cap on the excerpt budget even for large-context models: small models
/// do worse with more, and big ones don't need it to cite well.
const MAX_EXCERPT_TOKENS: usize = 6_000;
/// Floor, so tiny windows still get something.
const MIN_EXCERPT_TOKENS: usize = 600;

/// What the semantic stage did for one question.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticUse {
    Used { model_id: String },
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct AcrossOutcome {
    pub question: String,
    /// The model's answer (raw markdown — render with [`link_citations`]).
    pub answer: String,
    pub sources: Vec<SourceRef>,
    pub stats: RetrievalStats,
    pub semantic: SemanticUse,
}

static IN_FLIGHT: Lazy<Mutex<Option<(u64, CancellationToken)>>> = Lazy::new(|| Mutex::new(None));
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A fresh id for [`ask_across`], to match streamed deltas to the request.
pub fn next_request_id() -> u64 {
    REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Stop the in-flight cross-meeting answer, if any.
pub fn cancel_ask_across() -> bool {
    match IN_FLIGHT.lock() {
        Ok(slot) => slot.as_ref().map(|(_, t)| t.cancel()).is_some(),
        Err(_) => false,
    }
}

/// Excerpt budget for a model window of `context_tokens`.
pub fn excerpt_budget(context_tokens: usize, fixed_tokens: usize) -> usize {
    let answer_reserve = (context_tokens / 8).clamp(256, 2_048);
    context_tokens
        .saturating_sub(answer_reserve + fixed_tokens)
        .clamp(MIN_EXCERPT_TOKENS, MAX_EXCERPT_TOKENS)
}

/// Kick off background embedding of new/changed meetings, if a backend is
/// configured. Cheap to call often (single-flight, cooldown on failure).
pub async fn refresh_semantic_index(pool: &SqlitePool) {
    if let Ok(embedder) = embeddings::resolve_embedder(pool).await {
        embeddings::spawn_backfill(pool.clone(), embedder);
    }
}

/// Semantic-search status for the UI: availability plus
/// (indexed, total) meetings when it's configured.
pub async fn semantic_status(pool: &SqlitePool) -> (SemanticAvailability, Option<(i64, i64)>) {
    let availability = embeddings::availability(pool).await;
    let progress = match &availability {
        SemanticAvailability::Ready { model_id }
        | SemanticAvailability::Failing { model_id, .. } => {
            embeddings::index_progress(pool, model_id).await.ok()
        }
        SemanticAvailability::Off(_) => None,
    };
    (availability, progress)
}

/// The answer given when retrieval finds nothing.
pub fn nothing_found_answer(filters: &AskFilters) -> String {
    if filters.is_empty() {
        "I couldn't find anything about that in your meetings.".to_string()
    } else {
        "I couldn't find anything about that in the meetings matching your filters. Try widening them.".to_string()
    }
}

/// Answer `question` from all meetings passing `filters`. `history` is the
/// session's prior turns (oldest first), used for follow-ups.
pub async fn ask_across(
    sink: SharedEventSink,
    pool: SqlitePool,
    request_id: u64,
    question: String,
    history: Vec<ChatTurn>,
    filters: AskFilters,
) -> Result<AcrossOutcome, String> {
    let question = question.trim().to_string();
    if question.is_empty() {
        return Err("Type a question first.".to_string());
    }

    let model = SettingsRepository::get_model_config(&pool)
        .await
        .map_err(|e| format!("failed to read model settings: {e}"))?
        .filter(|m| !m.provider.is_empty() && !m.model.is_empty())
        .ok_or_else(|| "No model configured — set one up in Settings first.".to_string())?;
    let config = LlmConfig::resolve(
        &pool,
        &model.provider,
        &model.model,
        crate::paths::app_data_dir().ok(),
    )
    .await?;
    let context_tokens = config.token_threshold().await;

    // Retrieval query: the question plus the previous question, which
    // carries the topic of a follow-up ("and who owns that?").
    let mut query_text = question.clone();
    if let Some(prev) = history.iter().rev().find(|t| t.is_user) {
        query_text.push(' ');
        query_text.push_str(&prev.content);
    }

    // Semantic stage (best effort).
    let (semantic_hits, semantic) = match embeddings::resolve_embedder(&pool).await {
        Ok(embedder) => {
            let model_id = embedder.model_id();
            let result = embeddings::semantic_candidates(
                &pool,
                embedder.as_ref(),
                &query_text,
                &filters,
                SEMANTIC_LIMIT,
            )
            .await;
            // Index whatever is new for next time.
            embeddings::spawn_backfill(pool.clone(), embedder);
            match result {
                Ok(hits) => (hits, SemanticUse::Used { model_id }),
                Err(e) => {
                    warn!("Ask across meetings: semantic stage unavailable: {e}");
                    (Vec::new(), SemanticUse::Unavailable(e))
                }
            }
        }
        Err(SemanticAvailability::Off(reason)) => (Vec::new(), SemanticUse::Unavailable(reason)),
        Err(other) => (Vec::new(), SemanticUse::Unavailable(format!("{other:?}"))),
    };

    let budget = excerpt_budget(
        context_tokens,
        prompt::fixed_prompt_tokens(&question, &history),
    );
    let retrieved = retrieval::retrieve(
        &pool,
        &query_text,
        &filters,
        &semantic_hits,
        budget,
        SelectionLimits::default(),
    )
    .await
    .map_err(|e| format!("search failed: {e}"))?;

    if retrieved.meetings.is_empty() {
        return Ok(AcrossOutcome {
            question,
            answer: nothing_found_answer(&filters),
            sources: Vec::new(),
            stats: retrieved.stats,
            semantic,
        });
    }

    let filter_note = filters.describe();
    let built = prompt::build_prompt(
        &retrieved.meetings,
        &history,
        &question,
        filter_note.as_deref(),
    );
    info!(
        "Ask across meetings: provider={} model={} window={context_tokens} budget={budget} sources={} stats={:?} semantic={:?}",
        model.provider,
        model.model,
        built.sources.len(),
        retrieved.stats,
        semantic
    );

    let token = CancellationToken::new();
    if let Ok(mut slot) = IN_FLIGHT.lock() {
        *slot = Some((request_id, token.clone()));
    }
    let stream_sink = sink.clone();
    let on_delta = move |delta: &str| {
        let _ = stream_sink.emit_event(
            ASK_ACROSS_STREAM_EVENT,
            &serde_json::json!({ "request_id": request_id, "delta": delta }),
        );
    };
    let result = generate_summary(
        &config,
        &built.system,
        &built.user,
        Some(&token),
        Some(&on_delta),
    )
    .await;
    if let Ok(mut slot) = IN_FLIGHT.lock() {
        if slot.as_ref().is_some_and(|(id, _)| *id == request_id) {
            *slot = None;
        }
    }

    let answer = result
        .map(|raw| clean_llm_markdown_output(&raw))
        .and_then(|text| {
            if text.trim().is_empty() {
                Err("The model returned an empty answer.".to_string())
            } else {
                Ok(text)
            }
        })
        .map_err(|e| {
            if token.is_cancelled() {
                "Stopped.".to_string()
            } else {
                e
            }
        })?;

    Ok(AcrossOutcome {
        question,
        answer,
        sources: built.sources,
        stats: retrieved.stats,
        semantic,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_clamped() {
        assert_eq!(excerpt_budget(200_000, 500), MAX_EXCERPT_TOKENS);
        assert_eq!(excerpt_budget(1_000, 900), MIN_EXCERPT_TOKENS);
        // 4k window: reserve 500, fixed 500.
        assert_eq!(excerpt_budget(4_000, 500), 3_000);
    }

    #[tokio::test]
    async fn ask_validates_input_and_model() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let sink: SharedEventSink = std::sync::Arc::new(crate::events::NullSink);
        let err = ask_across(
            sink.clone(),
            pool.clone(),
            1,
            " ".into(),
            vec![],
            AskFilters::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("question"));
        let err = ask_across(sink, pool, 2, "What?".into(), vec![], AskFilters::default())
            .await
            .unwrap_err();
        assert!(err.contains("No model configured"), "{err}");
    }

    #[test]
    fn nothing_found_mentions_filters() {
        assert!(!nothing_found_answer(&AskFilters::default()).contains("filters"));
        let f = AskFilters {
            from: Some(chrono::Utc::now()),
            ..Default::default()
        };
        assert!(nothing_found_answer(&f).contains("filters"));
    }
}
