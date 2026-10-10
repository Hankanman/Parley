//! "Ask Parley": chat with a single meeting (issue #80).
//!
//! [`ask_meeting`] answers one question about one meeting with the same
//! provider/model the user configured for summaries. The context is the
//! speaker-attributed, `[mm:ss]`-stamped transcript plus the meeting's notes,
//! its summary and the prior chat turns (see [`context`] for how it's fitted
//! to the model's window). Answers stream as [`CHAT_STREAM_EVENT`] events
//! where the provider streams (today the built-in sidecar); every provider
//! returns the full answer at the end. History is stored per meeting in
//! `meeting_chat_messages` ([`repository`]).

pub mod context;
pub mod repository;

use crate::database::models::Transcript;
use crate::database::repositories::meeting_note::MeetingNotesRepository;
use crate::database::repositories::setting::SettingsRepository;
use crate::events::{EventSinkExt, SharedEventSink};
use crate::summary::llm_client::{generate_summary, LlmConfig};
use crate::summary::processor::clean_llm_markdown_output;
use context::{build_prompt, ChatTurn, ContextSegment, MeetingContext, TranscriptCoverage};
use once_cell::sync::Lazy;
use repository::{MeetingChatMessage, MeetingChatRepository, ROLE_ASSISTANT, ROLE_USER};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use context::{parse_citations, AnswerSpan};

/// Streamed answer text: `{ "meeting_id": String, "delta": String }`.
pub const CHAT_STREAM_EVENT: &str = "meeting-chat-stream";

/// A completed exchange: the stored question and its stored answer.
#[derive(Debug, Clone)]
pub struct AskOutcome {
    pub question: MeetingChatMessage,
    pub answer: MeetingChatMessage,
    pub coverage: TranscriptCoverage,
}

/// In-flight asks by meeting id, so the UI can stop one. Each entry carries
/// the registration id it was created with, so a finishing run never removes
/// a newer run's token.
static IN_FLIGHT: Lazy<Mutex<HashMap<String, (u64, CancellationToken)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static REGISTRATION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Stop the in-flight answer for `meeting_id`, if any.
pub fn cancel_ask(meeting_id: &str) -> bool {
    match IN_FLIGHT.lock() {
        Ok(map) => map.get(meeting_id).map(|(_, t)| t.cancel()).is_some(),
        Err(_) => false,
    }
}

/// Load everything about a meeting the chat prompt can use.
pub async fn load_meeting_context(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<MeetingContext, String> {
    let title: Option<(String,)> = sqlx::query_as("SELECT title FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("failed to load meeting: {e}"))?;
    let Some((title,)) = title else {
        return Err("Meeting not found".to_string());
    };

    let rows = sqlx::query_as::<_, Transcript>(
        "SELECT * FROM transcripts WHERE meeting_id = ? \
         ORDER BY COALESCE(audio_start_time, 0.0), sequence_id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("failed to load transcript: {e}"))?;
    let segments = rows
        .into_iter()
        .filter(|t| !t.transcript.trim().is_empty())
        .map(|t| ContextSegment {
            start: t.audio_start_time,
            speaker: t
                .speaker
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Speaker".to_string()),
            text: t.transcript.trim().to_string(),
        })
        .collect();

    let notes = MeetingNotesRepository::list_by_meeting(pool, meeting_id)
        .await
        .map_err(|e| format!("failed to load notes: {e}"))?
        .into_iter()
        .map(|n| n.body)
        .collect();

    // Just the result column: a summary row with an odd timestamp shouldn't
    // cost the chat its summary context.
    let summary_row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT result FROM summary_processes WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("failed to load summary: {e}"))?;
    let summary = summary_row
        .and_then(|(result,)| result)
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("markdown").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|m| !m.trim().is_empty());

    Ok(MeetingContext {
        title,
        summary,
        notes,
        segments,
    })
}

/// Answer `question` about `meeting_id` and store both turns.
///
/// The question is stored before the model runs; if generation fails or is
/// cancelled it's removed again, so history never holds an unanswered turn
/// and the UI can hand the text back to the input box.
pub async fn ask_meeting(
    sink: SharedEventSink,
    pool: SqlitePool,
    meeting_id: String,
    question: String,
) -> Result<AskOutcome, String> {
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

    let meeting = load_meeting_context(&pool, &meeting_id).await?;
    let history: Vec<ChatTurn> = MeetingChatRepository::list_by_meeting(&pool, &meeting_id)
        .await
        .map_err(|e| format!("failed to load chat history: {e}"))?
        .into_iter()
        .map(|m| ChatTurn {
            is_user: m.is_user(),
            content: m.content,
        })
        .collect();

    let prompt = build_prompt(&meeting, &history, &question, context_tokens);
    info!(
        "Ask Parley for {meeting_id}: provider={} model={} window={context_tokens} coverage={:?}",
        model.provider, model.model, prompt.coverage
    );

    let stored_question = MeetingChatRepository::append(&pool, &meeting_id, ROLE_USER, &question)
        .await
        .map_err(|e| format!("failed to save question: {e}"))?;

    let token = CancellationToken::new();
    let registration = REGISTRATION_COUNTER.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut map) = IN_FLIGHT.lock() {
        map.insert(meeting_id.clone(), (registration, token.clone()));
    }

    let stream_sink = sink.clone();
    let stream_meeting_id = meeting_id.clone();
    let on_delta = move |delta: &str| {
        let _ = stream_sink.emit_event(
            CHAT_STREAM_EVENT,
            &serde_json::json!({ "meeting_id": stream_meeting_id, "delta": delta }),
        );
    };
    let result = generate_summary(
        &config,
        &prompt.system,
        &prompt.user,
        Some(&token),
        Some(&on_delta),
    )
    .await;

    if let Ok(mut map) = IN_FLIGHT.lock() {
        // Only remove our own token (a newer ask may have replaced it).
        if map.get(&meeting_id).is_some_and(|(id, _)| *id == registration) {
            map.remove(&meeting_id);
        }
    }

    let answer_text = result
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
        });

    let answer_text = match answer_text {
        Ok(text) => text,
        Err(e) => {
            warn!("Ask Parley failed for {meeting_id}: {e}");
            let _ = MeetingChatRepository::delete(&pool, &stored_question.id).await;
            return Err(e);
        }
    };

    let answer = MeetingChatRepository::append(&pool, &meeting_id, ROLE_ASSISTANT, &answer_text)
        .await
        .map_err(|e| format!("failed to save answer: {e}"))?;

    Ok(AskOutcome {
        question: stored_question,
        answer,
        coverage: prompt.coverage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::meeting::MeetingsRepository;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn loads_transcript_notes_and_summary() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "Planning", None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, speaker, sequence_id)
             VALUES ('t2', 'm1', 'Second.', 'x', 20.0, 'Bob', 2),
                    ('t1', 'm1', 'First.', 'x', 5.0, NULL, 1),
                    ('t3', 'm1', '   ', 'x', 30.0, 'Bob', 3)",
        )
        .execute(&pool)
        .await
        .unwrap();
        MeetingNotesRepository::create(&pool, "m1", "Remember the deadline", "manual")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO summary_processes (meeting_id, status, created_at, updated_at, result)
             VALUES ('m1', 'completed', 'x', 'x', '{\"markdown\":\"## Decisions\\nShip it\"}')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let ctx = load_meeting_context(&pool, "m1").await.unwrap();
        assert_eq!(ctx.title, "Planning");
        assert_eq!(ctx.segments.len(), 2);
        assert_eq!(ctx.segments[0].text, "First.");
        assert_eq!(ctx.segments[0].speaker, "Speaker");
        assert_eq!(ctx.segments[1].speaker, "Bob");
        assert_eq!(ctx.notes, vec!["Remember the deadline".to_string()]);
        assert_eq!(ctx.summary.as_deref(), Some("## Decisions\nShip it"));

        assert!(load_meeting_context(&pool, "missing").await.is_err());
    }

    #[tokio::test]
    async fn ask_without_model_config_errors_and_stores_nothing() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "m1", "Planning", None)
            .await
            .unwrap();
        let sink: SharedEventSink = std::sync::Arc::new(crate::events::NullSink);
        let err = ask_meeting(sink.clone(), pool.clone(), "m1".into(), "  ".into())
            .await
            .unwrap_err();
        assert!(err.contains("question"));
        let err = ask_meeting(sink, pool.clone(), "m1".into(), "What?".into())
            .await
            .unwrap_err();
        assert!(err.contains("No model configured"), "{err}");
        assert!(MeetingChatRepository::list_by_meeting(&pool, "m1")
            .await
            .unwrap()
            .is_empty());
    }
}
