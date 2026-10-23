//! Semantic search over the transcript-chunk embeddings the app stores for
//! Ask Parley (`transcript_chunk_embeddings`; see
//! `parley_core::chat::across::embeddings`).
//!
//! This server only *reads* the index: the app builds and refreshes it in
//! the background whenever Ask Parley is open. Searching needs one network
//! call, embedding the query with the same Ollama model, and follows the
//! app's rule: only when the summary provider is Ollama (so the endpoint is
//! one the user already chose), with the `ask_embedding_model` setting
//! (default `nomic-embed-text`; empty turns it off).

use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::db::format_offset;

const KEY_ASK_EMBEDDING_MODEL: &str = "ask_embedding_model";
const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";
const DEFAULT_OLLAMA_ENDPOINT: &str = "http://localhost:11434";
/// Below this cosine similarity a chunk isn't related at all (as in the app).
const MIN_SIMILARITY: f32 = 0.2;
/// Longest excerpt returned per hit, in characters.
const EXCERPT_CHARS: usize = 1200;

/// Where the query gets embedded.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingBackend {
    pub endpoint: String,
    pub model: String,
}

impl EmbeddingBackend {
    /// The id stored with every vector, e.g. `ollama:nomic-embed-text`.
    pub fn model_id(&self) -> String {
        format!("ollama:{}", self.model)
    }

    /// nomic-embed-text retrieves much better with its task prefix.
    fn query_prefix(&self) -> &'static str {
        if self.model.contains("nomic-embed") {
            "search_query: "
        } else {
            ""
        }
    }
}

/// The embedding backend for the current settings, or why there isn't one.
pub async fn resolve_backend(pool: &SqlitePool) -> Result<EmbeddingBackend, String> {
    let config: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT provider, ollamaEndpoint FROM settings LIMIT 1")
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("couldn't read the model settings: {e}"))?;
    let (provider, endpoint) = config.unwrap_or((None, None));
    if !provider.is_some_and(|p| p.trim().eq_ignore_ascii_case("ollama")) {
        return Err("semantic search needs Ollama as Parley's summary provider \
                    (Settings → Summary); use `search_transcripts` instead"
            .into());
    }
    let model: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = ?")
            .bind(KEY_ASK_EMBEDDING_MODEL)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("couldn't read the embedding model setting: {e}"))?;
    let model = model
        .and_then(|json| serde_json::from_str::<String>(&json).ok())
        .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("semantic search is turned off in Parley's settings; use `search_transcripts`".into());
    }
    let endpoint = endpoint
        .map(|e| e.trim().trim_end_matches('/').to_string())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_string());
    Ok(EmbeddingBackend { endpoint, model })
}

/// Embed `query` with Ollama's `/api/embed`, L2-normalised.
pub async fn embed_query(backend: &EmbeddingBackend, query: &str) -> Result<Vec<f32>, String> {
    #[derive(serde::Deserialize)]
    struct Response {
        embeddings: Vec<Vec<f32>>,
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("{}/api/embed", backend.endpoint))
        .json(&serde_json::json!({
            "model": backend.model,
            "input": [format!("{}{}", backend.query_prefix(), query)],
        }))
        .send()
        .await
        .map_err(|e| format!("couldn't reach Ollama at {}: {e}", backend.endpoint))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Ollama returned {status} (is `{}` pulled?): {}",
            backend.model,
            body.chars().take(200).collect::<String>()
        ));
    }
    let mut vector = response
        .json::<Response>()
        .await
        .map_err(|e| format!("bad Ollama embeddings response: {e}"))?
        .embeddings
        .into_iter()
        .next()
        .ok_or("Ollama returned no embedding")?;
    normalize(&mut vector);
    Ok(vector)
}

pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 && norm.is_finite() {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn bytes_to_floats(bytes: &[u8]) -> Option<Vec<f32>> {
    let (floats, rest) = bytes.as_chunks::<4>();
    rest.is_empty()
        .then(|| floats.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// One passage that matched a semantic search.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SemanticHit {
    pub meeting_id: String,
    pub meeting_title: String,
    pub meeting_created_at: String,
    /// Cosine similarity to the query, 0–1.
    pub similarity: f32,
    /// `HH:MM:SS` where the passage starts, when known.
    pub start: Option<String>,
    /// The passage, as `Speaker: text` lines.
    pub excerpt: String,
}

/// How much of the corpus the app has indexed with this model.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct IndexCoverage {
    pub model: String,
    pub meetings_indexed: i64,
    pub meetings_total: i64,
}

pub async fn coverage(pool: &SqlitePool, model_id: &str) -> Result<IndexCoverage, sqlx::Error> {
    let meetings_indexed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meeting_embedding_state s JOIN meetings m ON m.id = s.meeting_id WHERE s.model = ?",
    )
    .bind(model_id)
    .fetch_one(pool)
    .await?;
    let meetings_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meetings WHERE COALESCE(status, 'completed') != 'recording'",
    )
    .fetch_one(pool)
    .await?;
    Ok(IndexCoverage {
        model: model_id.to_string(),
        meetings_indexed,
        meetings_total,
    })
}

/// The best `limit` stored chunks for `query_vector`, with their text
/// rebuilt from the transcript. `meeting_filter` adds conditions on the
/// meetings table (aliased `m`).
pub async fn search(
    pool: &SqlitePool,
    model_id: &str,
    query_vector: &[f32],
    limit: usize,
    meeting_filter: impl FnOnce(&mut QueryBuilder<'_, Sqlite>),
) -> Result<Vec<SemanticHit>, sqlx::Error> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT e.meeting_id, e.first_transcript_id, e.segment_count, e.vector, m.title, m.created_at
         FROM transcript_chunk_embeddings e JOIN meetings m ON m.id = e.meeting_id
         WHERE e.model = ",
    );
    qb.push_bind(model_id.to_string())
        .push(" AND e.dim = ")
        .push_bind(query_vector.len() as i64);
    meeting_filter(&mut qb);
    let rows: Vec<(String, String, i64, Vec<u8>, String, String)> =
        qb.build_query_as().fetch_all(pool).await?;

    let mut scored: Vec<_> = rows
        .into_iter()
        .filter_map(|(meeting_id, first_id, count, blob, title, created_at)| {
            let similarity = dot(query_vector, &bytes_to_floats(&blob)?);
            (similarity.is_finite() && similarity >= MIN_SIMILARITY)
                .then_some((similarity, meeting_id, first_id, count.max(1) as usize, title, created_at))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    let mut segments_by_meeting: HashMap<String, Vec<(String, Option<f64>, String)>> = HashMap::new();
    let mut hits = Vec::with_capacity(scored.len());
    for (similarity, meeting_id, first_id, count, title, created_at) in scored {
        if !segments_by_meeting.contains_key(&meeting_id) {
            let segments = load_segments(pool, &meeting_id).await?;
            segments_by_meeting.insert(meeting_id.clone(), segments);
        }
        let segments = &segments_by_meeting[&meeting_id];
        let Some(first) = segments.iter().position(|(id, _, _)| *id == first_id) else {
            // Re-transcribed since it was indexed; the app will re-index it.
            continue;
        };
        let passage = &segments[first..(first + count).min(segments.len())];
        let mut excerpt = passage.iter().map(|(_, _, line)| line.as_str()).collect::<Vec<_>>().join("\n");
        if excerpt.chars().count() > EXCERPT_CHARS {
            excerpt = excerpt.chars().take(EXCERPT_CHARS).collect::<String>() + "…";
        }
        hits.push(SemanticHit {
            meeting_id,
            meeting_title: title,
            meeting_created_at: created_at,
            similarity,
            start: format_offset(passage[0].1),
            excerpt,
        });
    }
    Ok(hits)
}

/// A meeting's segments in the order the app chunked them
/// (`parley_core::chat::across::retrieval::load_segments`), as
/// `(id, start, "Speaker: text")`.
/// `(id, start, speaker, voice profile name, text)`.
type SegmentRow = (String, Option<f64>, Option<String>, Option<String>, String);

async fn load_segments(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Vec<(String, Option<f64>, String)>, sqlx::Error> {
    let rows: Vec<SegmentRow> = sqlx::query_as(
        "SELECT t.id, t.audio_start_time, t.speaker, vp.name, t.transcript
         FROM transcripts t LEFT JOIN voice_profiles vp ON vp.id = t.voice_profile_id
         WHERE t.meeting_id = ? AND TRIM(t.transcript) != ''
         ORDER BY COALESCE(t.audio_start_time, 0.0), t.sequence_id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, start, speaker, profile, text)| {
            let speaker = profile
                .or(speaker)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Speaker".to_string());
            (id, start, format!("{speaker}: {}", text.trim()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn floats_to_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("../parley-core/migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn resolve_backend_follows_the_apps_rules() {
        let pool = pool().await;
        assert!(resolve_backend(&pool).await.unwrap_err().contains("Ollama"));

        sqlx::query("INSERT INTO settings (id, provider, model, whisperModel, ollamaEndpoint) VALUES ('1', 'ollama', 'gemma3:4b', 'x', 'http://gpu-box:11434/')")
            .execute(&pool).await.unwrap();
        let backend = resolve_backend(&pool).await.unwrap();
        assert_eq!(backend.endpoint, "http://gpu-box:11434");
        assert_eq!(backend.model_id(), "ollama:nomic-embed-text");

        sqlx::query("INSERT INTO app_settings (key, value, updated_at) VALUES ('ask_embedding_model', '\"\"', 'x')")
            .execute(&pool).await.unwrap();
        assert!(resolve_backend(&pool).await.unwrap_err().contains("turned off"));
    }

    #[tokio::test]
    async fn search_ranks_chunks_and_rebuilds_their_text() {
        let pool = pool().await;
        let now = "2026-10-20T09:00:00+00:00";
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES ('m1', 'Pricing', ?, ?)")
            .bind(now).bind(now).execute(&pool).await.unwrap();
        for (id, start, speaker, text) in [
            ("t1", 0.0, "Alex", "Morning."),
            ("t2", 5.0, "Sam", "The cost is too high."),
            ("t3", 9.0, "Alex", "Let's cut the price."),
        ] {
            sqlx::query("INSERT INTO transcripts (id, meeting_id, transcript, timestamp, speaker, audio_start_time) VALUES (?, 'm1', ?, '', ?, ?)")
                .bind(id).bind(text).bind(speaker).bind(start).execute(&pool).await.unwrap();
        }
        for (ix, first, count, vector) in [(0, "t1", 1, [0.0f32, 1.0]), (1, "t2", 2, [1.0, 0.0])] {
            sqlx::query("INSERT INTO transcript_chunk_embeddings (meeting_id, chunk_ix, first_transcript_id, segment_count, model, dim, vector) VALUES ('m1', ?, ?, ?, 'ollama:test', 2, ?)")
                .bind(ix).bind(first).bind(count).bind(floats_to_bytes(&vector))
                .execute(&pool).await.unwrap();
        }

        let hits = search(&pool, "ollama:test", &[1.0, 0.0], 5, |_| {}).await.unwrap();
        assert_eq!(hits.len(), 1, "the orthogonal chunk is below the similarity floor");
        assert_eq!(hits[0].excerpt, "Sam: The cost is too high.\nAlex: Let's cut the price.");
        assert_eq!(hits[0].start.as_deref(), Some("00:00:05"));

        assert!(search(&pool, "ollama:other", &[1.0, 0.0], 5, |_| {}).await.unwrap().is_empty());
        assert!(search(&pool, "ollama:test", &[1.0, 0.0, 0.0], 5, |_| {}).await.unwrap().is_empty());
    }
}
