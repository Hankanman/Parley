//! Retrieval for "Ask Parley across meetings": which transcript passages a
//! question gets to see.
//!
//! Two candidate sources are fused with reciprocal-rank fusion (RRF):
//!
//! 1. **Keywords (FTS5).** The question's content words (stopwords dropped)
//!    are expanded into an OR of prefix terms on a crude stem
//!    (`"migrat"*` for "migration") — the index is unstemmed, so the prefix
//!    is what finds "migrating"/"migrated" — and ranked by bm25 over
//!    `transcripts_fts`, which naturally favours segments matching more of
//!    the terms.
//! 2. **Meaning (embeddings),** when available: the best-matching embedded
//!    transcript chunks (see [`super::embeddings`]), passed in by the caller.
//!
//! Each hit becomes a *passage* — the hit segment plus a little context
//! (one segment before, a few after) — overlapping passages of a meeting are
//! merged (their scores add up), and the best passages are kept within a
//! token budget and per-meeting / per-answer caps. Small local models
//! struggle to synthesise many sources, so the caps are deliberately tight.
//!
//! Filters (person, date range, calendar series) restrict which meetings are
//! searched; a person filter additionally drops passages in which that
//! person doesn't speak. When nothing matches (a vague "what happened
//! lately?"), the most recent eligible meetings are sampled instead.

use crate::chat::context::{keywords, segment_line, ContextSegment};
use crate::summary::processor::rough_token_count;
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::collections::{HashMap, HashSet};

use super::prompt::{MeetingExcerpts, MEETING_SCAFFOLD_TOKENS};

/// Target size of one passage, in characters.
pub const PASSAGE_CHARS: usize = 700;
/// Segments of lead-in kept before a hit (usually the question it answers).
const LEAD_IN_SEGMENTS: usize = 1;
/// A passage never grows past this many segments after its hit.
const MAX_FOLLOW_SEGMENTS: usize = 6;
/// A merged passage stops absorbing neighbours beyond this many segments.
const MAX_MERGED_SEGMENTS: usize = 16;
/// FTS hits considered.
const FTS_LIMIT: i64 = 150;
/// Distinct meetings whose transcripts are loaded to build passages.
const MAX_CANDIDATE_MEETINGS: usize = 30;
/// Reciprocal-rank-fusion constant (the usual 60).
const RRF_K: f64 = 60.0;
/// Most recent meetings sampled when nothing matches.
const FALLBACK_MEETINGS: i64 = 4;
/// Upper bound on query terms taken from a question.
const MAX_QUERY_TERMS: usize = 12;

/// Caps on what goes into one answer's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionLimits {
    pub max_passages: usize,
    pub max_per_meeting: usize,
    pub max_meetings: usize,
}

impl Default for SelectionLimits {
    fn default() -> Self {
        Self {
            max_passages: 10,
            max_per_meeting: 3,
            max_meetings: 6,
        }
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Restricts which meetings (and, for a person, which passages) are searched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AskFilters {
    pub person: Option<PersonFilter>,
    /// Meetings created at or after this instant.
    pub from: Option<DateTime<Utc>>,
    /// Meetings created before this instant.
    pub to: Option<DateTime<Utc>>,
    pub series: Option<SeriesFilter>,
}

/// A speaker, by voice profile (and its name, which older or manually
/// labelled segments carry in `transcripts.speaker` without the link).
#[derive(Debug, Clone, PartialEq)]
pub struct PersonFilter {
    pub voice_profile_id: Option<String>,
    pub name: String,
}

/// A recurring calendar event: every occurrence shares its ICS UID.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesFilter {
    pub source_id: String,
    pub ics_uid: String,
    pub title: String,
}

impl PersonFilter {
    pub fn matches(&self, segment: &RetrievedSegment) -> bool {
        if let (Some(want), Some(have)) = (&self.voice_profile_id, &segment.voice_profile_id) {
            if want == have {
                return true;
            }
        }
        let name = self.name.trim();
        !name.is_empty() && segment.speaker.trim().eq_ignore_ascii_case(name)
    }
}

impl AskFilters {
    pub fn is_empty(&self) -> bool {
        self.person.is_none() && self.from.is_none() && self.to.is_none() && self.series.is_none()
    }

    /// Human description for the prompt ("Kingfisher; since 1 Aug 2026").
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(p) = &self.person {
            parts.push(format!("passages where {} speaks", p.name.trim()));
        }
        if let Some(s) = &self.series {
            parts.push(format!("the meeting series \"{}\"", s.title.trim().replace('"', "'")));
        }
        let day = |d: &DateTime<Utc>| super::prompt::format_meeting_date(d);
        match (&self.from, &self.to) {
            (Some(f), Some(t)) => parts.push(format!("meetings from {} to before {}", day(f), day(t))),
            (Some(f), None) => parts.push(format!("meetings since {}", day(f))),
            (None, Some(t)) => parts.push(format!("meetings before {}", day(t))),
            (None, None) => {}
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Append `AND …` conditions for `filters` to a query over `meetings m`.
pub fn push_meeting_filters(qb: &mut QueryBuilder<'_, Sqlite>, filters: &AskFilters) {
    if let Some(from) = &filters.from {
        qb.push(" AND julianday(m.created_at) >= julianday(")
            .push_bind(from.to_rfc3339())
            .push(")");
    }
    if let Some(to) = &filters.to {
        qb.push(" AND julianday(m.created_at) < julianday(")
            .push_bind(to.to_rfc3339())
            .push(")");
    }
    if let Some(series) = &filters.series {
        qb.push(" AND m.calendar_event_id IN (SELECT ce.id FROM calendar_events ce WHERE ce.source_id = ")
            .push_bind(series.source_id.clone())
            .push(" AND ce.ics_uid = ")
            .push_bind(series.ics_uid.clone())
            .push(")");
    }
    if let Some(person) = &filters.person {
        qb.push(" AND EXISTS (SELECT 1 FROM transcripts p WHERE p.meeting_id = m.id AND (p.voice_profile_id = ")
            .push_bind(person.voice_profile_id.clone().unwrap_or_default())
            .push(" OR LOWER(TRIM(p.speaker)) = LOWER(")
            .push_bind(person.name.trim().to_string())
            .push(")))");
    }
}

/// A person the user can filter by.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonOption {
    pub voice_profile_id: String,
    pub name: String,
}

/// Voice profiles, by name.
pub async fn list_people(pool: &SqlitePool) -> Result<Vec<PersonOption>, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, name FROM voice_profiles WHERE TRIM(name) != '' ORDER BY name COLLATE NOCASE")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(voice_profile_id, name)| PersonOption { voice_profile_id, name })
        .collect())
}

/// A recurring calendar event with recorded meetings.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesOption {
    pub filter: SeriesFilter,
    pub meeting_count: i64,
}

/// Calendar series that have recorded meetings: events that recur, or whose
/// UID is linked to at least two meetings. Most-recorded first.
pub async fn list_series(pool: &SqlitePool) -> Result<Vec<SeriesOption>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT ce.source_id, ce.ics_uid,
                COALESCE(MAX(ce.summary), 'Untitled event') AS title,
                COUNT(DISTINCT m.id) AS n,
                MAX(CASE WHEN ce.recurrence_id IS NOT NULL THEN 1 ELSE 0 END) AS recurring
         FROM meetings m
         JOIN calendar_events ce ON ce.id = m.calendar_event_id
         GROUP BY ce.source_id, ce.ics_uid
         HAVING n >= 2 OR recurring = 1
         ORDER BY n DESC, title COLLATE NOCASE",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(SeriesOption {
                filter: SeriesFilter {
                    source_id: r.try_get("source_id")?,
                    ics_uid: r.try_get("ics_uid")?,
                    title: r.try_get("title")?,
                },
                meeting_count: r.try_get("n")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// One transcript segment as retrieval sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievedSegment {
    pub id: String,
    pub start: Option<f64>,
    pub speaker: String,
    pub voice_profile_id: Option<String>,
    pub text: String,
}

impl RetrievedSegment {
    pub fn to_context(&self) -> ContextSegment {
        ContextSegment {
            start: self.start,
            speaker: self.speaker.clone(),
            text: self.text.trim().to_string(),
        }
    }
}

/// A meeting's non-blank segments in transcript display order (the same
/// order single-meeting chat and the transcript panel use).
pub async fn load_segments(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Vec<RetrievedSegment>, sqlx::Error> {
    let rows: Vec<(String, Option<f64>, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT id, audio_start_time, speaker, voice_profile_id, transcript
         FROM transcripts
         WHERE meeting_id = ? AND TRIM(transcript) != ''
         ORDER BY COALESCE(audio_start_time, 0.0), sequence_id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, start, speaker, voice_profile_id, text)| RetrievedSegment {
            id,
            start,
            speaker: speaker
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Speaker".to_string()),
            voice_profile_id,
            text: text.trim().to_string(),
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct MeetingMeta {
    pub id: String,
    pub title: String,
    pub created_at: Option<DateTime<Utc>>,
}

async fn load_meeting_meta(pool: &SqlitePool, id: &str) -> Result<Option<MeetingMeta>, sqlx::Error> {
    // strftime normalises both stored formats ('T' or ' ' separator, any
    // offset) to UTC.
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, title, strftime('%Y-%m-%dT%H:%M:%SZ', created_at) FROM meetings WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, title, created)| MeetingMeta {
        id,
        title,
        created_at: created
            .and_then(|c| DateTime::parse_from_rfc3339(&c).ok())
            .map(|d| d.with_timezone(&Utc)),
    }))
}

// ---------------------------------------------------------------------------
// Keyword stage
// ---------------------------------------------------------------------------

/// Crude suffix stripping, so the prefix query on the stem also finds other
/// forms of the word ("migration" → "migrat" → migrating, migrated).
fn crude_stem(word: &str) -> &str {
    const SUFFIXES: &[&str] = &[
        "ings", "ing", "ions", "ion", "ers", "ies", "ied", "ed", "es", "er", "ly",
    ];
    for suffix in SUFFIXES {
        if let Some(stem) = word.strip_suffix(suffix) {
            if stem.chars().count() >= 4 {
                return stem;
            }
        }
    }
    word
}

/// The FTS5 `MATCH` expression for a question: its content words as an OR
/// of quoted terms, prefix-matched on a crude stem when the word is long
/// enough. `None` when the question has no content words. Words in `exclude`
/// (e.g. the filtered person's name, already enforced by the filter) are
/// left out.
pub fn expand_query(text: &str, exclude: &[String]) -> Option<String> {
    let exclude: HashSet<String> = exclude.iter().flat_map(|e| keywords(e)).collect();
    let mut seen = HashSet::new();
    let terms: Vec<String> = keywords(text)
        .into_iter()
        .filter(|w| !exclude.contains(w))
        .filter(|w| seen.insert(w.clone()))
        .take(MAX_QUERY_TERMS)
        .map(|w| {
            let quoted = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
            if w.chars().count() >= 4 {
                quoted(crude_stem(&w)) + "*"
            } else {
                quoted(&w)
            }
        })
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentHit {
    pub transcript_id: String,
    pub meeting_id: String,
}

/// Best-first FTS5 hits for `expression` in meetings passing `filters`.
pub async fn fts_candidates(
    pool: &SqlitePool,
    expression: &str,
    filters: &AskFilters,
    limit: i64,
) -> Result<Vec<SegmentHit>, sqlx::Error> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT t.id, t.meeting_id FROM transcripts_fts
         JOIN transcripts t ON t.rowid = transcripts_fts.rowid
         JOIN meetings m ON m.id = t.meeting_id
         WHERE transcripts_fts MATCH ",
    );
    qb.push_bind(expression.to_string());
    push_meeting_filters(&mut qb, filters);
    qb.push(" ORDER BY transcripts_fts.rank LIMIT ").push_bind(limit);
    match qb.build_query_as::<(String, String)>().fetch_all(pool).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|(transcript_id, meeting_id)| SegmentHit {
                transcript_id,
                meeting_id,
            })
            .collect()),
        Err(sqlx::Error::Database(db)) if db.message().contains("fts5") || db.message().contains("MATCH") => {
            log::warn!("FTS5 rejected ask expression {expression:?}: {}", db.message());
            Ok(Vec::new())
        }
        Err(e) => Err(e),
    }
}

/// Most recent meetings passing `filters`.
async fn recent_meetings(
    pool: &SqlitePool,
    filters: &AskFilters,
    limit: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT m.id FROM meetings m WHERE 1 = 1");
    push_meeting_filters(&mut qb, filters);
    qb.push(" ORDER BY julianday(m.created_at) DESC LIMIT ").push_bind(limit);
    let rows: Vec<(String,)> = qb.build_query_as().fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

// ---------------------------------------------------------------------------
// Passages: pure
// ---------------------------------------------------------------------------

/// A candidate excerpt: segments `lo..=hi` of a meeting.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub meeting_id: String,
    pub lo: usize,
    pub hi: usize,
    pub score: f64,
}

fn segment_chars(s: &RetrievedSegment) -> usize {
    s.text.len() + s.speaker.len() + 10
}

/// Segment range of the passage around hit `ix`: one segment of lead-in,
/// then following segments until about `target_chars`.
pub fn passage_around(segments: &[RetrievedSegment], ix: usize, target_chars: usize) -> (usize, usize) {
    if segments.is_empty() {
        return (0, 0);
    }
    let ix = ix.min(segments.len() - 1);
    let (mut lo, mut hi) = (ix, ix);
    let mut total = segment_chars(&segments[ix]);
    while total < target_chars {
        if ix - lo < LEAD_IN_SEGMENTS && lo > 0 {
            lo -= 1;
            total += segment_chars(&segments[lo]);
        } else if hi + 1 < segments.len() && hi - ix < MAX_FOLLOW_SEGMENTS {
            hi += 1;
            total += segment_chars(&segments[hi]);
        } else if lo > 0 && ix - lo < LEAD_IN_SEGMENTS + 2 {
            // Hit near the end of the meeting: take more context before.
            lo -= 1;
            total += segment_chars(&segments[lo]);
        } else {
            break;
        }
    }
    (lo, hi)
}

/// RRF contribution of a 0-based rank.
pub fn rrf(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64 + 1.0)
}

/// Merge overlapping passages of the same meeting (scores add up — several
/// hits close together is a strong signal), unless the merge would make an
/// over-long passage.
pub fn merge_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut by_meeting: HashMap<String, Vec<Candidate>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for c in candidates {
        if !by_meeting.contains_key(&c.meeting_id) {
            order.push(c.meeting_id.clone());
        }
        by_meeting.entry(c.meeting_id.clone()).or_default().push(c);
    }
    let mut out = Vec::new();
    for meeting in order {
        let mut list = by_meeting.remove(&meeting).unwrap_or_default();
        list.sort_by_key(|c| (c.lo, c.hi));
        let mut merged: Vec<Candidate> = Vec::new();
        for c in list {
            let Some(last) = merged.last_mut() else {
                merged.push(c);
                continue;
            };
            if c.lo > last.hi {
                merged.push(c);
            } else if last.hi.max(c.hi) - last.lo < MAX_MERGED_SEGMENTS {
                last.hi = last.hi.max(c.hi);
                last.score += c.score;
            } else if c.hi > last.hi {
                // Too long to merge: keep the new one, minus the overlap.
                let lo = last.hi + 1;
                merged.push(Candidate { lo, ..c });
            } else {
                last.score += c.score;
            }
        }
        out.extend(merged);
    }
    out
}

/// Pick the best candidates that fit `budget_tokens` and `limits`. `cost`
/// is a candidate's prompt cost in tokens. Returned best-first.
pub fn select_passages(
    mut candidates: Vec<Candidate>,
    cost: impl Fn(&Candidate) -> usize,
    budget_tokens: usize,
    limits: SelectionLimits,
) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.meeting_id.cmp(&b.meeting_id))
            .then(a.lo.cmp(&b.lo))
    });
    let mut used = 0usize;
    let mut per_meeting: HashMap<String, usize> = HashMap::new();
    let mut picked = Vec::new();
    for c in candidates {
        if picked.len() >= limits.max_passages {
            break;
        }
        let count = per_meeting.get(&c.meeting_id).copied().unwrap_or(0);
        if count >= limits.max_per_meeting {
            continue;
        }
        let new_meeting = count == 0;
        if new_meeting && per_meeting.len() >= limits.max_meetings {
            continue;
        }
        let c_cost = cost(&c) + if new_meeting { MEETING_SCAFFOLD_TOKENS } else { 2 };
        if used + c_cost > budget_tokens {
            continue;
        }
        used += c_cost;
        *per_meeting.entry(c.meeting_id.clone()).or_insert(0) += 1;
        picked.push(c);
    }
    picked
}

/// Indices spread evenly over `0..n` (up to `count` of them).
fn spread(n: usize, count: usize) -> Vec<usize> {
    if n == 0 || count == 0 {
        return Vec::new();
    }
    let count = count.min(n);
    (0..count).map(|k| (2 * k + 1) * n / (2 * count)).collect()
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// A semantic-stage hit: an embedded chunk, best first.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkHit {
    pub meeting_id: String,
    pub first_transcript_id: String,
    pub segment_count: usize,
    pub similarity: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetrievalStats {
    pub keyword_hits: usize,
    pub semantic_hits: usize,
    /// Nothing matched; recent meetings were sampled instead.
    pub fallback: bool,
    /// Distinct meetings considered.
    pub meetings_considered: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Retrieved {
    /// Chosen excerpts, meetings in chronological order.
    pub meetings: Vec<MeetingExcerpts>,
    pub stats: RetrievalStats,
}

/// Retrieve the passages for `query_text` (the question, plus the previous
/// question for follow-ups) within `budget_tokens`.
pub async fn retrieve(
    pool: &SqlitePool,
    query_text: &str,
    filters: &AskFilters,
    semantic_hits: &[ChunkHit],
    budget_tokens: usize,
    limits: SelectionLimits,
) -> Result<Retrieved, sqlx::Error> {
    let mut stats = RetrievalStats::default();
    let exclude: Vec<String> = filters.person.iter().map(|p| p.name.clone()).collect();

    let fts_hits = match expand_query(query_text, &exclude) {
        Some(expr) => fts_candidates(pool, &expr, filters, FTS_LIMIT).await?,
        None => Vec::new(),
    };
    stats.keyword_hits = fts_hits.len();
    stats.semantic_hits = semantic_hits.len();

    // Meetings to load, in order of their best hit (keyword hits first at
    // equal rank — they're exact).
    let mut meeting_order: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let max_len = fts_hits.len().max(semantic_hits.len());
    for i in 0..max_len {
        for id in [fts_hits.get(i).map(|h| &h.meeting_id), semantic_hits.get(i).map(|h| &h.meeting_id)]
            .into_iter()
            .flatten()
        {
            if seen.insert(id.clone()) {
                meeting_order.push(id.clone());
            }
        }
    }
    meeting_order.truncate(MAX_CANDIDATE_MEETINGS);

    let mut segments: HashMap<String, Vec<RetrievedSegment>> = HashMap::new();
    for id in &meeting_order {
        segments.insert(id.clone(), load_segments(pool, id).await?);
    }

    let mut candidates = Vec::new();
    for (rank, hit) in fts_hits.iter().enumerate() {
        let Some(segs) = segments.get(&hit.meeting_id) else { continue };
        let Some(ix) = segs.iter().position(|s| s.id == hit.transcript_id) else { continue };
        let (lo, hi) = passage_around(segs, ix, PASSAGE_CHARS);
        candidates.push(Candidate { meeting_id: hit.meeting_id.clone(), lo, hi, score: rrf(rank) });
    }
    for (rank, hit) in semantic_hits.iter().enumerate() {
        let Some(segs) = segments.get(&hit.meeting_id) else { continue };
        let Some(lo) = segs.iter().position(|s| s.id == hit.first_transcript_id) else { continue };
        let hi = (lo + hit.segment_count.max(1) - 1).min(segs.len() - 1);
        candidates.push(Candidate { meeting_id: hit.meeting_id.clone(), lo, hi, score: rrf(rank) });
    }

    let person_ok = |c: &Candidate, segs: &HashMap<String, Vec<RetrievedSegment>>| match &filters.person {
        None => true,
        Some(p) => segs
            .get(&c.meeting_id)
            .is_some_and(|s| s[c.lo..=c.hi].iter().any(|seg| p.matches(seg))),
    };
    candidates.retain(|c| person_ok(c, &segments));

    if candidates.is_empty() {
        stats.fallback = true;
        let recent = recent_meetings(pool, filters, FALLBACK_MEETINGS).await?;
        for (mrank, id) in recent.iter().enumerate() {
            if !segments.contains_key(id) {
                segments.insert(id.clone(), load_segments(pool, id).await?);
            }
            let segs = &segments[id];
            let anchors: Vec<usize> = match &filters.person {
                Some(p) => {
                    let theirs: Vec<usize> =
                        (0..segs.len()).filter(|&i| p.matches(&segs[i])).collect();
                    spread(theirs.len(), 2).into_iter().map(|k| theirs[k]).collect()
                }
                None => spread(segs.len(), 2),
            };
            for (k, ix) in anchors.into_iter().enumerate() {
                let (lo, hi) = passage_around(segs, ix, PASSAGE_CHARS);
                candidates.push(Candidate { meeting_id: id.clone(), lo, hi, score: rrf(mrank * 2 + k) });
            }
        }
    }
    stats.meetings_considered = candidates.iter().map(|c| &c.meeting_id).collect::<HashSet<_>>().len();

    let merged = merge_candidates(candidates);
    let cost = |c: &Candidate| {
        segments.get(&c.meeting_id).map_or(0, |s| {
            s[c.lo..=c.hi]
                .iter()
                .map(|seg| rough_token_count(&segment_line(&seg.to_context())) + 1)
                .sum()
        })
    };
    let picked = select_passages(merged, cost, budget_tokens, limits);

    // Group by meeting; meetings chronological, passages in transcript order.
    let mut grouped: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    for c in &picked {
        grouped.entry(c.meeting_id.clone()).or_default().push((c.lo, c.hi));
    }
    let mut meetings = Vec::new();
    for (id, mut ranges) in grouped {
        let Some(meta) = load_meeting_meta(pool, &id).await? else { continue };
        ranges.sort_unstable();
        let segs = &segments[&id];
        meetings.push(MeetingExcerpts {
            meeting_id: id.clone(),
            title: meta.title,
            created_at: meta.created_at,
            passages: ranges
                .into_iter()
                .map(|(lo, hi)| segs[lo..=hi].iter().map(RetrievedSegment::to_context).collect())
                .collect(),
        });
    }
    meetings.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.meeting_id.cmp(&b.meeting_id)));

    Ok(Retrieved { meetings, stats })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    pub(crate) async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn meeting(pool: &SqlitePool, id: &str, title: &str, created: &str) {
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(title)
            .bind(created)
            .bind(created)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn segment(pool: &SqlitePool, meeting_id: &str, seq: i64, speaker: &str, text: &str) {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, speaker, sequence_id)
             VALUES (?, ?, ?, 'x', ?, ?, ?)",
        )
        .bind(format!("{meeting_id}-{seq}"))
        .bind(meeting_id)
        .bind(text)
        .bind(seq as f64 * 10.0)
        .bind(speaker)
        .bind(seq)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed(pool: &SqlitePool) {
        meeting(pool, "aug", "Platform sync", "2026-08-14T10:00:00+00:00").await;
        meeting(pool, "sep", "Platform sync", "2026-09-11 10:00:00.000+00:00").await;
        meeting(pool, "jul", "Offsite", "2026-07-01T10:00:00Z").await;
        for (id, lines) in [
            ("aug", vec![
                ("Alice", "Morning everyone."),
                ("Alice", "Kingfisher, where are we on the database migration?"),
                ("Kingfisher", "We start migrating the orders table next week."),
                ("Bob", "Lunch is at noon."),
            ]),
            ("sep", vec![
                ("Bob", "Quick status round."),
                ("Kingfisher", "The migration finished; rollback plan was not needed."),
                ("Alice", "Great."),
            ]),
            ("jul", vec![
                ("Bob", "We should plan the migration eventually."),
                ("Alice", "Budget review is due."),
            ]),
        ] {
            for (seq, (speaker, text)) in lines.into_iter().enumerate() {
                segment(pool, id, seq as i64, speaker, text).await;
            }
        }
    }

    #[test]
    fn query_expansion_stems_prefixes_and_drops_stopwords() {
        assert_eq!(
            expand_query("What did Kingfisher say about the migrations?", &[]).as_deref(),
            Some("\"kingfish\"* OR \"migrat\"*")
        );
        assert_eq!(
            expand_query("What did Kingfisher say about the migrations?", &["Kingfisher".into()]).as_deref(),
            Some("\"migrat\"*")
        );
        assert_eq!(expand_query("API cost", &[]).as_deref(), Some("\"api\" OR \"cost\"*"));
        assert_eq!(expand_query("what is it?", &[]), None);
        // Repeated words collapse.
        assert_eq!(expand_query("budget budget budgets", &[]).as_deref(), Some("\"budget\"*"));
    }

    #[tokio::test]
    async fn expanded_queries_are_valid_fts5() {
        let pool = test_pool().await;
        seed(&pool).await;
        for q in ["\"quoted\" NEAR(x) AND", "migration's \"rollback\"", "日本語 テスト", "o'brien c++ x*y"] {
            if let Some(expr) = expand_query(q, &[]) {
                fts_candidates(&pool, &expr, &AskFilters::default(), 10)
                    .await
                    .unwrap_or_else(|e| panic!("{q:?} -> {expr:?}: {e}"));
            }
        }
    }

    #[tokio::test]
    async fn keyword_stage_finds_other_word_forms_and_ranks_multi_term_hits_first() {
        let pool = test_pool().await;
        seed(&pool).await;
        let expr = expand_query("migration rollback", &[]).unwrap();
        let hits = fts_candidates(&pool, &expr, &AskFilters::default(), 50).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.transcript_id.as_str()).collect();
        // "migration" + "rollback" beats single-term hits; "migrating" found via the stem prefix.
        assert_eq!(ids[0], "sep-1");
        assert!(ids.contains(&"aug-2"), "{ids:?}");
        assert!(ids.contains(&"jul-0"), "{ids:?}");
    }

    #[tokio::test]
    async fn filters_restrict_meetings() {
        let pool = test_pool().await;
        seed(&pool).await;
        let expr = expand_query("migration", &[]).unwrap();
        let meetings = |filters: AskFilters| {
            let pool = pool.clone();
            let expr = expr.clone();
            async move {
                let mut ids: Vec<String> = fts_candidates(&pool, &expr, &filters, 50)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|h| h.meeting_id)
                    .collect();
                ids.sort();
                ids.dedup();
                ids
            }
        };
        let utc = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);

        assert_eq!(meetings(AskFilters::default()).await, vec!["aug", "jul", "sep"]);
        // Date range, across both stored timestamp formats.
        let since_aug = AskFilters { from: Some(utc("2026-08-01T00:00:00Z")), ..Default::default() };
        assert_eq!(meetings(since_aug).await, vec!["aug", "sep"]);
        let before_sep = AskFilters { to: Some(utc("2026-09-11T09:00:00Z")), ..Default::default() };
        assert_eq!(meetings(before_sep).await, vec!["aug", "jul"]);
        // Person: only meetings where they speak (by name; profile link below).
        let kingfisher = AskFilters {
            person: Some(PersonFilter { voice_profile_id: Some("vp-k".into()), name: "kingfisher".into() }),
            ..Default::default()
        };
        assert_eq!(meetings(kingfisher.clone()).await, vec!["aug", "sep"]);
        // Series: meetings linked to occurrences of one recurring event.
        sqlx::query("INSERT INTO calendar_sources (id, url, created_at, updated_at) VALUES ('src', 'u', 'x', 'x')")
            .execute(&pool)
            .await
            .unwrap();
        for (ev, rec, mid) in [("e1", "2026-08-14", "aug"), ("e2", "2026-09-11", "sep")] {
            sqlx::query(
                "INSERT INTO calendar_events (id, source_id, ics_uid, recurrence_id, summary, start_at, end_at, fetched_at)
                 VALUES (?, 'src', 'uid-sync', ?, 'Platform sync', 'x', 'x', 'x')",
            )
            .bind(ev)
            .bind(rec)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("UPDATE meetings SET calendar_event_id = ? WHERE id = ?")
                .bind(ev)
                .bind(mid)
                .execute(&pool)
                .await
                .unwrap();
        }
        let series = list_series(&pool).await.unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].meeting_count, 2);
        assert_eq!(series[0].filter.title, "Platform sync");
        let in_series = AskFilters { series: Some(series[0].filter.clone()), ..Default::default() };
        assert_eq!(meetings(in_series).await, vec!["aug", "sep"]);
        // Combined: series + date.
        let combined = AskFilters {
            series: Some(series[0].filter.clone()),
            from: Some(utc("2026-09-01T00:00:00Z")),
            ..Default::default()
        };
        assert_eq!(meetings(combined).await, vec!["sep"]);
    }

    #[tokio::test]
    async fn retrieve_groups_by_meeting_chronologically_with_person_filter() {
        let pool = test_pool().await;
        seed(&pool).await;
        // Link Kingfisher's August line to a voice profile, rename its label:
        // the profile id alone must still match.
        sqlx::query("UPDATE transcripts SET voice_profile_id = 'vp-k', speaker = 'Speaker 2' WHERE id = 'aug-2'")
            .execute(&pool)
            .await
            .unwrap();
        let filters = AskFilters {
            person: Some(PersonFilter { voice_profile_id: Some("vp-k".into()), name: "Kingfisher".into() }),
            ..Default::default()
        };
        let r = retrieve(&pool, "What has Kingfisher said about the migration?", &filters, &[], 2_000, SelectionLimits::default())
            .await
            .unwrap();
        assert!(!r.stats.fallback);
        let ids: Vec<&str> = r.meetings.iter().map(|m| m.meeting_id.as_str()).collect();
        // July has "migration" but Kingfisher isn't in it.
        assert_eq!(ids, vec!["aug", "sep"]);
        let aug = &r.meetings[0];
        assert_eq!(aug.title, "Platform sync");
        assert!(aug.created_at.is_some());
        let lines: Vec<&str> = aug.passages[0].iter().map(|s| s.text.as_str()).collect();
        assert!(lines.contains(&"We start migrating the orders table next week."), "{lines:?}");
        // The lead-in question is kept for context.
        assert!(lines.contains(&"Kingfisher, where are we on the database migration?"));
    }

    #[tokio::test]
    async fn retrieve_uses_semantic_hits_and_falls_back_to_recent_meetings() {
        let pool = test_pool().await;
        seed(&pool).await;
        // No keyword overlap at all, but a semantic hit on the July budget line.
        let semantic = vec![ChunkHit {
            meeting_id: "jul".into(),
            first_transcript_id: "jul-1".into(),
            segment_count: 1,
            similarity: 0.8,
        }];
        let r = retrieve(&pool, "finances", &AskFilters::default(), &semantic, 2_000, SelectionLimits::default())
            .await
            .unwrap();
        assert_eq!(r.stats.semantic_hits, 1);
        assert_eq!(r.meetings.len(), 1);
        assert!(r.meetings[0].passages[0].iter().any(|s| s.text == "Budget review is due."));

        // Nothing at all: recent meetings are sampled, newest considered first.
        let r = retrieve(&pool, "what happened?", &AskFilters::default(), &[], 2_000, SelectionLimits::default())
            .await
            .unwrap();
        assert!(r.stats.fallback);
        assert!(!r.meetings.is_empty());
        // Chronological output order.
        assert!(r.meetings.windows(2).all(|w| w[0].created_at <= w[1].created_at));
    }

    fn rseg(id: &str, text: &str) -> RetrievedSegment {
        RetrievedSegment {
            id: id.into(),
            start: Some(0.0),
            speaker: "S".into(),
            voice_profile_id: None,
            text: text.into(),
        }
    }

    #[test]
    fn passages_take_lead_in_and_following_context() {
        let segs: Vec<RetrievedSegment> = (0..20).map(|i| rseg(&i.to_string(), &"x".repeat(90))).collect();
        // ~100 chars per segment, target 700 → 1 before + hit + 5 after.
        assert_eq!(passage_around(&segs, 10, 700), (9, 15));
        assert_eq!(passage_around(&segs, 0, 700), (0, 6));
        // Near the end: borrows more context from before.
        assert_eq!(passage_around(&segs, 19, 400), (16, 19));
        assert_eq!(passage_around(&segs[..1], 0, 700), (0, 0));
    }

    #[test]
    fn merging_sums_overlapping_scores_and_keeps_meetings_apart() {
        let c = |m: &str, lo, hi, score| Candidate { meeting_id: m.into(), lo, hi, score };
        let merged = merge_candidates(vec![c("a", 0, 4, 1.0), c("b", 2, 5, 1.0), c("a", 3, 8, 0.5), c("a", 20, 22, 0.1)]);
        assert_eq!(merged, vec![c("a", 0, 8, 1.5), c("a", 20, 22, 0.1), c("b", 2, 5, 1.0)]);
        // Over-long merges are split instead.
        let merged = merge_candidates(vec![c("a", 0, 10, 1.0), c("a", 9, 30, 1.0)]);
        assert_eq!(merged, vec![c("a", 0, 10, 1.0), c("a", 11, 30, 1.0)]);
    }

    #[test]
    fn selection_ranks_and_respects_caps_and_budget() {
        let c = |m: &str, lo, score| Candidate { meeting_id: m.into(), lo, hi: lo, score };
        let cands = vec![
            c("a", 0, 0.9),
            c("a", 10, 0.8),
            c("a", 20, 0.7),
            c("b", 0, 0.6),
            c("c", 0, 0.5),
            c("d", 0, 0.4),
        ];
        let limits = SelectionLimits { max_passages: 10, max_per_meeting: 2, max_meetings: 2 };
        let picked = select_passages(cands.clone(), |_| 10, 10_000, limits);
        let got: Vec<(&str, usize)> = picked.iter().map(|c| (c.meeting_id.as_str(), c.lo)).collect();
        assert_eq!(got, vec![("a", 0), ("a", 10), ("b", 0)]);

        // Budget: 2 meetings' scaffolding + passages at 10 tokens each.
        let budget = 2 * (10 + MEETING_SCAFFOLD_TOKENS);
        let picked = select_passages(cands, |_| 10, budget, SelectionLimits { max_per_meeting: 1, ..SelectionLimits::default() });
        let got: Vec<&str> = picked.iter().map(|c| c.meeting_id.as_str()).collect();
        assert_eq!(got, vec!["a", "b"]);
    }

    #[test]
    fn spread_is_even() {
        assert_eq!(spread(10, 2), vec![2, 7]);
        assert_eq!(spread(1, 2), vec![0]);
        assert!(spread(0, 2).is_empty());
    }

    #[test]
    fn filter_description() {
        assert_eq!(AskFilters::default().describe(), None);
        let f = AskFilters {
            person: Some(PersonFilter { voice_profile_id: None, name: " Kingfisher ".into() }),
            ..Default::default()
        };
        assert_eq!(f.describe().as_deref(), Some("passages where Kingfisher speaks"));
    }
}
