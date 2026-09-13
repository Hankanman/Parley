// High-level client API for built-in AI summary generation
// Provides simple interface for generating text using the sidecar

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{anyhow, Result};
use llama_protocol::{Request, Response};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::models;
use super::sidecar::SidecarManager;
use crate::summary::processor::rough_token_count;

/// Pick the smallest power-of-two context window that holds the prompt plus
/// generation budget plus a safety buffer, capped at the model's max context.
///
/// Why: built-in AI runs on a user's GPU. Asking llama.cpp for the full 32k
/// context unconditionally pre-allocates ~4 GB of KV cache and a compute
/// buffer sized for the full window — that fails with cudaMalloc OOM on
/// modest GPUs (e.g. RTX 30xx-class) even when the actual prompt is a few
/// thousand tokens. Sizing n_ctx to what the request actually needs avoids
/// the OOM without changing user-visible behavior.
fn pick_context_size(prompt_tokens: usize, max_tokens: i32, model_max_ctx: u32) -> u32 {
    const SAFETY_BUFFER_TOKENS: usize = 1024;
    const MIN_CTX: u32 = 4096;

    let needed = prompt_tokens
        .saturating_add(max_tokens.max(0) as usize)
        .saturating_add(SAFETY_BUFFER_TOKENS);

    let mut ctx = MIN_CTX;
    while (ctx as usize) < needed && ctx < model_max_ctx {
        ctx = ctx.saturating_mul(2);
    }
    ctx.min(model_max_ctx)
}

// Request/Response types are shared with the sidecar via the llama-protocol
// crate — see its docs for the wire format.

// ============================================================================
// Global Sidecar Manager
// ============================================================================

static SIDECAR_MANAGER: LazyLock<Arc<Mutex<Option<Arc<SidecarManager>>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));

/// Initialize the global sidecar manager
pub async fn init_sidecar_manager(app_data_dir: PathBuf) -> Result<()> {
    let manager = SidecarManager::new(app_data_dir)?;
    let mut global_manager = SIDECAR_MANAGER.lock().await;
    *global_manager = Some(Arc::new(manager));
    Ok(())
}

/// Get the global sidecar manager
async fn get_sidecar_manager() -> Result<Arc<SidecarManager>> {
    let global_manager = SIDECAR_MANAGER.lock().await;
    global_manager
        .clone()
        .ok_or_else(|| anyhow!("Sidecar manager not initialized. Call init_sidecar_manager first."))
}

/// Resolve a model name to its on-disk GGUF path, erroring if the file
/// isn't downloaded yet.
fn resolve_model_path(app_data_dir: &PathBuf, model_name: &str) -> Result<PathBuf> {
    let model_path = models::get_model_path(app_data_dir, model_name)?;
    if !model_path.exists() {
        return Err(anyhow!(
            "Model file not found: {}. Please download the model '{}' first.",
            model_path.display(),
            model_name
        ));
    }
    Ok(model_path)
}

// ============================================================================
// Public API
// ============================================================================

/// Generate text using built-in AI
///
/// # Arguments
/// * `app_data_dir` - Application data directory (for model resolution)
/// * `model_name` - Model name (e.g., "gemma3:1b")
/// * `system_prompt` - System instructions for the model
/// * `user_prompt` - User message/task
/// * `cancellation_token` - Optional token for cancellation
/// * `on_delta` - Optional callback receiving incremental output text as
///   the sidecar streams it (the returned String stays authoritative)
///
/// # Returns
/// Generated text
pub async fn generate_with_builtin(
    app_data_dir: &PathBuf,
    model_name: &str,
    system_prompt: &str,
    user_prompt: &str,
    cancellation_token: Option<&CancellationToken>,
    on_delta: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<String> {
    // Check cancellation at start
    if let Some(token) = cancellation_token {
        if token.is_cancelled() {
            return Err(anyhow!("Generation cancelled before starting"));
        }
    }

    log::info!("Built-in AI generation request");
    log::info!("Model: {}", model_name);

    // Get model definition
    let model_def = models::get_model_by_name(model_name)
        .ok_or_else(|| anyhow!("Unknown model: {}", model_name))?;

    let model_path = resolve_model_path(app_data_dir, model_name)?;

    // Apply model-specific chat template
    let formatted_prompt = models::format_prompt(&model_def.template, system_prompt, user_prompt)?;
    // Get or initialize sidecar manager
    let manager = {
        let mut global_manager = SIDECAR_MANAGER.lock().await;
        if global_manager.is_none() {
            log::info!("Initializing sidecar manager");
            let new_manager = SidecarManager::new(app_data_dir.clone())?;
            *global_manager = Some(Arc::new(new_manager));
        }
        global_manager.clone().unwrap()
    };

    // Ensure sidecar is running with this model
    SidecarManager::ensure_running(&manager, model_path.clone()).await?;

    // Check cancellation after sidecar startup
    if let Some(token) = cancellation_token {
        if token.is_cancelled() {
            return Err(anyhow!("Generation cancelled during sidecar startup"));
        }
    }

    // Size the llama context to what this request actually needs, instead of
    // always asking for the model's max. See pick_context_size doc comment.
    let prompt_tokens = rough_token_count(&formatted_prompt);
    let chosen_ctx = pick_context_size(
        prompt_tokens,
        models::DEFAULT_MAX_TOKENS,
        model_def.context_size,
    );
    log::info!(
        "Sized llama_context: prompt={} tok, max_gen={} tok, ctx={} (model max {})",
        prompt_tokens,
        models::DEFAULT_MAX_TOKENS,
        chosen_ctx,
        model_def.context_size,
    );

    // Prepare generation request with model-specific sampling parameters
    let request = Request::Generate {
        prompt: formatted_prompt,
        max_tokens: Some(models::DEFAULT_MAX_TOKENS),
        context_size: Some(chosen_ctx),
        model_path: Some(model_path.to_string_lossy().to_string()),
        temperature: Some(model_def.sampling.temperature),
        top_k: Some(model_def.sampling.top_k),
        top_p: Some(model_def.sampling.top_p),
        stop_tokens: Some(model_def.sampling.stop_tokens.clone()),
    };

    // Send request with timeout
    let timeout = Duration::from_secs(models::GENERATION_TIMEOUT_SECS);

    log::info!("Sending generation request to sidecar");

    // Race between send_request and cancellation token
    let response = if let Some(token) = cancellation_token {
        tokio::select! {
            result = manager.send_request(&request, timeout, on_delta) => {
                result?
            }
            _ = token.cancelled() => {
                log::warn!("Generation cancelled by user, shutting down sidecar");
                // Shutdown sidecar to stop generation immediately
                if let Err(e) = manager.shutdown().await {
                    log::error!("Failed to shutdown sidecar during cancellation: {}", e);
                }
                return Err(anyhow!("Generation cancelled by user"));
            }
        }
    } else {
        manager.send_request(&request, timeout, on_delta).await?
    };

    // Check cancellation before returning
    if let Some(token) = cancellation_token {
        if token.is_cancelled() {
            return Err(anyhow!("Generation cancelled"));
        }
    }

    match response {
        Response::Response { text, error } => {
            if let Some(err_msg) = error {
                Err(anyhow!("Generation failed: {}", err_msg))
            } else {
                log::info!("Generation completed: {} chars", text.len());
                Ok(text)
            }
        }
        Response::Error { message } => Err(anyhow!("Sidecar error: {}", message)),
        other => Err(anyhow!(
            "Unexpected sidecar response to generate request: {:?}",
            other
        )),
    }
}

/// Shutdown the global sidecar (graceful cleanup)
/// Detaches the current manager and spawns a background task to drain active requests
pub async fn shutdown_sidecar_gracefully() -> Result<()> {
    let manager_opt = {
        let mut global_manager = SIDECAR_MANAGER.lock().await;
        global_manager.take()
    };

    if let Some(manager) = manager_opt {
        log::info!("Detaching sidecar manager for graceful shutdown");

        // Spawn background task to wait for active requests and then kill
        tokio::spawn(async move {
            if let Err(e) = manager.shutdown_gracefully().await {
                log::error!("Error during graceful shutdown: {}", e);
            }
        });
    }

    Ok(())
}

/// Force shutdown the global sidecar (for app exit)
/// Directly kills the process without waiting for active requests to complete.
/// This is synchronous and blocks until the sidecar is terminated.
pub async fn force_shutdown_sidecar() -> Result<()> {
    let manager_opt = {
        let mut global_manager = SIDECAR_MANAGER.lock().await;
        global_manager.take()
    };

    if let Some(manager) = manager_opt {
        log::info!("Force shutting down sidecar for app exit");
        // Call shutdown() directly - sends shutdown command and force kills after 3s
        manager.shutdown().await?;
    }

    Ok(())
}

/// Check if sidecar is healthy
pub async fn is_sidecar_healthy() -> bool {
    if let Ok(manager) = get_sidecar_manager().await {
        manager.is_healthy()
    } else {
        false
    }
}

// Wire-format serialization tests live in the llama-protocol crate,
// alongside the shared types they exercise.
