//! The MCP server: tool definitions, resource exposure, and the mapping from
//! database errors to MCP errors.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    ErrorData, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{db, extras, semantic};

/// Turn a sqlx error into an MCP error without leaking the whole SQL string
/// into the agent's context, while keeping the detail in the server log.
fn db_err(context: &'static str, e: sqlx::Error) -> ErrorData {
    tracing::error!(error = %e, context, "database error");
    ErrorData::internal_error(format!("{context}: {e}"), None)
}

fn action_item_not_found(id: &str) -> ErrorData {
    ErrorData::invalid_params(
        format!("no action item with id {id:?}. Use `get_action_items` to find valid ids."),
        None,
    )
}

fn not_found(what: &str, id: &str) -> ErrorData {
    ErrorData::invalid_params(
        format!("no {what} with id {id:?}. Use `list_meetings` to find valid ids."),
        None,
    )
}

/// Reject empty/whitespace-only text before it reaches the database, where it
/// would become an unhelpful row rather than an error.
fn require_nonempty(value: &str, field: &str) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ErrorData::invalid_params(
            format!("`{field}` must not be empty."),
            None,
        ));
    }
    Ok(trimmed.to_string())
}

/// Normalize an optional string arg: absent and blank mean the same thing.
fn optional(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Tool arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListMeetingsArgs {
    /// Max meetings to return. Default 50, capped at 500.
    pub limit: Option<u32>,
    /// Only meetings created on or after this UTC date, as YYYY-MM-DD.
    pub since: Option<String>,
    /// Only meetings created on or before this UTC date, as YYYY-MM-DD.
    pub until: Option<String>,
    /// Case-insensitive substring match against the meeting title.
    pub query: Option<String>,
    /// Only meetings with this person: a `speaker_id` from `list_speakers`,
    /// or a name or email (exact, case-insensitive) matched against
    /// recognised speakers, transcript speaker labels and the linked
    /// calendar event's attendees.
    pub person: Option<String>,
    /// Only occurrences of one recurring calendar meeting: a `series_id`
    /// from `get_meeting` or `list_calendar_events`.
    pub series_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCalendarEventsArgs {
    /// Start of the window, as YYYY-MM-DD (UTC). Default: now.
    pub from: Option<String>,
    /// End of the window, inclusive, as YYYY-MM-DD (UTC). Default: 7 days
    /// after `from`.
    pub to: Option<String>,
    /// Case-insensitive substring match against the event title.
    pub query: Option<String>,
    /// Max events to return, earliest first. Default 50, capped at 500.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EventIdArgs {
    /// The calendar event's id, as returned by `list_calendar_events` or
    /// `get_meeting`.
    pub event_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SemanticSearchArgs {
    /// A question or description in natural language ("how much will the
    /// launch cost"). Matches meaning, not exact words.
    pub query: String,
    /// Max passages to return, best first. Default 10, capped at 50.
    pub limit: Option<u32>,
    /// Only meetings with this person (as in `list_meetings`).
    pub person: Option<String>,
    /// Only meetings created on or after this UTC date, as YYYY-MM-DD.
    pub since: Option<String>,
    /// Only meetings created on or before this UTC date, as YYYY-MM-DD.
    pub until: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateActionItemArgs {
    /// The action item's id, as returned by `get_action_items`.
    pub action_item_id: String,
    /// New task text. Omit to keep it.
    pub text: Option<String>,
    /// New owner. Omit to keep it; an empty string clears it.
    pub assignee: Option<String>,
    /// New free-text due hint ("by Friday"). Omit to keep it; an empty
    /// string clears it.
    pub due_hint: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActionItemIdArgs {
    /// The action item's id, as returned by `get_action_items`.
    pub action_item_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeetingIdArgs {
    /// The meeting's id, as returned by `list_meetings`.
    pub meeting_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTranscriptArgs {
    /// The meeting's id, as returned by `list_meetings`.
    pub meeting_id: String,
    /// Attribute each line to a speaker and include per-segment detail.
    /// Default true. Set false for plain unattributed text.
    pub include_speakers: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchTranscriptsArgs {
    /// Words to find. Full-text keyword search: every word must appear in the
    /// segment (any order), case- and accent-insensitive, and the last word
    /// also matches as a prefix ("budg" finds "budget", "budgets"). Not
    /// semantic — synonyms don't match. Search operators and quotes are
    /// treated as plain text.
    pub query: String,
    /// Max matching segments to return. Default 50, capped at 500.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetActionItemsArgs {
    /// Restrict to one meeting. Omit for items across all meetings.
    pub meeting_id: Option<String>,
    /// Restrict to "open" or "done". Omit for both.
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetRecentSummariesArgs {
    /// Max summaries to return, most recently updated first. Default 5,
    /// capped at 500.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddMeetingNoteArgs {
    /// The meeting to attach the note to.
    pub meeting_id: String,
    /// The note text. Markdown is fine.
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateActionItemArgs {
    /// The meeting the action item came out of.
    pub meeting_id: String,
    /// The task, e.g. "Send the Q3 budget to Finance".
    pub text: String,
    /// Who owns it, if known.
    pub assignee: Option<String>,
    /// Free-text due hint as discussed ("by Friday", "next sprint"). Stored
    /// verbatim, not parsed into a date.
    pub due_hint: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetActionItemStatusArgs {
    /// The action item's id, as returned by `get_action_items`.
    pub action_item_id: String,
    /// "open" or "done".
    pub status: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateSummaryArgs {
    /// The meeting whose summary to replace.
    pub meeting_id: String,
    /// The full replacement summary, in markdown. This REPLACES the existing
    /// summary rather than appending to it — read `get_meeting` first if you
    /// mean to revise rather than overwrite.
    pub markdown: String,
}

// ---------------------------------------------------------------------------
// Tool results
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListMeetingsResult {
    pub count: usize,
    pub meetings: Vec<db::MeetingListItem>,
}

/// Everything about one meeting except its transcript.
#[derive(Debug, Serialize, JsonSchema)]
pub struct MeetingResult {
    #[serde(flatten)]
    pub meeting: db::MeetingDetail,
    /// The calendar event the meeting is linked to: title, time, location,
    /// organizer, attendees and series.
    pub calendar_event: Option<extras::CalendarEvent>,
    /// Topic chapters, in order.
    pub chapters: Vec<extras::Chapter>,
    /// Moments the user bookmarked while recording.
    pub bookmarks: Vec<extras::Bookmark>,
    /// The user's (and agents') notes on the meeting.
    pub notes: Vec<db::MeetingNote>,
    /// Whether a follow-up email draft exists (`get_followup_draft`).
    pub has_followup_draft: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CalendarEventsResult {
    pub count: usize,
    pub from: String,
    pub to: String,
    pub events: Vec<extras::CalendarEvent>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SpeakersResult {
    pub count: usize,
    pub speakers: Vec<extras::Speaker>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SemanticSearchResult {
    pub count: usize,
    pub matches: Vec<semantic::SemanticHit>,
    /// How many meetings the app has indexed. Parley indexes in the
    /// background while Ask Parley is open, so recent meetings may be
    /// missing; `search_transcripts` covers everything.
    pub index: semantic::IndexCoverage,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeletedResult {
    pub deleted: bool,
    pub id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TranscriptResult {
    pub meeting_id: String,
    pub title: String,
    pub segment_count: usize,
    /// The transcript rendered as readable text.
    pub text: String,
    /// Per-segment detail; present only when `include_speakers` is true.
    pub segments: Option<Vec<db::TranscriptSegment>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MeetingStatsResult {
    pub meeting_id: String,
    pub title: String,
    /// The stats as a short readable report.
    pub text: String,
    /// Structured stats: per-speaker talk time (seconds) and share (0-1),
    /// turns, words per minute, longest monologue, questions, interruptions;
    /// meeting duration, silence ratio and mic-vs-system talk share.
    #[schemars(with = "serde_json::Value")]
    pub stats: crate::stats::MeetingStats,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResult {
    pub count: usize,
    /// True when the result was cut off at `limit` — there may be more.
    pub truncated: bool,
    pub matches: Vec<db::TranscriptHit>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActionItemsResult {
    pub count: usize,
    pub action_items: Vec<db::ActionItem>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SummariesResult {
    pub count: usize,
    pub summaries: Vec<db::MeetingSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UpdateSummaryResult {
    pub meeting_id: String,
    /// The summary process status after the write. Preserved from any existing
    /// row — writing a summary never changes it.
    pub status: String,
    pub updated_at: String,
    /// Present when the write may be overwritten by an in-flight generation.
    pub warning: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ParleyMcp {
    pool: SqlitePool,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl ParleyMcp {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "list_meetings",
        description = "List recorded meetings, newest first, with whether each \
                       has a summary and how many action items are still open. \
                       Start here to find a meeting_id."
    )]
    pub async fn list_meetings(
        &self,
        Parameters(args): Parameters<ListMeetingsArgs>,
    ) -> Result<Json<ListMeetingsResult>, ErrorData> {
        let since = args
            .since
            .as_deref()
            .map(|v| db::validate_date(v, "since"))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let until = args
            .until
            .as_deref()
            .map(|v| db::validate_date(v, "until"))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let query = optional(args.query);
        let person = optional(args.person);
        let series = optional(args.series_id);
        let series = series
            .as_deref()
            .map(|s| {
                extras::parse_series_id(s).ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("`series_id` {s:?} isn't a series id from `get_meeting` or `list_calendar_events`."),
                        None,
                    )
                })
            })
            .transpose()?;

        let meetings = db::list_meetings(
            &self.pool,
            db::clamp_limit(args.limit, 50),
            since.as_deref(),
            until.as_deref(),
            query.as_deref(),
            person.as_deref(),
            series,
        )
        .await
        .map_err(|e| db_err("failed to list meetings", e))?;

        Ok(Json(ListMeetingsResult {
            count: meetings.len(),
            meetings,
        }))
    }

    #[tool(
        name = "get_meeting",
        description = "Get everything about one meeting except the transcript: \
                       metadata, the generated summary (markdown), the linked \
                       calendar event with its attendees, chapters, bookmarks, \
                       notes, and counts of transcript segments and action items."
    )]
    pub async fn get_meeting(
        &self,
        Parameters(args): Parameters<MeetingIdArgs>,
    ) -> Result<Json<MeetingResult>, ErrorData> {
        self.load_meeting(&args.meeting_id).await.map(Json)
    }

    #[tool(
        name = "list_calendar_events",
        description = "List events from the user's calendar feeds in a date \
                       window (default: the next 7 days), earliest first, with \
                       attendees, the meetings recorded for each, and whether \
                       Parley has prepared a brief. Use it to prepare for \
                       upcoming meetings or to find what a meeting was about."
    )]
    pub async fn list_calendar_events(
        &self,
        Parameters(args): Parameters<ListCalendarEventsArgs>,
    ) -> Result<Json<CalendarEventsResult>, ErrorData> {
        let from = optional(args.from)
            .map(|v| db::validate_date(&v, "from"))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let to = optional(args.to)
            .map(|v| db::validate_date(&v, "to"))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let from = match from {
            Some(d) => format!("{d}T00:00:00+00:00"),
            None => chrono::Utc::now().to_rfc3339(),
        };
        let to = match to {
            Some(d) => format!("{d}T23:59:59+00:00"),
            None => chrono::DateTime::parse_from_rfc3339(&from)
                .map(|f| (f + chrono::Duration::days(7)).to_rfc3339())
                .unwrap_or_else(|_| from.clone()),
        };
        let query = optional(args.query);
        let events = extras::list_calendar_events(
            &self.pool,
            &from,
            &to,
            query.as_deref(),
            db::clamp_limit(args.limit, 50),
        )
        .await
        .map_err(|e| db_err("failed to list calendar events", e))?;
        Ok(Json(CalendarEventsResult {
            count: events.len(),
            from,
            to,
            events,
        }))
    }

    #[tool(
        name = "get_brief",
        description = "Get the pre-meeting brief Parley prepared for a calendar \
                       event: what was discussed and decided in related past \
                       meetings and what is still open, citing them as [M1], \
                       [M2]… (see `sources`). Briefs exist only once the user \
                       has opened them in Parley; `has_brief` on the event says \
                       whether one is ready."
    )]
    pub async fn get_brief(
        &self,
        Parameters(args): Parameters<EventIdArgs>,
    ) -> Result<Json<extras::Brief>, ErrorData> {
        if let Some(brief) = extras::get_brief(&self.pool, &args.event_id)
            .await
            .map_err(|e| db_err("failed to get brief", e))?
        {
            return Ok(Json(brief));
        }
        let exists = extras::get_calendar_event(&self.pool, &args.event_id)
            .await
            .map_err(|e| db_err("failed to get calendar event", e))?
            .is_some();
        Err(ErrorData::invalid_params(
            if exists {
                "Parley hasn't prepared a brief for this event yet (the user opens it from \
                 Upcoming in the sidebar). Use `list_meetings` with its `series_id` or \
                 attendees to gather the past meetings yourself."
                    .to_string()
            } else {
                format!(
                    "no calendar event with id {:?}. Use `list_calendar_events` to find valid ids.",
                    args.event_id
                )
            },
            None,
        ))
    }

    #[tool(
        name = "list_speakers",
        description = "List the people Parley recognises by voice (saved \
                       voice profiles), with how many meetings each spoke in. \
                       Pass a `speaker_id` or name as `person` to \
                       `list_meetings` or `semantic_search`."
    )]
    pub async fn list_speakers(&self) -> Result<Json<SpeakersResult>, ErrorData> {
        let speakers = extras::list_speakers(&self.pool)
            .await
            .map_err(|e| db_err("failed to list speakers", e))?;
        Ok(Json(SpeakersResult {
            count: speakers.len(),
            speakers,
        }))
    }

    #[tool(
        name = "get_followup_draft",
        description = "Get the follow-up email the user drafted in Parley for a \
                       meeting (subject, body, tone). Parley never sends it."
    )]
    pub async fn get_followup_draft(
        &self,
        Parameters(args): Parameters<MeetingIdArgs>,
    ) -> Result<Json<extras::FollowupDraft>, ErrorData> {
        if let Some(draft) = extras::get_followup_draft(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to get follow-up draft", e))?
        {
            return Ok(Json(draft));
        }
        if !db::meeting_exists(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to check meeting", e))?
        {
            return Err(not_found("meeting", &args.meeting_id));
        }
        Err(ErrorData::invalid_params(
            "no follow-up email has been drafted for this meeting. You can write one \
             from `get_meeting` (summary and attendees) and `get_action_items`."
                .to_string(),
            None,
        ))
    }

    #[tool(
        name = "semantic_search",
        description = "Search transcripts by meaning rather than exact words, \
                       using the embeddings Parley builds for Ask Parley: finds \
                       passages about a topic even when they're phrased \
                       differently. Needs Ollama as Parley's summary provider \
                       and only covers meetings Parley has indexed; otherwise \
                       use `search_transcripts`."
    )]
    pub async fn semantic_search(
        &self,
        Parameters(args): Parameters<SemanticSearchArgs>,
    ) -> Result<Json<SemanticSearchResult>, ErrorData> {
        let query = require_nonempty(&args.query, "query")?;
        let since = optional(args.since)
            .map(|v| db::validate_date(&v, "since"))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let until = optional(args.until)
            .map(|v| db::validate_date(&v, "until"))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let person = optional(args.person);
        let limit = args.limit.unwrap_or(10).clamp(1, 50) as usize;

        let backend = semantic::resolve_backend(&self.pool)
            .await
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let model_id = backend.model_id();
        let index = semantic::coverage(&self.pool, &model_id)
            .await
            .map_err(|e| db_err("failed to read the search index", e))?;
        if index.meetings_indexed == 0 {
            return Err(ErrorData::invalid_params(
                format!(
                    "no meetings are indexed for semantic search with {model_id} yet. Parley \
                     builds the index while Ask Parley is open; use `search_transcripts` meanwhile."
                ),
                None,
            ));
        }
        let vector = semantic::embed_query(&backend, &query).await.map_err(|e| {
            tracing::warn!(error = %e, "semantic search: embedding failed");
            ErrorData::internal_error(format!("{e}. Use `search_transcripts` instead."), None)
        })?;

        let date_expr = db::date_prefix_filter("m.created_at");
        let matches = semantic::search(&self.pool, &model_id, &vector, limit, |qb| {
            if let Some(v) = since {
                qb.push(format!(" AND {date_expr} >= ")).push_bind(v);
            }
            if let Some(v) = until {
                qb.push(format!(" AND {date_expr} <= ")).push_bind(v);
            }
            if let Some(p) = &person {
                extras::push_person_filter(qb, p);
            }
        })
        .await
        .map_err(|e| db_err("failed to search", e))?;

        Ok(Json(SemanticSearchResult {
            count: matches.len(),
            matches,
            index,
        }))
    }

    #[tool(
        name = "get_transcript",
        description = "Get a meeting's full transcript in recorded order, \
                       attributed to speakers with timestamps by default."
    )]
    pub async fn get_transcript(
        &self,
        Parameters(args): Parameters<GetTranscriptArgs>,
    ) -> Result<Json<TranscriptResult>, ErrorData> {
        let meeting = db::get_meeting(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to get meeting", e))?
            .ok_or_else(|| not_found("meeting", &args.meeting_id))?;

        let include_speakers = args.include_speakers.unwrap_or(true);
        let segments = db::get_transcript(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to get transcript", e))?;

        Ok(Json(TranscriptResult {
            meeting_id: meeting.id,
            title: meeting.title,
            segment_count: segments.len(),
            text: db::render_transcript(&segments, include_speakers),
            segments: include_speakers.then_some(segments),
        }))
    }

    #[tool(
        name = "get_meeting_stats",
        description = "Talk-time and speaking analytics for one meeting, \
                       computed from the transcript (no AI): each speaker's \
                       share of talk time, turns, words per minute, longest \
                       monologue, questions asked and interruptions, plus \
                       meeting duration, silence ratio and how much the user \
                       (microphone) spoke versus everyone else."
    )]
    pub async fn get_meeting_stats(
        &self,
        Parameters(args): Parameters<MeetingIdArgs>,
    ) -> Result<Json<MeetingStatsResult>, ErrorData> {
        let meeting = db::get_meeting(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to get meeting", e))?
            .ok_or_else(|| not_found("meeting", &args.meeting_id))?;
        let (segments, duration) = db::get_stats_segments(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to load transcript", e))?;
        let stats = crate::stats::compute(&segments, duration);
        Ok(Json(MeetingStatsResult {
            meeting_id: meeting.id,
            title: meeting.title,
            text: db::render_stats(&stats),
            stats,
        }))
    }

    #[tool(
        name = "search_transcripts",
        description = "Full-text keyword search over transcript segments across \
                       every meeting, ranked by relevance (best first). Every \
                       word must appear in a segment (any order); case- and \
                       accent-insensitive; the last word also matches as a \
                       prefix. Each hit has a snippet with matched words in \
                       **bold**. Keyword-based, not semantic: try a few \
                       distinctive words, and synonyms if nothing comes back."
    )]
    pub async fn search_transcripts(
        &self,
        Parameters(args): Parameters<SearchTranscriptsArgs>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let query = require_nonempty(&args.query, "query")?;
        let limit = db::clamp_limit(args.limit, 50);

        let matches = db::search_transcripts(&self.pool, &query, limit)
            .await
            .map_err(|e| db_err("failed to search transcripts", e))?;

        Ok(Json(SearchResult {
            count: matches.len(),
            truncated: matches.len() as u32 == limit,
            matches,
        }))
    }

    #[tool(
        name = "get_action_items",
        description = "List action items, optionally filtered to one meeting \
                       and/or a status (\"open\" or \"done\"). Open items come \
                       first."
    )]
    pub async fn get_action_items(
        &self,
        Parameters(args): Parameters<GetActionItemsArgs>,
    ) -> Result<Json<ActionItemsResult>, ErrorData> {
        let status = optional(args.status)
            .map(|s| db::validate_status(&s))
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let meeting_id = optional(args.meeting_id);

        if let Some(id) = &meeting_id {
            if !db::meeting_exists(&self.pool, id)
                .await
                .map_err(|e| db_err("failed to check meeting", e))?
            {
                return Err(not_found("meeting", id));
            }
        }

        let action_items = db::get_action_items(&self.pool, meeting_id.as_deref(), status.as_deref())
            .await
            .map_err(|e| db_err("failed to get action items", e))?;

        Ok(Json(ActionItemsResult {
            count: action_items.len(),
            action_items,
        }))
    }

    #[tool(
        name = "get_recent_summaries",
        description = "Get the most recently updated meeting summaries in \
                       markdown — a fast way to catch up on what's been \
                       discussed lately."
    )]
    pub async fn get_recent_summaries(
        &self,
        Parameters(args): Parameters<GetRecentSummariesArgs>,
    ) -> Result<Json<SummariesResult>, ErrorData> {
        let summaries = db::get_recent_summaries(&self.pool, db::clamp_limit(args.limit, 5))
            .await
            .map_err(|e| db_err("failed to get recent summaries", e))?;

        Ok(Json(SummariesResult {
            count: summaries.len(),
            summaries,
        }))
    }

    #[tool(
        name = "add_meeting_note",
        description = "Attach a note to a meeting. The note is recorded with \
                       source=\"agent\" and shows up alongside the user's own \
                       notes in Parley."
    )]
    pub async fn add_meeting_note(
        &self,
        Parameters(args): Parameters<AddMeetingNoteArgs>,
    ) -> Result<Json<db::MeetingNote>, ErrorData> {
        let body = require_nonempty(&args.body, "body")?;

        if !db::meeting_exists(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to check meeting", e))?
        {
            return Err(not_found("meeting", &args.meeting_id));
        }

        db::add_meeting_note(&self.pool, &args.meeting_id, &body)
            .await
            .map(Json)
            .map_err(|e| db_err("failed to add note", e))
    }

    #[tool(
        name = "create_action_item",
        description = "Create an open action item against a meeting, recorded \
                       with source=\"agent\"."
    )]
    pub async fn create_action_item(
        &self,
        Parameters(args): Parameters<CreateActionItemArgs>,
    ) -> Result<Json<db::ActionItem>, ErrorData> {
        let text = require_nonempty(&args.text, "text")?;

        if !db::meeting_exists(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to check meeting", e))?
        {
            return Err(not_found("meeting", &args.meeting_id));
        }

        let id = db::create_action_item(
            &self.pool,
            &args.meeting_id,
            &text,
            optional(args.assignee).as_deref(),
            optional(args.due_hint).as_deref(),
        )
        .await
        .map_err(|e| db_err("failed to create action item", e))?;

        db::get_action_item(&self.pool, &id)
            .await
            .map_err(|e| db_err("failed to read back action item", e))?
            .map(Json)
            .ok_or_else(|| {
                ErrorData::internal_error(
                    "action item was created but could not be read back".to_string(),
                    None,
                )
            })
    }

    #[tool(
        name = "set_action_item_status",
        description = "Mark an action item \"done\" or reopen it as \"open\". \
                       Completing sets its completion timestamp; reopening \
                       clears it."
    )]
    pub async fn set_action_item_status(
        &self,
        Parameters(args): Parameters<SetActionItemStatusArgs>,
    ) -> Result<Json<db::ActionItem>, ErrorData> {
        let status =
            db::validate_status(&args.status).map_err(|e| ErrorData::invalid_params(e, None))?;

        let updated = db::set_action_item_status(&self.pool, &args.action_item_id, &status)
            .await
            .map_err(|e| db_err("failed to update action item", e))?;
        if !updated {
            return Err(action_item_not_found(&args.action_item_id));
        }

        db::get_action_item(&self.pool, &args.action_item_id)
            .await
            .map_err(|e| db_err("failed to read back action item", e))?
            .map(Json)
            .ok_or_else(|| not_found("action item", &args.action_item_id))
    }

    #[tool(
        name = "update_action_item",
        description = "Edit an action item's text, owner (`assignee`) or due \
                       hint. Omitted fields are kept; an empty string clears \
                       the owner or due hint."
    )]
    pub async fn update_action_item(
        &self,
        Parameters(args): Parameters<UpdateActionItemArgs>,
    ) -> Result<Json<db::ActionItem>, ErrorData> {
        let text = args
            .text
            .as_deref()
            .map(|t| require_nonempty(t, "text"))
            .transpose()?;
        let assignee = extras::FieldEdit::from_arg(args.assignee);
        let due_hint = extras::FieldEdit::from_arg(args.due_hint);

        let updated = extras::update_action_item(
            &self.pool,
            &args.action_item_id,
            text.as_deref(),
            &assignee,
            &due_hint,
        )
        .await
        .map_err(|e| db_err("failed to update action item", e))?;
        if !updated {
            return Err(action_item_not_found(&args.action_item_id));
        }
        db::get_action_item(&self.pool, &args.action_item_id)
            .await
            .map_err(|e| db_err("failed to read back action item", e))?
            .map(Json)
            .ok_or_else(|| action_item_not_found(&args.action_item_id))
    }

    #[tool(
        name = "delete_action_item",
        description = "Permanently delete an action item. Prefer \
                       `set_action_item_status` with \"done\" for finished \
                       work; delete only items that are wrong or duplicated."
    )]
    pub async fn delete_action_item(
        &self,
        Parameters(args): Parameters<ActionItemIdArgs>,
    ) -> Result<Json<DeletedResult>, ErrorData> {
        let deleted = extras::delete_action_item(&self.pool, &args.action_item_id)
            .await
            .map_err(|e| db_err("failed to delete action item", e))?;
        if !deleted {
            return Err(action_item_not_found(&args.action_item_id));
        }
        Ok(Json(DeletedResult {
            deleted,
            id: args.action_item_id,
        }))
    }

    #[tool(
        name = "update_summary",
        description = "Replace a meeting's summary markdown. This overwrites \
                       the existing summary wholesale — read it with \
                       `get_meeting` first if you intend to revise it. The \
                       summary generation status is left untouched."
    )]
    pub async fn update_summary(
        &self,
        Parameters(args): Parameters<UpdateSummaryArgs>,
    ) -> Result<Json<UpdateSummaryResult>, ErrorData> {
        let markdown = require_nonempty(&args.markdown, "markdown")?;

        if !db::meeting_exists(&self.pool, &args.meeting_id)
            .await
            .map_err(|e| db_err("failed to check meeting", e))?
        {
            return Err(not_found("meeting", &args.meeting_id));
        }

        let write = db::update_summary(&self.pool, &args.meeting_id, &markdown)
            .await
            .map_err(|e| db_err("failed to update summary", e))?;

        if let Some(w) = &write.warning {
            tracing::warn!(meeting_id = %args.meeting_id, "{w}");
        }

        Ok(Json(UpdateSummaryResult {
            meeting_id: args.meeting_id,
            status: write.status,
            updated_at: write.updated_at,
            warning: write.warning,
        }))
    }
}

impl ParleyMcp {
    /// A meeting with its calendar event, chapters, bookmarks and notes, or
    /// a not-found error.
    async fn load_meeting(&self, meeting_id: &str) -> Result<MeetingResult, ErrorData> {
        let meeting = db::get_meeting(&self.pool, meeting_id)
            .await
            .map_err(|e| db_err("failed to get meeting", e))?
            .ok_or_else(|| not_found("meeting", meeting_id))?;
        let calendar_event = match &meeting.calendar_event_id {
            Some(id) => extras::get_calendar_event(&self.pool, id)
                .await
                .map_err(|e| db_err("failed to get calendar event", e))?,
            None => None,
        };
        let chapters = extras::get_chapters(&self.pool, meeting_id)
            .await
            .map_err(|e| db_err("failed to get chapters", e))?;
        let bookmarks = extras::get_bookmarks(&self.pool, meeting_id)
            .await
            .map_err(|e| db_err("failed to get bookmarks", e))?;
        let notes = extras::get_notes(&self.pool, meeting_id)
            .await
            .map_err(|e| db_err("failed to get notes", e))?;
        let has_followup_draft = extras::get_followup_draft(&self.pool, meeting_id)
            .await
            .map_err(|e| db_err("failed to get follow-up draft", e))?
            .is_some();
        Ok(MeetingResult {
            meeting,
            calendar_event,
            chapters,
            bookmarks,
            notes,
            has_followup_draft,
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ParleyMcp {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive], so it has to be built by mutating a
        // default rather than with a struct literal.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        info.instructions = Some(
            "Access to the user's local Parley meeting history: meetings, \
                 transcripts, AI-generated summaries, action items, notes, \
                 chapters and bookmarks, the people Parley recognises, the \
                 user's calendar, pre-meeting briefs and follow-up drafts.\n\n\
                 Typical flow: `list_meetings` (filter by date, title, `person` \
                 or `series_id`) or a search to find a meeting_id, then \
                 `get_meeting` for the summary, calendar event and chapters, or \
                 `get_transcript` for what was actually said. To prepare for a \
                 meeting: `list_calendar_events`, then `get_brief`, or \
                 `list_meetings` with its `series_id` or attendees.\n\n\
                 Notes: `search_transcripts` is ranked full-text keyword search \
                 (all words must match); `semantic_search` matches meaning but \
                 needs Ollama and covers only indexed meetings. All timestamps \
                 are UTC. Anything you create is tagged source=\"agent\" so the \
                 user can tell it apart from their own entries. \
                 `update_summary` overwrites the whole summary; read it first \
                 if revising.\n\n\
                 Each meeting is also readable as a resource at \
                 parley://meeting/{id}, returning summary plus transcript as \
                 markdown."
                .to_string(),
        );
        info
    }

    /// Expose recent meetings as readable resources.
    ///
    /// Unpaginated by design: this returns the most recent
    /// [`db::MAX_RESOURCES`] meetings and stops. The full corpus is reachable
    /// through `list_meetings`, which does take a limit and date filters, so
    /// paginating here would add protocol surface for no capability.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let meetings = db::list_meeting_refs(&self.pool, db::MAX_RESOURCES)
            .await
            .map_err(|e| db_err("failed to list meetings", e))?;

        let resources = meetings
            .into_iter()
            .map(|m| {
                Resource::new(db::meeting_resource_uri(&m.id), m.title.clone())
                    .with_title(m.title.clone())
                    .with_description(format!("Meeting \"{}\" from {}", m.title, m.created_at))
                    .with_mime_type("text/markdown")
            })
            .collect();

        Ok(ListResourcesResult::with_all_items(resources))
    }

    /// Read a meeting as markdown: metadata, summary, then full transcript.
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let meeting_id = db::parse_meeting_resource_uri(&request.uri).ok_or_else(|| {
            ErrorData::resource_not_found(
                format!(
                    "unknown resource uri {:?}. Meeting resources look like \
                     parley://meeting/{{meeting_id}}.",
                    request.uri
                ),
                None,
            )
        })?;

        if !db::meeting_exists(&self.pool, meeting_id)
            .await
            .map_err(|e| db_err("failed to check meeting", e))?
        {
            return Err(ErrorData::resource_not_found(
                format!("no meeting with id {meeting_id:?}"),
                None,
            ));
        }
        let meeting = self.load_meeting(meeting_id).await?;

        let segments = db::get_transcript(&self.pool, meeting_id)
            .await
            .map_err(|e| db_err("failed to get transcript", e))?;

        let markdown = render_meeting_markdown(&meeting, &segments);

        Ok(ReadResourceResult::new(vec![
            ResourceContents::TextResourceContents {
                uri: request.uri,
                mime_type: Some("text/markdown".to_string()),
                text: markdown,
                meta: None,
            },
        ]))
    }
}

/// Render a meeting as a self-contained markdown document.
pub fn render_meeting_markdown(full: &MeetingResult, segments: &[db::TranscriptSegment]) -> String {
    let meeting = &full.meeting;
    let mut out = format!("# {}\n\n", meeting.title);
    out.push_str(&format!("- Meeting ID: `{}`\n", meeting.id));
    out.push_str(&format!("- Recorded: {}\n", meeting.created_at));
    if let Some(event) = &full.calendar_event {
        out.push_str(&format!(
            "- Calendar event: {} ({} to {})\n",
            event.title.as_deref().unwrap_or("Untitled event"),
            event.start_at,
            event.end_at
        ));
        let people: Vec<String> = event
            .attendees
            .iter()
            .filter_map(|a| a.name.clone().or_else(|| a.email.clone()))
            .collect();
        if !people.is_empty() {
            out.push_str(&format!("- Attendees: {}\n", people.join(", ")));
        }
    }
    out.push_str(&format!(
        "- Open action items: {}\n\n",
        meeting.open_action_item_count
    ));

    out.push_str("## Summary\n\n");
    match &meeting.summary {
        Some(s) => {
            out.push_str(s.trim());
            out.push_str("\n\n");
        }
        None => out.push_str("_No summary has been generated for this meeting._\n\n"),
    }

    if !full.chapters.is_empty() {
        out.push_str("## Chapters\n\n");
        for c in &full.chapters {
            out.push_str(&format!("- [{}] {}\n", c.start.as_deref().unwrap_or("?"), c.title));
        }
        out.push('\n');
    }
    if !full.bookmarks.is_empty() {
        out.push_str("## Bookmarks\n\n");
        for b in &full.bookmarks {
            out.push_str(&format!(
                "- [{}] {}\n",
                b.start.as_deref().unwrap_or("?"),
                b.label.as_deref().unwrap_or("Bookmark")
            ));
        }
        out.push('\n');
    }
    if !full.notes.is_empty() {
        out.push_str("## Notes\n\n");
        for n in &full.notes {
            out.push_str(&format!("- {}\n", n.body.trim()));
        }
        out.push('\n');
    }

    out.push_str("## Transcript\n\n");
    if segments.is_empty() {
        out.push_str("_No transcript was recorded for this meeting._\n");
    } else {
        out.push_str(&db::render_transcript(segments, true));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(summary: Option<&str>) -> MeetingResult {
        MeetingResult {
            meeting: meeting(summary),
            calendar_event: None,
            chapters: vec![],
            bookmarks: vec![],
            notes: vec![],
            has_followup_draft: false,
        }
    }

    fn meeting(summary: Option<&str>) -> db::MeetingDetail {
        db::MeetingDetail {
            id: "m1".into(),
            title: "Team Standup".into(),
            created_at: "2026-07-15T09:00:00+00:00".into(),
            updated_at: "2026-07-15T09:30:00+00:00".into(),
            folder_path: None,
            calendar_event_id: None,
            summary: summary.map(Into::into),
            summary_status: Some("completed".into()),
            transcript_segment_count: 1,
            open_action_item_count: 2,
            done_action_item_count: 0,
            note_count: 0,
        }
    }

    fn segment(text: &str) -> db::TranscriptSegment {
        db::TranscriptSegment {
            text: text.into(),
            speaker_label: "You (microphone)".into(),
            speaker: Some("mic".into()),
            start: Some("00:00:00".into()),
            audio_start_time: Some(0.0),
            audio_end_time: None,
        }
    }

    #[test]
    fn require_nonempty_rejects_blank_input() {
        assert_eq!(require_nonempty(" hi ", "body").unwrap(), "hi");
        assert!(require_nonempty("   ", "body").is_err());
        assert!(require_nonempty("", "body").is_err());
    }

    #[test]
    fn optional_collapses_blank_to_none() {
        assert_eq!(optional(Some("  x ".into())), Some("x".into()));
        assert_eq!(optional(Some("   ".into())), None);
        assert_eq!(optional(None), None);
    }

    #[test]
    fn render_meeting_markdown_includes_summary_and_transcript() {
        let md = render_meeting_markdown(&detail(Some("## Decisions\n- Ship it")), &[segment("Hello.")]);
        assert!(md.starts_with("# Team Standup"), "{md}");
        assert!(md.contains("- Ship it"), "{md}");
        assert!(md.contains("[00:00:00] You (microphone): Hello."), "{md}");
    }

    #[test]
    fn render_meeting_markdown_says_so_when_empty() {
        let md = render_meeting_markdown(&detail(None), &[]);
        assert!(md.contains("_No summary has been generated"), "{md}");
        assert!(md.contains("_No transcript was recorded"), "{md}");
    }

    #[test]
    fn tool_router_exposes_every_tool() {
        // The router is macro-generated; this pins the public tool surface so a
        // dropped #[tool] attribute fails the build rather than silently
        // shrinking the API.
        let router = ParleyMcp::tool_router();
        let mut names: Vec<_> = router.list_all().into_iter().map(|t| t.name).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "add_meeting_note",
                "create_action_item",
                "delete_action_item",
                "get_action_items",
                "get_brief",
                "get_followup_draft",
                "get_meeting",
                "get_meeting_stats",
                "get_recent_summaries",
                "get_transcript",
                "list_calendar_events",
                "list_meetings",
                "list_speakers",
                "search_transcripts",
                "semantic_search",
                "set_action_item_status",
                "update_action_item",
                "update_summary",
            ]
        );
    }

    #[test]
    fn write_tools_declare_their_required_args() {
        let router = ParleyMcp::tool_router();
        let tools = router.list_all();
        let create = tools
            .iter()
            .find(|t| t.name == "create_action_item")
            .expect("tool is registered");

        let required = create.input_schema.get("required").expect("has required");
        let required: Vec<&str> = required
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"meeting_id"), "{required:?}");
        assert!(required.contains(&"text"), "{required:?}");
        assert!(
            !required.contains(&"assignee"),
            "assignee is optional: {required:?}"
        );
    }
}
