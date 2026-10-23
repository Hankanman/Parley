//! Semantic search over the transcript-chunk embeddings the app stores for
//! Ask Parley (`transcript_chunk_embeddings`; see
//! `parley_core::chat::across::embeddings`).
//!
//! This server only *reads* the index: the app builds and refreshes it in
//! the background whenever Ask Parley is open. Searching embeds the query
//! with the same model, picked by the app's rules
//! (`parley_core::chat::across::embeddings::resolve_embedder`):
//!
//! * the user's Ollama server when Ollama is the summary provider, with the
//!   `ask_embedding_model` setting (default `nomic-embed-text`);
//! * otherwise the built-in nomic-embed-text GGUF the app downloads, run by
//!   the bundled `llama-helper` (see [`find_helper`]).
//!
//! An empty `ask_embedding_model` turns semantic search off either way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use serde::Serialize;
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::db::format_offset;

const KEY_ASK_EMBEDDING_MODEL: &str = "ask_embedding_model";
/// The built-in model, as `parley_core::chat::across::builtin_embedder`
/// names, stores and sizes it.
const BUILTIN_MODEL_NAME: &str = "nomic-embed-text-v1.5";
const BUILTIN_MODEL_FILE: &str = "nomic-embed-text-v1.5.Q8_0.gguf";
const BUILTIN_MODEL_BYTES: u64 = 146_146_432;
/// Written by the app: the AppImage it last ran from.
const APPIMAGE_PATH_FILE: &str = "appimage-path";
/// Starting the helper (and mounting the AppImage) plus loading the model.
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";
const DEFAULT_OLLAMA_ENDPOINT: &str = "http://localhost:11434";
/// Below this cosine similarity a chunk isn't related at all (as in the app).
const MIN_SIMILARITY: f32 = 0.2;
/// Longest excerpt returned per hit, in characters.
const EXCERPT_CHARS: usize = 1200;

/// Where the query gets embedded.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbeddingBackend {
    Ollama { endpoint: String, model: String },
    Builtin { model_path: PathBuf },
}

impl EmbeddingBackend {
    /// The id stored with every vector, e.g. `ollama:nomic-embed-text`.
    pub fn model_id(&self) -> String {
        match self {
            EmbeddingBackend::Ollama { model, .. } => format!("ollama:{model}"),
            EmbeddingBackend::Builtin { .. } => format!("builtin:{BUILTIN_MODEL_NAME}"),
        }
    }

    /// nomic-embed-text retrieves much better with its task prefix.
    fn query_prefix(&self) -> &'static str {
        match self {
            EmbeddingBackend::Ollama { model, .. } if !model.contains("nomic-embed") => "",
            _ => "search_query: ",
        }
    }
}

/// The embedding backend for the current settings, or why there isn't one.
/// `data_dir` is Parley's app data directory (the database's folder).
pub async fn resolve_backend(pool: &SqlitePool, data_dir: &Path) -> Result<EmbeddingBackend, String> {
    let config: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT provider, ollamaEndpoint FROM settings LIMIT 1")
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("couldn't read the model settings: {e}"))?;
    let (provider, endpoint) = config.unwrap_or((None, None));
    let setting: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = ?")
            .bind(KEY_ASK_EMBEDDING_MODEL)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("couldn't read the embedding model setting: {e}"))?;
    let setting = setting
        .and_then(|json| serde_json::from_str::<String>(&json).ok())
        .map(|m| m.trim().to_string());
    if setting.as_deref() == Some("") {
        return Err("semantic search is turned off in Parley's settings; use `search_transcripts`".into());
    }

    if provider.is_some_and(|p| p.trim().eq_ignore_ascii_case("ollama")) {
        let endpoint = endpoint
            .map(|e| e.trim().trim_end_matches('/').to_string())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_string());
        let model = setting.unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());
        return Ok(EmbeddingBackend::Ollama { endpoint, model });
    }

    let model_path = data_dir.join("models").join("embeddings").join(BUILTIN_MODEL_FILE);
    let ready = std::fs::metadata(&model_path).is_ok_and(|m| m.len() == BUILTIN_MODEL_BYTES);
    if !ready {
        return Err("Parley hasn't finished downloading its semantic search model yet (it does so \
                    in the background after launch); use `search_transcripts` meanwhile"
            .into());
    }
    Ok(EmbeddingBackend::Builtin { model_path })
}

/// Embed `query`, L2-normalised.
pub async fn embed_query(
    backend: &EmbeddingBackend,
    data_dir: &Path,
    query: &str,
) -> Result<Vec<f32>, String> {
    let text = format!("{}{}", backend.query_prefix(), query);
    let mut vector = match backend {
        EmbeddingBackend::Ollama { endpoint, model } => embed_with_ollama(endpoint, model, &text).await?,
        EmbeddingBackend::Builtin { model_path } => {
            let helper = find_helper(data_dir).ok_or(
                "couldn't find Parley's llama.cpp helper: run Parley once (from the AppImage) \
                 so parley-mcp can find it, or set $PARLEY_LLAMA_HELPER",
            )?;
            embed_with_helper(&helper, model_path, &text).await?
        }
    };
    normalize(&mut vector);
    Ok(vector)
}

/// How to start `llama-helper`.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl HelperCommand {
    fn direct(program: PathBuf) -> Self {
        Self { program, args: vec![] }
    }

    /// The Parley AppImage (or binary), told to run its bundled helper.
    fn via_parley(program: PathBuf) -> Self {
        Self {
            program,
            args: vec!["--llama-helper".into()],
        }
    }
}

/// Find `llama-helper`, in order: `$PARLEY_LLAMA_HELPER`; a `llama-helper`
/// next to this binary (a source build's `target/`); the `parley` AppImage
/// next to this binary (`install.sh` puts both in `~/.local/bin`); the
/// AppImage the app last ran from (recorded in its data dir); the build tree
/// this binary came from.
pub fn find_helper(data_dir: &Path) -> Option<HelperCommand> {
    if let Some(path) = std::env::var_os("PARLEY_LLAMA_HELPER").map(PathBuf::from) {
        if path.is_file() {
            return Some(HelperCommand::direct(path));
        }
    }
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    if let Some(dir) = &exe_dir {
        let helper = dir.join("llama-helper");
        if helper.is_file() {
            return Some(HelperCommand::direct(helper));
        }
        let parley = dir.join("parley");
        if parley.is_file() {
            return Some(HelperCommand::via_parley(parley));
        }
    }
    if let Ok(recorded) = std::fs::read_to_string(data_dir.join(APPIMAGE_PATH_FILE)) {
        let path = PathBuf::from(recorded.trim());
        if path.is_file() {
            return Some(HelperCommand::via_parley(path));
        }
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    ["release", "debug"]
        .iter()
        .map(|profile| workspace.join("target").join(profile).join("llama-helper"))
        .find(|p| p.is_file())
        .map(HelperCommand::direct)
}

/// Run one `Embed` request through a fresh helper process.
async fn embed_with_helper(helper: &HelperCommand, model_path: &Path, text: &str) -> Result<Vec<f32>, String> {
    use llama_protocol::{Request, Response};

    let mut child = tokio::process::Command::new(&helper.program)
        .args(&helper.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", helper.program.display()))?;
    let mut stdin = child.stdin.take().ok_or("helper has no stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("helper has no stdout")?);

    let request = Request::Embed {
        model_path: model_path.to_string_lossy().into_owned(),
        texts: vec![text.to_string()],
    };
    let exchange = async {
        let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())?;
        loop {
            let mut reply = String::new();
            if stdout.read_line(&mut reply).await.map_err(|e| e.to_string())? == 0 {
                return Err("the helper exited without answering".to_string());
            }
            match serde_json::from_str::<Response>(reply.trim()) {
                Ok(Response::Embeddings { mut vectors }) if !vectors.is_empty() => return Ok(vectors.swap_remove(0)),
                Ok(Response::Error { message }) => return Err(message),
                Ok(other) => return Err(format!("unexpected helper response: {other:?}")),
                // Not a protocol line; keep reading.
                Err(_) => continue,
            }
        }
    };
    let result = tokio::time::timeout(HELPER_TIMEOUT, exchange)
        .await
        .unwrap_or_else(|_| Err("the helper took too long".to_string()));
    let _ = stdin.write_all(b"{\"type\":\"shutdown\"}\n").await;
    drop(stdin);
    if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
    result
}

/// Embed `text` with Ollama's `/api/embed`.
async fn embed_with_ollama(endpoint: &str, model: &str, text: &str) -> Result<Vec<f32>, String> {
    #[derive(serde::Deserialize)]
    struct Response {
        embeddings: Vec<Vec<f32>>,
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("{endpoint}/api/embed"))
        .json(&serde_json::json!({ "model": model, "input": [text] }))
        .send()
        .await
        .map_err(|e| format!("couldn't reach Ollama at {endpoint}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Ollama returned {status} (is `{model}` pulled?): {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    response
        .json::<Response>()
        .await
        .map_err(|e| format!("bad Ollama embeddings response: {e}"))?
        .embeddings
        .into_iter()
        .next()
        .ok_or_else(|| "Ollama returned no embedding".to_string())
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
        let data = tempfile::tempdir().unwrap();

        // Built-in by default, once the app has downloaded the model.
        assert!(resolve_backend(&pool, data.path()).await.unwrap_err().contains("downloading"));
        let model = data.path().join("models/embeddings").join(BUILTIN_MODEL_FILE);
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        std::fs::File::create(&model).unwrap().set_len(BUILTIN_MODEL_BYTES).unwrap();
        let backend = resolve_backend(&pool, data.path()).await.unwrap();
        assert_eq!(backend.model_id(), "builtin:nomic-embed-text-v1.5");
        assert_eq!(backend.query_prefix(), "search_query: ");

        sqlx::query("INSERT INTO settings (id, provider, model, whisperModel, ollamaEndpoint) VALUES ('1', 'ollama', 'gemma3:4b', 'x', 'http://gpu-box:11434/')")
            .execute(&pool).await.unwrap();
        let backend = resolve_backend(&pool, data.path()).await.unwrap();
        assert_eq!(
            backend,
            EmbeddingBackend::Ollama { endpoint: "http://gpu-box:11434".into(), model: "nomic-embed-text".into() }
        );

        sqlx::query("INSERT INTO app_settings (key, value, updated_at) VALUES ('ask_embedding_model', '\"\"', 'x')")
            .execute(&pool).await.unwrap();
        assert!(resolve_backend(&pool, data.path()).await.unwrap_err().contains("turned off"));
    }

    #[test]
    fn find_helper_prefers_the_recorded_appimage_over_the_build_tree() {
        let data = tempfile::tempdir().unwrap();
        let appimage = data.path().join("Parley.AppImage");
        std::fs::write(&appimage, b"").unwrap();
        std::fs::write(data.path().join(APPIMAGE_PATH_FILE), appimage.to_string_lossy().as_bytes()).unwrap();
        // The test binary lives in target/debug/deps, with no helper beside it.
        let helper = find_helper(data.path()).unwrap();
        assert_eq!(helper, HelperCommand::via_parley(appimage));
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
