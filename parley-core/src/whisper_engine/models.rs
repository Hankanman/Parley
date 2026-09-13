use crate::config::WHISPER_MODEL_CATALOG;
use crate::events::{EventSinkExt, SharedEventSink};
use crate::whisper_engine::{ModelInfo, WhisperEngine};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// Global whisper engine
pub static WHISPER_ENGINE: Mutex<Option<Arc<WhisperEngine>>> = Mutex::new(None);

// Global models directory path (set during app initialization)
static MODELS_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Initialize the models directory path using app_data_dir
/// This should be called during app setup before whisper_init
pub fn set_models_directory() {
    let app_data_dir = crate::paths::app_data_dir().expect("Failed to get app data dir");

    let models_dir = app_data_dir.join("models");

    // Create directory if it doesn't exist
    if !models_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&models_dir) {
            log::error!("Failed to create models directory: {}", e);
            return;
        }
    }

    log::info!("Models directory set to: {}", models_dir.display());

    let mut guard = MODELS_DIR.lock().unwrap();
    *guard = Some(models_dir);
}

/// Get the configured models directory
pub fn get_models_directory() -> Option<PathBuf> {
    MODELS_DIR.lock().unwrap().clone()
}

pub async fn whisper_init() -> Result<(), String> {
    let mut guard = WHISPER_ENGINE.lock().unwrap();
    if guard.is_some() {
        return Ok(());
    }

    let models_dir = get_models_directory();
    let engine = WhisperEngine::new_with_models_dir(models_dir)
        .map_err(|e| format!("Failed to initialize whisper engine: {}", e))?;
    *guard = Some(Arc::new(engine));
    Ok(())
}

pub async fn whisper_get_available_models() -> Result<Vec<ModelInfo>, String> {
    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap();
        guard.as_ref().cloned()
    };

    if let Some(engine) = engine {
        engine
            .discover_models()
            .await
            .map_err(|e| format!("Failed to discover models: {}", e))
    } else {
        // Fallback: scan models directory directly without initialized engine
        log::info!("Whisper engine not initialized, scanning models directory directly");
        discover_models_standalone()
    }
}

/// Discover Whisper models by scanning the models directory directly.
/// Used when the Whisper engine isn't initialized yet (e.g., during onboarding
/// status checks before any model has been loaded).
///
/// Delegates the actual per-file corruption/size check to
/// `whisper_engine::scan_catalog_entry` - the same logic
/// `WhisperEngine::discover_models` uses - so this fallback path reports
/// `Corrupted` models identically instead of only ever seeing
/// `Available`/`Missing`. There's no engine here to consult for
/// currently-downloading state, so `downloading_progress` is always `None`
/// (an undersized file just reads as `Corrupted`, same as it would for the
/// real engine when it has no in-memory record of that model either).
pub fn discover_models_standalone() -> Result<Vec<ModelInfo>, String> {
    use crate::whisper_engine::scan_catalog_entry;

    let models_dir =
        get_models_directory().ok_or_else(|| "Models directory not initialized".to_string())?;

    // Whisper models are stored directly in the models directory (not in a whisper subdirectory)
    let whisper_dir = models_dir.clone();

    log::info!("Scanning for Whisper models in: {}", whisper_dir.display());

    // Use centralized model catalog from config.rs
    let model_configs = WHISPER_MODEL_CATALOG;

    let mut models = Vec::new();

    for &(name, filename, size_mb, accuracy, speed, description) in model_configs {
        let status = scan_catalog_entry(&whisper_dir, filename, size_mb, None);

        models.push(ModelInfo {
            name: name.to_string(),
            path: whisper_dir.join(filename),
            size_mb,
            status,
            accuracy: accuracy.to_string(),
            speed: speed.to_string(),
            description: description.to_string(),
        });
    }

    let downloaded_count = models
        .iter()
        .filter(|m| matches!(m.status, crate::whisper_engine::ModelStatus::Available))
        .count();
    log::info!("Found {} downloaded Whisper models", downloaded_count);

    Ok(models)
}

pub async fn whisper_has_available_models() -> Result<bool, String> {
    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap();
        guard.as_ref().cloned()
    };

    if let Some(engine) = engine {
        let models = engine
            .discover_models()
            .await
            .map_err(|e| format!("Failed to discover models: {}", e))?;

        // Check if at least one model is available
        let available_models: Vec<_> = models
            .iter()
            .filter(|model| matches!(model.status, crate::whisper_engine::ModelStatus::Available))
            .collect();

        Ok(!available_models.is_empty())
    } else {
        Ok(false)
    }
}

/// Ensure a Whisper model is loaded, resolving which one from the saved
/// transcript config, and return its name. Reads the config straight from
/// the DB pool (`provider`/`model` only — no API key needed here) rather
/// than going through the `api_get_transcript_config` command, mirroring
/// `audio::transcription::engine::get_or_init_whisper`. `pool` is `None`
/// when `AppState` isn't managed yet (first-launch cold start), which is
/// treated the same as "no saved config".
pub async fn whisper_validate_model_ready_with_config(
    pool: Option<&sqlx::SqlitePool>,
) -> Result<String, String> {
    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap();
        guard.as_ref().cloned()
    };

    if let Some(engine) = engine {
        // Check if a model is currently loaded
        if engine.is_model_loaded().await {
            if let Some(current_model) = engine.get_current_model().await {
                log::info!("Model already loaded: {}", current_model);
                return Ok(current_model);
            }
        }

        // No model loaded - try to load user's configured model from transcript config
        let model_to_load = match pool {
            Some(pool) => match crate::database::repositories::setting::SettingsRepository::get_transcript_config(pool).await {
                Ok(Some(config)) => {
                    log::info!(
                        "Got transcript config from DB - provider: {}, model: {}",
                        config.provider,
                        config.model
                    );
                    if config.provider == "localWhisper" && !config.model.is_empty() {
                        log::info!("Using user's configured model: {}", config.model);
                        Some(config.model)
                    } else {
                        log::info!(
                            "Saved config uses non-local provider ({}) or empty model, will auto-select",
                            config.provider
                        );
                        None
                    }
                }
                Ok(None) => {
                    log::info!("No transcript config found in DB, will auto-select model");
                    None
                }
                Err(e) => {
                    log::warn!(
                        "Failed to get transcript config from DB: {}, will auto-select model",
                        e
                    );
                    None
                }
            },
            None => {
                log::warn!("No DB pool yet; will auto-select model");
                None
            }
        };

        // Check available models
        let models = engine
            .discover_models()
            .await
            .map_err(|e| format!("Failed to discover models: {}", e))?;

        let available_models: Vec<_> = models
            .iter()
            .filter(|model| matches!(model.status, crate::whisper_engine::ModelStatus::Available))
            .collect();

        if available_models.is_empty() {
            return Err(
                "No Whisper models are available. Please download a model to enable transcription."
                    .to_string(),
            );
        }

        // Try to load user's configured model if specified
        let model_name = if let Some(configured_model) = model_to_load {
            // Check if configured model is available
            if available_models.iter().any(|m| m.name == configured_model) {
                log::info!("Loading user's configured model: {}", configured_model);
                configured_model
            } else {
                log::warn!(
                    "Configured model '{}' not found, falling back to first available: {}",
                    configured_model,
                    available_models[0].name
                );
                available_models[0].name.clone()
            }
        } else {
            // No configured model, use first available
            log::info!(
                "No configured model, loading first available: {}",
                available_models[0].name
            );
            available_models[0].name.clone()
        };

        engine
            .load_model(&model_name)
            .await
            .map_err(|e| format!("Failed to load model {}: {}", model_name, e))?;

        Ok(model_name)
    } else {
        Err("Whisper engine not initialized".to_string())
    }
}

pub async fn whisper_get_models_directory() -> Result<String, String> {
    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap();
        guard.as_ref().cloned()
    };

    if let Some(engine) = engine {
        let path = engine.get_models_directory().await;
        Ok(path.to_string_lossy().to_string())
    } else {
        Err("Whisper engine not initialized".to_string())
    }
}

/// Download `model_name`, reporting progress through `sink` via
/// `model-download-progress` / `model-download-complete` /
/// `model-download-error` — the same three events the command previously
/// emitted directly through the `AppHandle`.
pub async fn download_model_with_progress(
    sink: SharedEventSink,
    model_name: String,
) -> Result<(), String> {
    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap();
        guard.as_ref().cloned()
    };

    let Some(engine) = engine else {
        return Err("Whisper engine not initialized".to_string());
    };

    // Create progress callback that emits events
    let progress_sink = sink.clone();
    let model_name_clone = model_name.clone();

    let progress_callback = Box::new(move |progress: u8| {
        log::info!("Download progress for {}: {}%", model_name_clone, progress);

        // Emit download progress event
        if let Err(e) = progress_sink.emit_event(
            "model-download-progress",
            &serde_json::json!({
                "modelName": model_name_clone,
                "progress": progress
            }),
        ) {
            log::error!("Failed to emit download progress event: {}", e);
        }
    });

    let result = engine
        .download_model(&model_name, Some(progress_callback))
        .await;

    match result {
        Ok(()) => {
            // Emit completion event
            if let Err(e) = sink.emit_event(
                "model-download-complete",
                &serde_json::json!({
                    "modelName": model_name
                }),
            ) {
                log::error!("Failed to emit download complete event: {}", e);
            }
            Ok(())
        }
        Err(e) => {
            // Emit error event
            if let Err(emit_e) = sink.emit_event(
                "model-download-error",
                &serde_json::json!({
                    "modelName": model_name,
                    "error": e.to_string()
                }),
            ) {
                log::error!("Failed to emit download error event: {}", emit_e);
            }
            Err(format!("Failed to download model: {}", e))
        }
    }
}

pub async fn whisper_cancel_download(model_name: String) -> Result<(), String> {
    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap();
        guard.as_ref().cloned()
    };

    if let Some(engine) = engine {
        engine
            .cancel_download(&model_name)
            .await
            .map_err(|e| format!("Failed to cancel download: {}", e))
    } else {
        Err("Whisper engine not initialized".to_string())
    }
}

pub async fn whisper_delete_corrupted_model(model_name: String) -> Result<String, String> {
    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap();
        guard.as_ref().cloned()
    };

    if let Some(engine) = engine {
        engine
            .delete_model(&model_name)
            .await
            .map_err(|e| format!("Failed to delete model: {}", e))
    } else {
        Err("Whisper engine not initialized".to_string())
    }
}

/// Open the models folder in the system file explorer
pub fn open_models_folder_path() -> Result<String, String> {
    let models_dir =
        get_models_directory().ok_or_else(|| "Models directory not initialized".to_string())?;

    // Ensure directory exists before trying to open it
    if !models_dir.exists() {
        std::fs::create_dir_all(&models_dir)
            .map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    let folder_path = models_dir.to_string_lossy().to_string();

    std::process::Command::new("xdg-open")
        .arg(&folder_path)
        .spawn()
        .map_err(|e| format!("Failed to open folder: {}", e))?;

    log::info!("Opened models folder: {}", folder_path);
    Ok(folder_path)
}
