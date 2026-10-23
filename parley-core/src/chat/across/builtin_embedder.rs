//! The built-in embedding backend: nomic-embed-text v1.5 run by the bundled
//! `llama-helper` sidecar, so semantic search works without Ollama.
//!
//! The model (a ~146 MB GGUF) is downloaded once in the background at
//! startup, or on first use, into `<app data>/models/embeddings/`. Embedding
//! requests go to a sidecar of their own (a second [`SidecarManager`]), so
//! indexing never evicts the summary model from the generation sidecar or
//! waits behind a long summary. It exits after the usual idle timeout.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use llama_protocol::{Request, Response};
use tokio::sync::Mutex;

use super::embeddings::{EmbedFuture, EmbedKind, Embedder};
use crate::summary::summary_engine::sidecar::SidecarManager;

/// Stored with every vector as `builtin:<name>`.
pub const MODEL_NAME: &str = "nomic-embed-text-v1.5";
pub const MODEL_FILE: &str = "nomic-embed-text-v1.5.Q8_0.gguf";
const MODEL_URL: &str =
    "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/main/nomic-embed-text-v1.5.Q8_0.gguf";
/// Exact size of [`MODEL_FILE`], to tell a finished download from a partial
/// one.
const MODEL_BYTES: u64 = 146_146_432;
/// One batch of transcript chunks on a slow CPU, with room to spare.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Where the model lives: `<app data>/models/embeddings/<file>`.
pub fn model_path() -> Option<PathBuf> {
    crate::paths::app_data_dir()
        .ok()
        .map(|dir| dir.join("models").join("embeddings").join(MODEL_FILE))
}

/// Whether the model is fully downloaded.
pub fn model_is_ready() -> bool {
    model_path()
        .and_then(|p| std::fs::metadata(p).ok())
        .is_some_and(|m| m.len() == MODEL_BYTES)
}

static DOWNLOADING: AtomicBool = AtomicBool::new(false);

/// Whether a download is in progress.
pub fn model_is_downloading() -> bool {
    DOWNLOADING.load(Ordering::SeqCst)
}

/// Download the model unless it's already there (single-flight: a second
/// caller returns immediately while one is running). Written to a `.part`
/// file and renamed, so an interrupted download is never mistaken for the
/// model.
pub async fn ensure_model() -> Result<PathBuf, String> {
    let path = model_path().ok_or("app data directory not configured")?;
    if model_is_ready() {
        return Ok(path);
    }
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("the search model is still downloading".into());
    }
    download_claimed(path).await
}

/// The download itself, once the caller has set [`DOWNLOADING`]; clears it
/// when done.
async fn download_claimed(path: PathBuf) -> Result<PathBuf, String> {
    let result = async {
        let part = path.with_extension("gguf.part");
        log::info!("Downloading the semantic search model (~146 MB) → {}", path.display());
        crate::utils::download_file_to(MODEL_URL, &part)
            .await
            .map_err(|e| e.to_string())?;
        let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if size != MODEL_BYTES {
            let _ = std::fs::remove_file(&part);
            return Err(format!("search model download was incomplete ({size} of {MODEL_BYTES} bytes)"));
        }
        std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
        log::info!("Semantic search model ready");
        Ok(path.clone())
    }
    .await;
    DOWNLOADING.store(false, Ordering::SeqCst);
    result
}

/// Start [`ensure_model`] in the background if the model is missing. Needs
/// a tokio runtime.
pub fn spawn_model_download() {
    // Unit tests resolve embedders too; they must never fetch 146 MB.
    if cfg!(test) || model_is_ready() || model_is_downloading() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let Some(path) = model_path() else {
        return;
    };
    // Claim the flag before spawning, so [`model_is_downloading`] is true
    // as soon as this returns (the Ask page polls on it straight away).
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return;
    }
    handle.spawn(async move {
        if let Err(e) = download_claimed(path).await {
            log::warn!("Semantic search model download failed: {e}");
        }
    });
}

static SIDECAR: LazyLock<Mutex<Option<Arc<SidecarManager>>>> = LazyLock::new(|| Mutex::new(None));

async fn sidecar() -> Result<Arc<SidecarManager>, String> {
    let mut slot = SIDECAR.lock().await;
    if let Some(manager) = slot.as_ref() {
        return Ok(manager.clone());
    }
    let dir = crate::paths::app_data_dir()?;
    let manager = Arc::new(SidecarManager::new(dir).map_err(|e| e.to_string())?);
    *slot = Some(manager.clone());
    Ok(manager)
}

/// Stop the embedding sidecar (app exit).
pub async fn shutdown() {
    if let Some(manager) = SIDECAR.lock().await.take() {
        if let Err(e) = manager.shutdown().await {
            log::warn!("Failed to stop the embedding sidecar: {e}");
        }
    }
}

/// nomic-embed-text's task prefixes; it retrieves much better with them.
pub fn prefixed(text: &str, kind: EmbedKind) -> String {
    match kind {
        EmbedKind::Query => format!("search_query: {text}"),
        EmbedKind::Document => format!("search_document: {text}"),
    }
}

/// Embeddings from the bundled llama.cpp sidecar.
pub struct BuiltinEmbedder;

impl Embedder for BuiltinEmbedder {
    fn model_id(&self) -> String {
        format!("builtin:{MODEL_NAME}")
    }

    fn embed<'a>(&'a self, texts: &'a [String], kind: EmbedKind) -> EmbedFuture<'a> {
        Box::pin(async move {
            let path = ensure_model().await?;
            let manager = sidecar().await?;
            SidecarManager::ensure_running(&manager, path.clone())
                .await
                .map_err(|e| format!("couldn't start the embedding helper: {e}"))?;
            let request = Request::Embed {
                model_path: path.to_string_lossy().into_owned(),
                texts: texts.iter().map(|t| prefixed(t, kind)).collect(),
            };
            match manager.send_request(&request, REQUEST_TIMEOUT, None).await {
                Ok(Response::Embeddings { vectors }) if vectors.len() == texts.len() => Ok(vectors),
                Ok(Response::Embeddings { vectors }) => Err(format!(
                    "the embedding helper returned {} vectors for {} texts",
                    vectors.len(),
                    texts.len()
                )),
                Ok(Response::Error { message }) => Err(message),
                Ok(other) => Err(format!("unexpected embedding helper response: {other:?}")),
                Err(e) => Err(format!("embedding helper failed: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_match_nomics_task_names() {
        assert_eq!(prefixed("cost", EmbedKind::Query), "search_query: cost");
        assert_eq!(prefixed("Sam: hi", EmbedKind::Document), "search_document: Sam: hi");
    }

    #[test]
    fn model_id_is_distinct_from_ollamas() {
        assert_eq!(BuiltinEmbedder.model_id(), "builtin:nomic-embed-text-v1.5");
    }
}
