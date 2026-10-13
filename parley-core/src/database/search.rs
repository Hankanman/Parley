//! Full-text search over transcript segments (issue #79).
//!
//! Backed by the `transcripts_fts` FTS5 table (migration
//! `20261010000000_add_transcripts_fts.sql`), an external-content index over
//! `transcripts.transcript` kept in sync by triggers. Hits are ranked with
//! bm25 and carry an FTS5 `snippet()` with caller-chosen match markers.
//!
//! User input never reaches `MATCH` verbatim: [`match_expression`] turns it
//! into a conjunction of quoted terms (prefix search on the last one), so FTS5
//! operators and stray quotes are matched as text instead of being parsed.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

/// Upper bound on the number of terms taken from a query. Anything past this
/// is dropped; a 30-word "search" is a paste, not a query.
const MAX_TERMS: usize = 32;

/// Upper bound on `limit`, so a caller can't pull the whole corpus.
pub const MAX_LIMIT: u32 = 500;

/// Approximate number of tokens FTS5 puts in a snippet.
const SNIPPET_TOKENS: i64 = 16;

/// Text FTS5 puts where a snippet is cut off.
pub const SNIPPET_ELLIPSIS: &str = "…";

/// Markers wrapped around each matched term in [`SearchHit::snippet`].
#[derive(Debug, Clone, Copy)]
pub struct Highlight<'a> {
    pub start: &'a str,
    pub end: &'a str,
}

impl Highlight<'static> {
    /// No markers: a plain-text snippet.
    pub const NONE: Highlight<'static> = Highlight { start: "", end: "" };
    /// Markdown bold, for agent/LLM-facing output.
    pub const MARKDOWN: Highlight<'static> = Highlight {
        start: "**",
        end: "**",
    };
}

/// One ranked transcript-segment hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub transcript_id: String,
    pub meeting_id: String,
    pub meeting_title: String,
    /// Excerpt of the segment around the match, with matched terms wrapped in
    /// the requested [`Highlight`] markers and cut-offs marked with `…`.
    pub snippet: String,
    /// The segment's wall-clock timestamp, as stored.
    pub timestamp: String,
    /// Seconds from the start of the recording, when known — the value a
    /// jump-to-timestamp would seek to.
    pub audio_start_time: Option<f64>,
    /// bm25 score. Lower (more negative) is a better match; results are
    /// already sorted by it.
    pub rank: f64,
}

/// Turns free-form user input into a safe FTS5 `MATCH` expression, or `None`
/// when there's nothing searchable in it.
///
/// Each whitespace-separated word becomes a double-quoted FTS5 string (inner
/// quotes doubled), so `AND`, `NEAR`, `-`, `*`, `:`, parentheses and column
/// filters are all searched as plain text rather than parsed. Words are
/// implicitly ANDed. The last word gets a prefix `*` so results update while
/// the user is still typing it — unless the input ends in whitespace, which
/// means the word is finished. Words with no letters or digits are dropped:
/// the tokenizer would reduce them to nothing anyway, and an empty phrase is
/// not useful to match.
pub fn match_expression(input: &str) -> Option<String> {
    let words: Vec<&str> = input
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .take(MAX_TERMS)
        .collect();
    if words.is_empty() {
        return None;
    }
    let last_is_partial = !input.ends_with(char::is_whitespace);
    let last = words.len() - 1;
    let terms: Vec<String> = words
        .iter()
        .enumerate()
        .map(|(i, word)| {
            let quoted = format!("\"{}\"", word.replace('"', "\"\""));
            if i == last && last_is_partial {
                quoted + "*"
            } else {
                quoted
            }
        })
        .collect();
    Some(terms.join(" "))
}

/// Ranked full-text search across every transcript segment.
///
/// Returns at most `limit` (clamped to `1..=MAX_LIMIT`) segment hits, best
/// first. Several hits can belong to the same meeting; callers that want one
/// row per meeting can keep the first hit for each, which is that meeting's
/// best. An empty/unsearchable query returns no hits. A query FTS5 still
/// rejects (which [`match_expression`] should make impossible) is logged and
/// treated as "no hits" rather than surfaced as an error.
pub async fn search_transcripts(
    pool: &SqlitePool,
    query: &str,
    limit: u32,
    highlight: Highlight<'_>,
) -> Result<Vec<SearchHit>, sqlx::Error> {
    let Some(expression) = match_expression(query) else {
        return Ok(Vec::new());
    };
    let limit = limit.clamp(1, MAX_LIMIT);

    let rows = sqlx::query(
        "SELECT t.id AS transcript_id,
                t.meeting_id,
                m.title AS meeting_title,
                snippet(transcripts_fts, 0, ?, ?, ?, ?) AS snippet,
                t.timestamp,
                t.audio_start_time,
                transcripts_fts.rank AS rank
         FROM transcripts_fts
         JOIN transcripts t ON t.rowid = transcripts_fts.rowid
         JOIN meetings m ON m.id = t.meeting_id
         WHERE transcripts_fts MATCH ?
         ORDER BY transcripts_fts.rank
         LIMIT ?",
    )
    .bind(highlight.start)
    .bind(highlight.end)
    .bind(SNIPPET_ELLIPSIS)
    .bind(SNIPPET_TOKENS)
    .bind(&expression)
    .bind(limit as i64)
    .fetch_all(pool)
    .await;

    let rows = match rows {
        Ok(rows) => rows,
        Err(e) if is_fts_query_error(&e) => {
            log::warn!("FTS5 rejected search expression {expression:?}: {e}");
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };

    rows.into_iter()
        .map(|r| {
            Ok(SearchHit {
                transcript_id: r.try_get("transcript_id")?,
                meeting_id: r.try_get("meeting_id")?,
                meeting_title: r.try_get("meeting_title")?,
                snippet: r.try_get("snippet")?,
                timestamp: r.try_get("timestamp")?,
                audio_start_time: r.try_get("audio_start_time")?,
                rank: r.try_get("rank")?,
            })
        })
        .collect()
}

/// Rebuilds the whole FTS index from `transcripts`. Not needed in normal
/// operation (triggers keep it in sync); this is the repair path if the index
/// ever drifts, e.g. after a VACUUM renumbered the implicit rowids it keys on.
pub async fn rebuild_index(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO transcripts_fts (transcripts_fts) VALUES ('rebuild')")
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether `e` is FTS5 refusing the MATCH expression (a syntax error), as
/// opposed to a real database problem worth propagating.
fn is_fts_query_error(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db) => {
            let msg = db.message();
            msg.contains("fts5") || msg.contains("MATCH")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn add_meeting(pool: &SqlitePool, id: &str, title: &str) {
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, '2026-10-10T09:00:00+00:00', '2026-10-10T09:00:00+00:00')",
        )
        .bind(id)
        .bind(title)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_segment(pool: &SqlitePool, id: &str, meeting_id: &str, text: &str, start: f64) {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time) VALUES (?, ?, ?, '2026-10-10T09:00:00+00:00', ?)",
        )
        .bind(id)
        .bind(meeting_id)
        .bind(text)
        .bind(start)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn hits(pool: &SqlitePool, query: &str) -> Vec<SearchHit> {
        search_transcripts(
            pool,
            query,
            50,
            Highlight {
                start: "[",
                end: "]",
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn bundled_sqlite_has_fts5() {
        let pool = test_pool().await;
        let enabled: i64 = sqlx::query_scalar("SELECT sqlite_compileoption_used('ENABLE_FTS5')")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(enabled, 1, "bundled SQLite must be compiled with FTS5");
        // And the migration actually created the virtual table.
        let sql: String =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'transcripts_fts'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(sql.to_lowercase().contains("fts5"), "{sql}");
    }

    #[test]
    fn match_expression_quotes_terms_and_prefixes_the_last() {
        assert_eq!(
            match_expression("budget review").as_deref(),
            Some("\"budget\" \"review\"*")
        );
        assert_eq!(
            match_expression("budget review ").as_deref(),
            Some("\"budget\" \"review\"")
        );
        assert_eq!(match_expression("  budg").as_deref(), Some("\"budg\"*"));
    }

    #[test]
    fn match_expression_neutralises_fts_syntax() {
        assert_eq!(
            match_expression("say \"hi").as_deref(),
            Some("\"say\" \"\"\"hi\"*")
        );
        assert_eq!(
            match_expression("a AND b").as_deref(),
            Some("\"a\" \"AND\" \"b\"*")
        );
        assert_eq!(
            match_expression("transcript:foo").as_deref(),
            Some("\"transcript:foo\"*")
        );
    }

    #[test]
    fn match_expression_drops_unsearchable_input() {
        assert_eq!(match_expression(""), None);
        assert_eq!(match_expression("   "), None);
        assert_eq!(match_expression("\" * - ( )"), None);
        assert_eq!(match_expression("- budget").as_deref(), Some("\"budget\"*"));
    }

    #[tokio::test]
    async fn hostile_queries_never_error() {
        let pool = test_pool().await;
        add_meeting(&pool, "m1", "Standup").await;
        add_segment(&pool, "t1", "m1", "we should review the budget", 1.0).await;
        for q in [
            "\"",
            "\"\"",
            "*",
            "budget*",
            "NEAR(a b)",
            "a OR",
            "AND",
            "NOT budget",
            "(budget",
            "transcript:budget",
            "^budget",
            "budget\"review",
            "{a b}:c",
            "'; DROP TABLE transcripts; --",
            "a + b",
            "😀",
            "x".repeat(5000).as_str(),
        ] {
            // The sanitised expression must be valid FTS5 on its own — not
            // merely rescued by `search_transcripts`' error swallowing.
            if let Some(expr) = match_expression(q) {
                sqlx::query("SELECT count(*) FROM transcripts_fts WHERE transcripts_fts MATCH ?")
                    .bind(&expr)
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|e| panic!("query {q:?} -> {expr:?} errored: {e}"));
            }
            search_transcripts(&pool, q, 10, Highlight::NONE)
                .await
                .unwrap_or_else(|e| panic!("query {q:?} errored: {e}"));
        }
        // "NOT budget" is searched as two literal words, not as negation.
        assert!(hits(&pool, "budget").await.len() == 1);
    }

    #[tokio::test]
    async fn finds_ranks_and_snippets_hits() {
        let pool = test_pool().await;
        add_meeting(&pool, "m1", "Standup").await;
        add_meeting(&pool, "m2", "Planning").await;
        add_segment(
            &pool,
            "t1",
            "m1",
            "the weather is nice and the budget came up once",
            3.5,
        )
        .await;
        add_segment(
            &pool,
            "t2",
            "m2",
            "budget budget budget: the budget review",
            42.0,
        )
        .await;
        add_segment(&pool, "t3", "m2", "nothing relevant here", 50.0).await;

        let results = hits(&pool, "budget").await;
        assert_eq!(results.len(), 2);
        // The segment that is nearly all "budget" outranks the passing mention.
        assert_eq!(results[0].transcript_id, "t2");
        assert_eq!(results[0].meeting_id, "m2");
        assert_eq!(results[0].meeting_title, "Planning");
        assert_eq!(results[0].audio_start_time, Some(42.0));
        assert!(results[0].rank <= results[1].rank);
        assert!(
            results[1].snippet.contains("[budget]"),
            "{}",
            results[1].snippet
        );
    }

    #[tokio::test]
    async fn prefix_and_diacritics() {
        let pool = test_pool().await;
        add_meeting(&pool, "m1", "Standup").await;
        add_segment(
            &pool,
            "t1",
            "m1",
            "Meet at the Café for the quarterly roadmap",
            0.0,
        )
        .await;
        assert_eq!(
            hits(&pool, "quart").await.len(),
            1,
            "prefix on the last term"
        );
        assert_eq!(
            hits(&pool, "quart ").await.len(),
            0,
            "a finished word isn't a prefix"
        );
        assert_eq!(hits(&pool, "cafe").await.len(), 1, "diacritics are folded");
        assert_eq!(hits(&pool, "ROADMAP").await.len(), 1, "case-insensitive");
        assert_eq!(
            hits(&pool, "roadmap banana").await.len(),
            0,
            "terms are ANDed"
        );
    }

    #[tokio::test]
    async fn triggers_keep_index_in_sync() {
        let pool = test_pool().await;
        add_meeting(&pool, "m1", "Standup").await;
        add_segment(&pool, "t1", "m1", "alpha", 0.0).await;
        assert_eq!(hits(&pool, "alpha").await.len(), 1);

        sqlx::query("UPDATE transcripts SET transcript = 'bravo' WHERE id = 't1'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(hits(&pool, "alpha").await.is_empty());
        assert_eq!(hits(&pool, "bravo").await.len(), 1);

        // Non-text updates don't disturb the index.
        sqlx::query("UPDATE transcripts SET speaker = 'mic' WHERE id = 't1'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(hits(&pool, "bravo").await.len(), 1);

        sqlx::query("DELETE FROM transcripts WHERE id = 't1'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(hits(&pool, "bravo").await.is_empty());

        // Deleting a meeting removes its segments from search too (whether the
        // app deletes transcripts explicitly or via ON DELETE CASCADE).
        add_segment(&pool, "t2", "m1", "charlie", 0.0).await;
        crate::database::repositories::meeting::MeetingsRepository::delete_meeting(&pool, "m1")
            .await
            .unwrap();
        assert!(hits(&pool, "charlie").await.is_empty());

        let check = sqlx::query(
            "INSERT INTO transcripts_fts (transcripts_fts, rank) VALUES ('integrity-check', 1)",
        )
        .execute(&pool)
        .await;
        assert!(check.is_ok(), "index out of sync with content: {check:?}");
    }

    #[tokio::test]
    async fn live_upsert_reindexes_text() {
        let pool = test_pool().await;
        add_meeting(&pool, "m1", "Standup").await;
        let upsert = |text: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, sequence_id)
                     VALUES (?, 'm1', ?, 'ts', 7)
                     ON CONFLICT(meeting_id, sequence_id) DO UPDATE SET transcript = excluded.transcript",
                )
                .bind(format!("t-{text}"))
                .bind(text)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        upsert("draft words").await;
        upsert("final words").await;
        assert!(hits(&pool, "draft").await.is_empty());
        assert_eq!(hits(&pool, "final").await.len(), 1);
    }

    #[tokio::test]
    async fn migration_backfills_existing_rows() {
        // Run every migration before the FTS one, insert data, then run the
        // rest — the FTS migration must index rows that predate it.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut migrator = sqlx::migrate!("./migrations");
        let fts_version = 20261010000000_i64;
        let all = migrator.migrations.clone();
        migrator.migrations = all
            .iter()
            .filter(|m| m.version < fts_version)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        migrator.run(&pool).await.unwrap();

        add_meeting(&pool, "m1", "Old meeting").await;
        add_segment(&pool, "t1", "m1", "legacy segment about kubernetes", 0.0).await;

        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let results = hits(&pool, "kubernetes").await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].meeting_title, "Old meeting");
    }
}
