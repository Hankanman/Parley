//! Semantic stage of cross-meeting retrieval: sentence embeddings of
//! transcript chunks, stored in SQLite and searched by brute-force cosine.
//!
//! ## Backend
//!
//! Embeddings come from an [`Embedder`]. Today's implementation is
//! [`OllamaEmbedder`] (`POST /api/embed`), used when the user's summary
//! provider is Ollama — the server is already configured and running, and
//! `ollama pull nomic-embed-text` is all it takes. The model name is the
//! `ask_embedding_model` app setting (default [`DEFAULT_OLLAMA_MODEL`]; an
//! empty string turns the semantic stage off). With any other provider the
//! semantic stage is off and retrieval is FTS5-only.
//!
//! The seam for a fully built-in path is the [`Embedder`] trait: a
//! llama.cpp embedding mode in the `llama-helper` sidecar (a small GGUF such
//! as nomic-embed-text or bge-small, downloaded through the summary model
//! manager) only has to implement it and be returned by [`resolve_embedder`].
//!
//! ## Storage and indexing
//!
//! Each meeting's segments (display order) are grouped into chunks of about
//! [`CHUNK_CHARS`] and each chunk's L2-normalised vector is stored in
//! `transcript_chunk_embeddings`. [`backfill`] (run in the background via
//! [`spawn_backfill`], single-flight) re-embeds only meetings whose cheap
//! transcript signature (segment count, max rowid, total text length) or
//! embedding model changed since `meeting_embedding_state` last recorded it,
//! so new and re-transcribed meetings are picked up lazily. Meetings still
//! recording are skipped. Rows go with their meeting.
//!
//! If the backend fails (Ollama down, model not pulled), the error is
//! remembered for [`FAILURE_COOLDOWN`] so asks don't each wait on it, and
//! retrieval carries on with keywords only.

use super::retrieval::{load_segments, push_meeting_filters, AskFilters, ChunkHit, RetrievedSegment};
use crate::database::repositories::setting::SettingsRepository;
use crate::database::repositories::voice_profile::{bytes_to_floats, floats_to_bytes};
use crate::summary::llm_client::LLMProvider;
use once_cell::sync::Lazy;
use sqlx::{QueryBuilder, Sqlite, SqlitePool};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// App setting holding the Ollama embedding model name (JSON string).
pub const KEY_ASK_EMBEDDING_MODEL: &str = "ask_embedding_model";
pub const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";
/// Target chunk size, in characters (well inside small embedders' windows).
pub const CHUNK_CHARS: usize = 800;
/// Texts per embedding request.
const BATCH: usize = 16;
/// Bumped when chunking changes, so every meeting is re-indexed.
const CHUNKING_VERSION: u32 = 1;
/// How long a failed backend is left alone.
pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(600);
/// Below this cosine similarity a chunk isn't considered related at all.
const MIN_SIMILARITY: f32 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    /// The user's question.
    Query,
    /// A transcript chunk being indexed.
    Document,
}

pub type EmbedFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>, String>> + Send + 'a>>;

/// A sentence-embedding backend.
pub trait Embedder: Send + Sync {
    /// Stable id of the model, stored with every vector (e.g.
    /// `ollama:nomic-embed-text`). Vectors from different ids never mix.
    fn model_id(&self) -> String;
    /// One vector per input text, in order.
    fn embed<'a>(&'a self, texts: &'a [String], kind: EmbedKind) -> EmbedFuture<'a>;
}

/// Embeddings from an Ollama server's `/api/embed`.
pub struct OllamaEmbedder {
    pub endpoint: String,
    pub model: String,
    client: reqwest::Client,
}

impl OllamaEmbedder {
    pub fn new(endpoint: Option<String>, model: String) -> Self {
        let endpoint = endpoint
            .map(|e| e.trim().trim_end_matches('/').to_string())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self { endpoint, model, client }
    }

    /// nomic-embed-text is trained with task prefixes and retrieves much
    /// better with them.
    fn prefix(&self, kind: EmbedKind) -> &'static str {
        if self.model.contains("nomic-embed") {
            match kind {
                EmbedKind::Query => "search_query: ",
                EmbedKind::Document => "search_document: ",
            }
        } else {
            ""
        }
    }
}

impl Embedder for OllamaEmbedder {
    fn model_id(&self) -> String {
        format!("ollama:{}", self.model)
    }

    fn embed<'a>(&'a self, texts: &'a [String], kind: EmbedKind) -> EmbedFuture<'a> {
        Box::pin(async move {
            #[derive(serde::Deserialize)]
            struct Response {
                embeddings: Vec<Vec<f32>>,
            }
            let prefix = self.prefix(kind);
            let input: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
            let response = self
                .client
                .post(format!("{}/api/embed", self.endpoint))
                .json(&serde_json::json!({ "model": self.model, "input": input }))
                .send()
                .await
                .map_err(|e| format!("Ollama embeddings request failed: {e}"))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(format!(
                    "Ollama embeddings returned {status}: {}",
                    body.chars().take(200).collect::<String>()
                ));
            }
            let parsed: Response = response
                .json()
                .await
                .map_err(|e| format!("Bad Ollama embeddings response: {e}"))?;
            if parsed.embeddings.len() != texts.len() {
                return Err(format!(
                    "Ollama returned {} embeddings for {} inputs",
                    parsed.embeddings.len(),
                    texts.len()
                ));
            }
            Ok(parsed.embeddings)
        })
    }
}

/// Whether semantic retrieval is available, and why not.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticAvailability {
    Ready { model_id: String },
    /// Turned off, or the provider has no embedding backend yet.
    Off(String),
    /// The backend failed recently.
    Failing { model_id: String, error: String },
}

static LAST_FAILURE: Lazy<Mutex<Option<(String, Instant, String)>>> = Lazy::new(|| Mutex::new(None));

fn record_failure(model_id: &str, error: &str) {
    if let Ok(mut f) = LAST_FAILURE.lock() {
        *f = Some((model_id.to_string(), Instant::now(), error.to_string()));
    }
}

fn clear_failure(model_id: &str) {
    if let Ok(mut f) = LAST_FAILURE.lock() {
        if f.as_ref().is_some_and(|(m, _, _)| m == model_id) {
            *f = None;
        }
    }
}

fn recent_failure(model_id: &str) -> Option<String> {
    let f = LAST_FAILURE.lock().ok()?;
    let (m, at, err) = f.as_ref()?;
    (m == model_id && at.elapsed() < FAILURE_COOLDOWN).then(|| err.clone())
}

/// The embedder for the current settings, or why there isn't one.
pub async fn resolve_embedder(pool: &SqlitePool) -> Result<Arc<dyn Embedder>, SemanticAvailability> {
    let config = SettingsRepository::get_model_config(pool)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| SemanticAvailability::Off("no model configured".into()))?;
    if LLMProvider::from_str(&config.provider).ok() != Some(LLMProvider::Ollama) {
        return Err(SemanticAvailability::Off(
            "semantic search currently needs Ollama as the summary provider".into(),
        ));
    }
    let model = SettingsRepository::get_setting::<String>(pool, KEY_ASK_EMBEDDING_MODEL)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(SemanticAvailability::Off("turned off in settings".into()));
    }
    Ok(Arc::new(OllamaEmbedder::new(config.ollama_endpoint, model)))
}

/// Availability for the current settings (no network call).
pub async fn availability(pool: &SqlitePool) -> SemanticAvailability {
    match resolve_embedder(pool).await {
        Err(off) => off,
        Ok(embedder) => {
            let model_id = embedder.model_id();
            match recent_failure(&model_id) {
                Some(error) => SemanticAvailability::Failing { model_id, error },
                None => SemanticAvailability::Ready { model_id },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Vectors
// ---------------------------------------------------------------------------

/// Scale `v` to unit length (zero vectors are left alone).
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 && norm.is_finite() {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A run of consecutive segments embedded as one vector.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedChunk {
    pub first_index: usize,
    pub segment_count: usize,
    pub text: String,
}

/// Group segments into chunks of about `max_chars` (`Speaker: text` lines,
/// no timestamps — they carry no meaning). An oversized segment is a chunk
/// of its own.
pub fn chunk_for_embedding(segments: &[RetrievedSegment], max_chars: usize) -> Vec<EmbedChunk> {
    let mut chunks = Vec::new();
    let mut current: Option<EmbedChunk> = None;
    for (ix, s) in segments.iter().enumerate() {
        let line = format!("{}: {}", s.speaker, s.text.trim());
        match current.as_mut() {
            Some(c) if c.text.len() + 1 + line.len() <= max_chars => {
                c.text.push('\n');
                c.text.push_str(&line);
                c.segment_count += 1;
            }
            _ => {
                if let Some(done) = current.take() {
                    chunks.push(done);
                }
                current = Some(EmbedChunk {
                    first_index: ix,
                    segment_count: 1,
                    text: line,
                });
            }
        }
    }
    chunks.extend(current);
    chunks
}

// ---------------------------------------------------------------------------
// Backfill
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub meetings_indexed: usize,
    pub chunks_embedded: usize,
    /// Meetings already up to date.
    pub meetings_current: usize,
}

/// Index every meeting whose transcript or embedding model changed since it
/// was last indexed. Stops at the first backend error (recorded for the
/// cooldown) and returns it.
pub async fn backfill(pool: &SqlitePool, embedder: &dyn Embedder) -> Result<BackfillReport, String> {
    let model_id = embedder.model_id();
    let signatures: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT m.id, COUNT(t.rowid), COALESCE(MAX(t.rowid), 0), COALESCE(SUM(LENGTH(t.transcript)), 0)
         FROM meetings m LEFT JOIN transcripts t ON t.meeting_id = m.id
         WHERE COALESCE(m.status, 'completed') != 'recording'
         GROUP BY m.id
         ORDER BY m.created_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("failed to read transcript signatures: {e}"))?;
    let state: HashMap<String, (String, String)> =
        sqlx::query_as::<_, (String, String, String)>("SELECT meeting_id, model, signature FROM meeting_embedding_state")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("failed to read embedding state: {e}"))?
            .into_iter()
            .map(|(id, model, sig)| (id, (model, sig)))
            .collect();

    let mut report = BackfillReport::default();
    for (meeting_id, count, max_rowid, total_len) in signatures {
        let signature = format!("v{CHUNKING_VERSION}:{count}:{max_rowid}:{total_len}");
        if state.get(&meeting_id) == Some(&(model_id.clone(), signature.clone())) {
            report.meetings_current += 1;
            continue;
        }
        let segments = load_segments(pool, &meeting_id)
            .await
            .map_err(|e| format!("failed to load transcript: {e}"))?;
        let chunks = chunk_for_embedding(&segments, CHUNK_CHARS);
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(BATCH) {
            let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
            let embedded = match embedder.embed(&texts, EmbedKind::Document).await {
                Ok(v) => v,
                Err(e) => {
                    record_failure(&model_id, &e);
                    return Err(e);
                }
            };
            vectors.extend(embedded.into_iter().map(|mut v| {
                normalize(&mut v);
                v
            }));
        }
        clear_failure(&model_id);

        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        // The meeting may have been deleted while we were embedding.
        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM meetings WHERE id = ?")
            .bind(&meeting_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        if exists.is_none() {
            continue;
        }
        sqlx::query("DELETE FROM transcript_chunk_embeddings WHERE meeting_id = ?")
            .bind(&meeting_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        for (ix, (chunk, vector)) in chunks.iter().zip(&vectors).enumerate() {
            sqlx::query(
                "INSERT INTO transcript_chunk_embeddings
                 (meeting_id, chunk_ix, first_transcript_id, segment_count, model, dim, vector)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&meeting_id)
            .bind(ix as i64)
            .bind(&segments[chunk.first_index].id)
            .bind(chunk.segment_count as i64)
            .bind(&model_id)
            .bind(vector.len() as i64)
            .bind(floats_to_bytes(vector))
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        sqlx::query(
            "INSERT INTO meeting_embedding_state (meeting_id, model, signature, chunk_count, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(meeting_id) DO UPDATE SET model = excluded.model, signature = excluded.signature,
                chunk_count = excluded.chunk_count, updated_at = excluded.updated_at",
        )
        .bind(&meeting_id)
        .bind(&model_id)
        .bind(&signature)
        .bind(chunks.len() as i64)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        report.meetings_indexed += 1;
        report.chunks_embedded += chunks.len();
    }
    Ok(report)
}

static BACKFILL_RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether a background backfill is running.
pub fn backfill_running() -> bool {
    BACKFILL_RUNNING.load(Ordering::SeqCst)
}

/// Start [`backfill`] in the background unless one is already running or
/// the backend is cooling down after a failure. Needs a tokio runtime.
pub fn spawn_backfill(pool: SqlitePool, embedder: Arc<dyn Embedder>) {
    if recent_failure(&embedder.model_id()).is_some() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if BACKFILL_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    handle.spawn(async move {
        match backfill(&pool, embedder.as_ref()).await {
            Ok(r) if r.meetings_indexed > 0 => log::info!(
                "Ask Parley: embedded {} chunk(s) across {} meeting(s) with {}",
                r.chunks_embedded,
                r.meetings_indexed,
                embedder.model_id()
            ),
            Ok(_) => {}
            Err(e) => log::warn!("Ask Parley: embedding backfill stopped: {e}"),
        }
        BACKFILL_RUNNING.store(false, Ordering::SeqCst);
    });
}

/// (meetings indexed with `model_id`, meetings that can be indexed).
pub async fn index_progress(pool: &SqlitePool, model_id: &str) -> Result<(i64, i64), sqlx::Error> {
    let indexed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meeting_embedding_state s JOIN meetings m ON m.id = s.meeting_id WHERE s.model = ?",
    )
    .bind(model_id)
    .fetch_one(pool)
    .await?;
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM meetings WHERE COALESCE(status, 'completed') != 'recording'")
            .fetch_one(pool)
            .await?;
    Ok((indexed, total))
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Best `limit` stored chunks by cosine similarity to `query_vector`
/// (already normalised), in meetings passing `filters`.
pub async fn search_vectors(
    pool: &SqlitePool,
    model_id: &str,
    query_vector: &[f32],
    filters: &AskFilters,
    limit: usize,
) -> Result<Vec<ChunkHit>, sqlx::Error> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT e.meeting_id, e.first_transcript_id, e.segment_count, e.vector
         FROM transcript_chunk_embeddings e JOIN meetings m ON m.id = e.meeting_id
         WHERE e.model = ",
    );
    qb.push_bind(model_id.to_string());
    qb.push(" AND e.dim = ").push_bind(query_vector.len() as i64);
    push_meeting_filters(&mut qb, filters);
    let rows: Vec<(String, String, i64, Vec<u8>)> = qb.build_query_as().fetch_all(pool).await?;
    let mut hits: Vec<ChunkHit> = rows
        .into_iter()
        .filter_map(|(meeting_id, first_transcript_id, segment_count, blob)| {
            let v = bytes_to_floats(&blob)?;
            let similarity = dot(query_vector, &v);
            (similarity.is_finite() && similarity >= MIN_SIMILARITY).then(|| ChunkHit {
                meeting_id,
                first_transcript_id,
                segment_count: segment_count.max(1) as usize,
                similarity,
            })
        })
        .collect();
    hits.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);
    Ok(hits)
}

/// Embed `query` and search. Backend errors are recorded (cooldown) and
/// returned; callers fall back to keywords only.
pub async fn semantic_candidates(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
    query: &str,
    filters: &AskFilters,
    limit: usize,
) -> Result<Vec<ChunkHit>, String> {
    let model_id = embedder.model_id();
    if let Some(error) = recent_failure(&model_id) {
        return Err(error);
    }
    let mut vector = match embedder.embed(&[query.to_string()], EmbedKind::Query).await {
        Ok(mut v) if !v.is_empty() => v.remove(0),
        Ok(_) => return Err("no query embedding returned".into()),
        Err(e) => {
            record_failure(&model_id, &e);
            return Err(e);
        }
    };
    normalize(&mut vector);
    search_vectors(pool, &model_id, &vector, filters, limit)
        .await
        .map_err(|e| format!("vector search failed: {e}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chat::context::keywords;

    /// Deterministic bag-of-words embedder over a fixed vocabulary.
    pub(crate) struct FakeEmbedder {
        pub vocab: Vec<&'static str>,
        pub fail: bool,
    }

    impl Embedder for FakeEmbedder {
        fn model_id(&self) -> String {
            if self.fail { "fake:failing".into() } else { "fake:bow".into() }
        }
        fn embed<'a>(&'a self, texts: &'a [String], _kind: EmbedKind) -> EmbedFuture<'a> {
            Box::pin(async move {
                if self.fail {
                    return Err("backend down".into());
                }
                Ok(texts
                    .iter()
                    .map(|t| {
                        let words = keywords(t);
                        self.vocab
                            .iter()
                            .map(|v| words.iter().filter(|w| w.starts_with(v)).count() as f32)
                            .collect()
                    })
                    .collect())
            })
        }
    }

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn add(pool: &SqlitePool, meeting: &str, lines: &[&str]) {
        sqlx::query("INSERT OR IGNORE INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, '2026-09-01T00:00:00Z', 'x')")
            .bind(meeting)
            .bind(meeting)
            .execute(pool)
            .await
            .unwrap();
        let base: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transcripts WHERE meeting_id = ?")
            .bind(meeting)
            .fetch_one(pool)
            .await
            .unwrap();
        for (i, line) in lines.iter().enumerate() {
            let seq = base + i as i64;
            sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, speaker, sequence_id)
                 VALUES (?, ?, ?, 'x', ?, 'S', ?)",
            )
            .bind(format!("{meeting}-{seq}"))
            .bind(meeting)
            .bind(*line)
            .bind(seq as f64)
            .bind(seq)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    fn fake() -> FakeEmbedder {
        FakeEmbedder { vocab: vec!["budget", "finance", "money", "holiday", "travel"], fail: false }
    }

    #[test]
    fn chunks_group_lines_and_vectors_normalise() {
        let seg = |id: &str, text: &str| RetrievedSegment {
            id: id.into(),
            start: None,
            speaker: "A".into(),
            voice_profile_id: None,
            text: text.into(),
        };
        let segs = vec![seg("1", &"x".repeat(30)), seg("2", &"y".repeat(30)), seg("3", &"z".repeat(30))];
        let chunks = chunk_for_embedding(&segs, 70);
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].first_index, chunks[0].segment_count), (0, 2));
        assert_eq!((chunks[1].first_index, chunks[1].segment_count), (2, 1));
        assert!(chunks[0].text.starts_with("A: xxx"));

        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
        let mut zero = vec![0.0, 0.0];
        normalize(&mut zero);
        assert_eq!(zero, vec![0.0, 0.0]);
    }

    #[tokio::test]
    async fn backfill_is_incremental_and_search_ranks_by_meaning() {
        let pool = test_pool().await;
        add(&pool, "fin", &["The budget is tight.", "Finance wants the money back."]).await;
        add(&pool, "trip", &["Holiday travel plans for the team."]).await;
        let embedder = fake();

        let r = backfill(&pool, &embedder).await.unwrap();
        assert_eq!(r.meetings_indexed, 2);
        assert_eq!(r.chunks_embedded, 2);
        // Nothing changed: nothing re-embedded.
        let r = backfill(&pool, &embedder).await.unwrap();
        assert_eq!((r.meetings_indexed, r.meetings_current), (0, 2));
        // A new segment re-indexes just that meeting.
        add(&pool, "trip", &["More travel."]).await;
        let r = backfill(&pool, &embedder).await.unwrap();
        assert_eq!((r.meetings_indexed, r.meetings_current), (1, 1));
        assert_eq!(index_progress(&pool, "fake:bow").await.unwrap(), (2, 2));

        let hits = semantic_candidates(&pool, &embedder, "money and finance", &AskFilters::default(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "unrelated chunks fall below the similarity floor");
        assert_eq!(hits[0].meeting_id, "fin");
        assert_eq!(hits[0].first_transcript_id, "fin-0");
        assert_eq!(hits[0].segment_count, 2);

        // Filters apply to vector search too.
        let none = AskFilters {
            from: Some(chrono::Utc::now()),
            ..Default::default()
        };
        assert!(semantic_candidates(&pool, &embedder, "money", &none, 10).await.unwrap().is_empty());

        // Deleting a meeting removes its vectors.
        crate::database::repositories::meeting::MeetingsRepository::delete_meeting(&pool, "fin")
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transcript_chunk_embeddings WHERE meeting_id = 'fin'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn failing_backend_is_reported_and_cooled_down() {
        let pool = test_pool().await;
        add(&pool, "m", &["Budget talk."]).await;
        let embedder = FakeEmbedder { vocab: vec!["budget"], fail: true };
        assert!(backfill(&pool, &embedder).await.is_err());
        assert!(recent_failure("fake:failing").is_some());
        let err = semantic_candidates(&pool, &embedder, "budget", &AskFilters::default(), 5)
            .await
            .unwrap_err();
        assert!(err.contains("backend down"));
    }

    #[tokio::test]
    async fn embedder_resolves_only_for_ollama() {
        let pool = test_pool().await;
        assert!(matches!(resolve_embedder(&pool).await, Err(SemanticAvailability::Off(_))));
        SettingsRepository::save_model_config(&pool, "ollama", "llama3", "large-v3", Some("http://host:1/"))
            .await
            .unwrap();
        let e = resolve_embedder(&pool).await.ok().unwrap();
        assert_eq!(e.model_id(), "ollama:nomic-embed-text");
        SettingsRepository::set_setting(&pool, KEY_ASK_EMBEDDING_MODEL, &"").await.unwrap();
        assert!(matches!(resolve_embedder(&pool).await, Err(SemanticAvailability::Off(_))));
        SettingsRepository::save_model_config(&pool, "claude", "x", "large-v3", None).await.unwrap();
        assert!(matches!(resolve_embedder(&pool).await, Err(SemanticAvailability::Off(_))));
    }
}
