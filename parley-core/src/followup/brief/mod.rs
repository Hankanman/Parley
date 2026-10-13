//! Pre-meeting briefs.
//!
//! [`get_or_generate_brief`] is what the Brief view calls when it opens:
//!
//! 1. **Gather** ([`context::gather`]): related past meetings (same series,
//!    same people), their open action items and their stored summaries'
//!    latest decisions and open questions. Database only.
//! 2. **No history?** When nothing related was found, or none of it has a
//!    summary or open items, say so — no model call.
//! 3. **Prompt** ([`prompt::build_prompt`]): one `<meeting ref="M1" …>` block
//!    per meeting, trimmed to the model's window.
//! 4. **Cache**: briefs are stored per event *instance* (source + ICS UID +
//!    start) with a fingerprint of the prompt. An unchanged context reuses
//!    the stored brief; a changed one (a new meeting in the series, an item
//!    closed, a summary regenerated) regenerates it. Generation is
//!    single-flight, so two openers never pay for two calls.
//! 5. **Generate** with the summary model and validate
//!    ([`prompt::validate_brief`]). The brief is untrusted text: the UI
//!    renders it with [`prompt::render_markdown`] (escaped, citations as
//!    in-app links only).

pub mod context;
pub mod prompt;

use crate::calendar::models::CalendarEvent;
use crate::calendar::repository::CalendarRepository;
use crate::followup::fingerprint;
use crate::summary::processor::SummaryLlm;
use chrono::{DateTime, Duration, Utc};
use context::BriefContext;
use once_cell::sync::Lazy;
use prompt::{BriefPrompt, BriefSource};
use sqlx::SqlitePool;
use tracing::{info, warn};

pub use prompt::{format_event_start, link_target, render_markdown};

/// Briefs for events that started longer ago than this are pruned.
const CACHE_RETENTION_DAYS: i64 = 30;
/// Room kept for the brief itself in the model's window.
const OUTPUT_RESERVE_TOKENS: usize = 700;
const MIN_PROMPT_TOKENS: usize = 800;
const MAX_PROMPT_TOKENS: usize = 12_000;

/// Serialises brief generation (see the module docs).
static GENERATION_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

/// What the Brief view shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Brief {
    pub event_title: String,
    pub event_start: DateTime<Utc>,
    /// Other attendees.
    pub people: Vec<String>,
    pub body: BriefBody,
    /// The meetings the brief may cite, `M1` first.
    pub sources: Vec<BriefSource>,
    /// Related meetings found before selection.
    pub total_related: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BriefBody {
    /// No history to brief on; the message says why. No model was called.
    NoHistory(String),
    /// The model's brief (raw, untrusted — render with [`render_markdown`]).
    Generated {
        text: String,
        /// RFC 3339.
        generated_at: String,
        from_cache: bool,
    },
}

/// The cache key of an event instance.
pub fn cache_key(event: &CalendarEvent) -> String {
    format!(
        "{}|{}|{}",
        event.source_id,
        event.ics_uid,
        event.start_at.to_rfc3339()
    )
}

/// The brief for calendar event `event_id`, generating it when there's no
/// up-to-date cached one (or `force` is set).
pub async fn get_or_generate_brief(
    pool: SqlitePool,
    event_id: String,
    force: bool,
) -> Result<Brief, String> {
    let event = CalendarRepository::get_event(&pool, &event_id)
        .await
        .map_err(|e| format!("failed to load the calendar event: {e}"))?
        .ok_or_else(|| {
            "This event is no longer in your calendar. Try refreshing it in Settings.".to_string()
        })?;
    let ctx = context::gather(&pool, &event)
        .await
        .map_err(|e| format!("failed to gather past meetings: {e}"))?;

    if ctx.meetings.is_empty() || !ctx.has_substance() {
        return Ok(brief_from(
            &ctx,
            BriefBody::NoHistory(prompt::no_history_message(&ctx)),
            Vec::new(),
        ));
    }

    let config = super::resolve_llm(&pool).await?;
    let window = config.token_threshold().await;
    let budget = window
        .saturating_sub(OUTPUT_RESERVE_TOKENS)
        .clamp(MIN_PROMPT_TOKENS, MAX_PROMPT_TOKENS);
    let built = prompt::build_prompt(&ctx, budget);
    info!(
        "Brief for event {event_id}: {} related meeting(s), {} in the prompt, window={window}",
        ctx.total_related,
        built.sources.len()
    );
    let (text, generated_at, from_cache) =
        cached_or_generate(&pool, &config, &event, &built, force).await?;
    Ok(brief_from(
        &ctx,
        BriefBody::Generated {
            text,
            generated_at,
            from_cache,
        },
        built.sources,
    ))
}

fn brief_from(ctx: &BriefContext, body: BriefBody, sources: Vec<BriefSource>) -> Brief {
    Brief {
        event_title: ctx.event_title.clone(),
        event_start: ctx.event_start,
        people: ctx.people.clone(),
        body,
        sources,
        total_related: ctx.total_related,
    }
}

/// Reuse the cached brief for this event instance when its fingerprint
/// matches `built`, else generate and store a new one. Returns (text,
/// generated_at, from_cache).
pub async fn cached_or_generate<L: SummaryLlm>(
    pool: &SqlitePool,
    llm: &L,
    event: &CalendarEvent,
    built: &BriefPrompt,
    force: bool,
) -> Result<(String, String, bool), String> {
    let key = cache_key(event);
    let print = fingerprint(&format!("{}\n{}", built.system, built.user));
    let _guard = GENERATION_LOCK.lock().await;

    if !force {
        match BriefCacheRepository::get(pool, &key).await {
            Ok(Some(cached)) if cached.fingerprint == print => {
                return Ok((cached.brief, cached.created_at, true));
            }
            Ok(_) => {}
            Err(e) => warn!("brief cache read failed: {e}"),
        }
    }

    let raw = llm
        .complete(&built.system, &built.user, None, None, None)
        .await?;
    let text = prompt::validate_brief(&raw)?;
    let now = Utc::now().to_rfc3339();
    if let Err(e) =
        BriefCacheRepository::save(pool, &key, event, &print, &text, &built.sources).await
    {
        warn!("failed to cache brief: {e}");
    }
    Ok((text, now, false))
}

/// One cached brief.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedBrief {
    pub fingerprint: String,
    pub brief: String,
    pub sources: Vec<BriefSource>,
    pub created_at: String,
}

/// `meeting_briefs`: the brief cache.
pub struct BriefCacheRepository;

impl BriefCacheRepository {
    pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<CachedBrief>, sqlx::Error> {
        let row: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT fingerprint, brief, sources_json, created_at FROM meeting_briefs WHERE cache_key = ?",
        )
        .bind(key)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(
            |(fingerprint, brief, sources_json, created_at)| CachedBrief {
                fingerprint,
                brief,
                sources: serde_json::from_str(&sources_json).unwrap_or_default(),
                created_at,
            },
        ))
    }

    /// Store (or replace) a brief, and prune briefs for long-past events.
    pub async fn save(
        pool: &SqlitePool,
        key: &str,
        event: &CalendarEvent,
        fingerprint: &str,
        brief: &str,
        sources: &[BriefSource],
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        let sources_json = serde_json::to_string(sources).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO meeting_briefs (cache_key, source_id, ics_uid, start_at, fingerprint, brief, sources_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(cache_key) DO UPDATE SET
                fingerprint = excluded.fingerprint, brief = excluded.brief,
                sources_json = excluded.sources_json, created_at = excluded.created_at",
        )
        .bind(key)
        .bind(&event.source_id)
        .bind(&event.ics_uid)
        .bind(event.start_at.to_rfc3339())
        .bind(fingerprint)
        .bind(brief)
        .bind(&sources_json)
        .bind(now.to_rfc3339())
        .execute(pool)
        .await?;
        sqlx::query("DELETE FROM meeting_briefs WHERE julianday(start_at) < julianday(?)")
            .bind((now - Duration::days(CACHE_RETENTION_DAYS)).to_rfc3339())
            .execute(pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::models::Attendee;
    use crate::database::repositories::meeting::MeetingsRepository;
    use crate::summary::llm_client::{ResponseSchema, StreamSink};
    use chrono::TimeZone;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    struct CountingLlm {
        reply: String,
        calls: AtomicUsize,
    }

    impl SummaryLlm for CountingLlm {
        fn complete<'a>(
            &'a self,
            _s: &'a str,
            _u: &'a str,
            schema: Option<ResponseSchema<'a>>,
            _c: Option<&'a CancellationToken>,
            _d: Option<StreamSink<'a>>,
        ) -> impl Future<Output = Result<String, String>> + Send + 'a {
            async move {
                assert!(schema.is_none());
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.reply.clone())
            }
        }
    }

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn attendee(name: &str, email: &str) -> Attendee {
        Attendee {
            name: Some(name.into()).filter(|n: &String| !n.is_empty()),
            email: Some(email.into()).filter(|e: &String| !e.is_empty()),
            ..Default::default()
        }
    }

    async fn add_event(
        pool: &SqlitePool,
        id: &str,
        uid: &str,
        start: DateTime<Utc>,
        attendees: &[Attendee],
    ) -> CalendarEvent {
        sqlx::query(
            "INSERT OR IGNORE INTO calendar_sources (id, url, created_at, updated_at) VALUES ('src', 'https://x', 'x', 'x')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO calendar_events (id, source_id, ics_uid, recurrence_id, summary, start_at, end_at, attendees_json, fetched_at)
             VALUES (?, 'src', ?, ?, 'Weekly sync', ?, ?, ?, 'x')",
        )
        .bind(id)
        .bind(uid)
        .bind(start.to_rfc3339())
        .bind(start.to_rfc3339())
        .bind((start + Duration::hours(1)).to_rfc3339())
        .bind(serde_json::to_string(attendees).unwrap())
        .execute(pool)
        .await
        .unwrap();
        CalendarRepository::get_event(pool, id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn add_meeting(
        pool: &SqlitePool,
        id: &str,
        title: &str,
        at: DateTime<Utc>,
        event_id: Option<&str>,
    ) {
        MeetingsRepository::create_recording_meeting(pool, id, title, None)
            .await
            .unwrap();
        sqlx::query("UPDATE meetings SET created_at = ?, status = 'completed', calendar_event_id = ? WHERE id = ?")
            .bind(at.to_rfc3339())
            .bind(event_id)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn add_summary(pool: &SqlitePool, meeting_id: &str, decision: &str) {
        let result = serde_json::json!({
            "markdown": "x",
            "structured": { "overview": "o", "decisions": [decision], "open_questions": ["Q?"], "action_items": [] }
        });
        sqlx::query(
            "INSERT INTO summary_processes (meeting_id, status, created_at, updated_at, result) VALUES (?, 'completed', 'x', 'x', ?)",
        )
        .bind(meeting_id)
        .bind(result.to_string())
        .execute(pool)
        .await
        .unwrap();
    }

    fn day(d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, d, 9, 0, 0).unwrap()
    }

    /// Series + attendee + voice-profile + speaker-label matching, the user
    /// excluded, open items only, later meetings ignored.
    #[tokio::test]
    async fn gathers_series_and_people_history() {
        let pool = pool().await;
        let me = attendee("Seb", "seb@example.com");
        let ann = attendee("Ann Lee", "ann@example.com");
        let bob = attendee("", "bob@example.com");
        sqlx::query(
            "INSERT INTO voice_profiles (id, name, email, embedding, embedding_dim, sample_count, is_self, created_at, updated_at)
             VALUES ('vp-self', 'Seb', 'seb@example.com', x'', 0, 0, 1, 'x', 'x'),
                    ('vp-bob', 'Robert', 'BOB@example.com', x'', 0, 0, 0, 'x', 'x')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // The calendar the event lives in is listed as an attendee too
        // (Google does this); it must not count as a person.
        let calendar = attendee("Team calendar", "abc123@group.calendar.google.com");
        let upcoming = add_event(
            &pool,
            "e-next",
            "weekly",
            day(30),
            &[me.clone(), ann.clone(), bob.clone(), calendar.clone()],
        )
        .await;

        // Same series, earlier occurrence.
        add_event(&pool, "e-1", "weekly", day(1), &[me.clone()]).await;
        add_meeting(&pool, "m-series", "Weekly sync #1", day(1), Some("e-1")).await;
        add_summary(&pool, "m-series", "Ship v2").await;
        sqlx::query(
            "INSERT INTO action_items (id, meeting_id, text, assignee, status, source, created_at, updated_at)
             VALUES ('a1', 'm-series', 'Send deck', 'Ann', 'open', 'manual', 'x', 'x'),
                    ('a2', 'm-series', 'Old task', NULL, 'done', 'manual', 'x', 'x')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Different event, Ann attending (by email).
        add_event(
            &pool,
            "e-other",
            "other",
            day(5),
            &[me.clone(), attendee("Ann", "ANN@example.com")],
        )
        .await;
        add_meeting(&pool, "m-ann", "Design review", day(5), Some("e-other")).await;
        add_summary(&pool, "m-ann", "Use the blue theme").await;

        // No event, but Bob's voice profile speaks.
        add_meeting(&pool, "m-bob", "Hallway chat", day(7), None).await;
        add_summary(&pool, "m-bob", "Hire a contractor").await;
        // A segment labelled with Ann's name.
        add_meeting(&pool, "m-label", "Labelled", day(8), None).await;
        add_summary(&pool, "m-label", "Move the offsite").await;
        // Only the user speaks: unrelated.
        add_meeting(&pool, "m-self", "Solo", day(9), None).await;
        add_summary(&pool, "m-self", "Nothing").await;
        // Only shares the calendar "attendee": unrelated.
        add_event(
            &pool,
            "e-cal",
            "cal",
            day(10),
            &[me.clone(), calendar.clone()],
        )
        .await;
        add_meeting(&pool, "m-cal", "Same calendar", day(10), Some("e-cal")).await;
        add_summary(&pool, "m-cal", "Nothing").await;
        // Related (Ann attends) but nothing to brief on: counted, not selected.
        add_event(&pool, "e-empty", "empty", day(11), &[ann.clone()]).await;
        add_meeting(&pool, "m-empty", "No summary yet", day(11), Some("e-empty")).await;
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, speaker, voice_profile_id, sequence_id)
             VALUES ('t1', 'm-bob', 'hi', 'x', 'Robert', 'vp-bob', 1),
                    ('t2', 'm-label', 'hello', 'x', 'ann lee', NULL, 1),
                    ('t3', 'm-self', 'me', 'x', 'Seb', 'vp-self', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Same series but after the event starts: ignored.
        add_event(
            &pool,
            "e-later",
            "weekly",
            Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap(),
            &[],
        )
        .await;
        add_meeting(
            &pool,
            "m-later",
            "Later",
            Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap(),
            Some("e-later"),
        )
        .await;

        let ctx = context::gather(&pool, &upcoming).await.unwrap();
        assert_eq!(
            ctx.people,
            vec!["Ann Lee".to_string(), "bob@example.com".to_string()]
        );
        let ids: Vec<&str> = ctx.meetings.iter().map(|m| m.meeting_id.as_str()).collect();
        assert_eq!(ids, vec!["m-label", "m-bob", "m-ann", "m-series"]);
        assert_eq!(
            ctx.total_related, 5,
            "m-empty is related but has nothing to brief on"
        );

        let series = &ctx.meetings[3];
        assert!(series.same_series);
        assert_eq!(series.open_items.len(), 1, "done items are left out");
        assert_eq!(series.open_items[0].text, "Send deck");
        assert_eq!(
            series.summary.structured.as_ref().unwrap().decisions,
            vec!["Ship v2".to_string()]
        );
        assert_eq!(ctx.meetings[2].people, vec!["Ann Lee".to_string()]);
        assert_eq!(ctx.meetings[1].people, vec!["bob@example.com".to_string()]);
        assert_eq!(ctx.meetings[0].people, vec!["Ann Lee".to_string()]);
        assert!(ctx.has_substance());
    }

    #[tokio::test]
    async fn no_history_skips_the_model() {
        let pool = pool().await;
        add_event(
            &pool,
            "e-next",
            "weekly",
            day(30),
            &[attendee("Zed", "zed@example.com")],
        )
        .await;
        add_meeting(&pool, "m1", "Unrelated", day(1), None).await;
        // No model configured either: would error if it were called.
        let brief = get_or_generate_brief(pool, "e-next".into(), false)
            .await
            .unwrap();
        assert!(
            matches!(brief.body, BriefBody::NoHistory(ref m) if m.contains("No past meetings"))
        );
        assert!(brief.sources.is_empty());
    }

    #[tokio::test]
    async fn caches_per_event_instance_until_context_changes() {
        let pool = pool().await;
        let event = add_event(
            &pool,
            "e-next",
            "weekly",
            Utc::now() + Duration::hours(1),
            &[],
        )
        .await;
        let llm = CountingLlm {
            reply: "<think>hm</think>- Ship v2 was agreed [M1]".into(),
            calls: AtomicUsize::new(0),
        };
        let built = |user: &str| BriefPrompt {
            system: "s".into(),
            user: user.into(),
            sources: vec![],
        };

        let (text, _, cached) = cached_or_generate(&pool, &llm, &event, &built("u1"), false)
            .await
            .unwrap();
        assert_eq!(text, "- Ship v2 was agreed [M1]");
        assert!(!cached);
        let (_, _, cached) = cached_or_generate(&pool, &llm, &event, &built("u1"), false)
            .await
            .unwrap();
        assert!(cached);
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);

        // Changed context, or forced: regenerate.
        cached_or_generate(&pool, &llm, &event, &built("u2"), false)
            .await
            .unwrap();
        cached_or_generate(&pool, &llm, &event, &built("u2"), true)
            .await
            .unwrap();
        assert_eq!(llm.calls.load(Ordering::SeqCst), 3);
        let stored = BriefCacheRepository::get(&pool, &cache_key(&event))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.brief, "- Ship v2 was agreed [M1]");
    }

    #[tokio::test]
    async fn deleting_a_cited_meeting_drops_its_briefs() {
        let pool = pool().await;
        let event = add_event(&pool, "e-next", "weekly", Utc::now(), &[]).await;
        add_meeting(&pool, "m1", "Cited", day(1), None).await;
        let sources = vec![BriefSource {
            label: "M1".into(),
            meeting_id: "m1".into(),
            title: "Cited".into(),
            created_at: None,
        }];
        BriefCacheRepository::save(&pool, "k", &event, "f", "brief", &sources)
            .await
            .unwrap();
        MeetingsRepository::delete_meeting(&pool, "m1")
            .await
            .unwrap();
        assert!(BriefCacheRepository::get(&pool, "k")
            .await
            .unwrap()
            .is_none());
    }
}
