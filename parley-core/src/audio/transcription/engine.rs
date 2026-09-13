// audio/transcription/engine.rs
//
// TranscriptionEngine enum and model initialization/validation logic.
// Whisper is the sole local ASR engine (a prior remote-provider `Provider`
// variant was removed as dead code — it had no live implementation).

use log::{info, warn};
use std::sync::Arc;

// Transcription engine abstraction.
pub enum TranscriptionEngine {
    Whisper(Arc<crate::whisper_engine::WhisperEngine>), // Local Whisper (direct access)
}

impl TranscriptionEngine {
    pub async fn is_model_loaded(&self) -> bool {
        match self {
            Self::Whisper(engine) => engine.is_model_loaded().await,
        }
    }

    pub async fn get_current_model(&self) -> Option<String> {
        match self {
            Self::Whisper(engine) => engine.get_current_model().await,
        }
    }

    pub fn provider_name(&self) -> &str {
        match self {
            Self::Whisper(_) => "Whisper (direct)",
        }
    }
}

/// Validate that the local Whisper model is ready before recording starts.
pub async fn validate_transcription_model_ready(
    pool: Option<&sqlx::SqlitePool>,
) -> Result<(), String> {
    info!("🔍 Validating Whisper model...");

    if let Err(init_error) = crate::whisper_engine::models::whisper_init().await {
        warn!("❌ Failed to initialize Whisper engine: {}", init_error);
        return Err(format!(
            "Failed to initialize speech recognition: {}",
            init_error
        ));
    }

    match crate::whisper_engine::models::whisper_validate_model_ready_with_config(pool).await {
        Ok(model_name) => {
            info!(
                "✅ Whisper model validation successful: {} is ready",
                model_name
            );
            Ok(())
        }
        Err(e) => {
            warn!("❌ Whisper model validation failed: {}", e);
            Err(e)
        }
    }
}

/// Get or initialize the Whisper transcription engine for live recording.
/// Remote providers (e.g., OpenAI) are not used during the live audio path —
/// the worker pool is currently Whisper-only.
pub async fn get_or_init_transcription_engine(
    pool: Option<&sqlx::SqlitePool>,
) -> Result<TranscriptionEngine, String> {
    info!("🎤 Initializing Whisper transcription engine");
    let whisper_engine = get_or_init_whisper(pool).await?;
    Ok(TranscriptionEngine::Whisper(whisper_engine))
}

/// Get or initialize the Whisper engine, loading the model from saved config.
///
/// Resolution order:
/// 1. The configured `localWhisper` model, if it is downloaded.
/// 2. Otherwise the first downloaded model (with a warning) — the same
///    fallback `whisper_validate_model_ready_with_config` applies before a
///    recording is allowed to start, so the worker can never bail out on a
///    model the validator already tolerated.
///
/// The currently loaded model is only unloaded once the replacement has been
/// resolved to something loadable, so a bad config never leaves the engine
/// empty mid-recording.
pub async fn get_or_init_whisper(
    pool: Option<&sqlx::SqlitePool>,
) -> Result<Arc<crate::whisper_engine::WhisperEngine>, String> {
    let existing_engine = {
        let engine_guard = crate::whisper_engine::models::WHISPER_ENGINE
            .lock()
            .unwrap();
        engine_guard.as_ref().cloned()
    };

    let engine = match existing_engine {
        Some(engine) => engine,
        None => {
            info!("Initializing Whisper engine");
            if let Err(e) = crate::whisper_engine::models::whisper_init().await {
                return Err(format!("Failed to initialize Whisper engine: {}", e));
            }
            let engine_guard = crate::whisper_engine::models::WHISPER_ENGINE
                .lock()
                .unwrap();
            engine_guard
                .as_ref()
                .cloned()
                .ok_or("Failed to get initialized engine")?
        }
    };

    let current_model = if engine.is_model_loaded().await {
        engine.get_current_model().await
    } else {
        None
    };

    // Which model does the saved config ask for (if any)? Reads the setting
    // directly from the pool rather than through `api_get_transcript_config`
    // (a `#[tauri::command]`) — this only needs `provider`/`model`, not the
    // API key that command also resolves. No pool yet (DB still initialising
    // on a first-launch cold start) is treated like "no saved config".
    let saved_config = match pool {
        Some(pool) => {
            crate::database::repositories::setting::SettingsRepository::get_transcript_config(pool)
                .await
        }
        None => {
            warn!("⚠️ No DB pool yet; using the default Whisper model");
            Ok(None)
        }
    };
    let configured_model: Option<String> =
        match saved_config {
            Ok(Some(config)) => {
                info!(
                    "📝 Saved transcript config - provider: {}, model: {}",
                    config.provider, config.model
                );
                if config.provider != "localWhisper" {
                    if let Some(ref loaded) = current_model {
                        info!(
                            "ℹ️ Config uses provider '{}'; reusing loaded model '{}'",
                            config.provider, loaded
                        );
                        return Ok(engine);
                    }
                    return Err(format!(
                        "Cannot initialize Whisper engine: config uses provider '{}'. Local recording requires 'localWhisper'.",
                        config.provider
                    ));
                }
                if config.model.is_empty() {
                    None
                } else {
                    Some(config.model)
                }
            }
            Ok(None) => None,
            Err(e) => {
                warn!("⚠️ Failed to get transcript config: {}", e);
                None
            }
        };

    let configured_model = match (configured_model, &current_model) {
        (Some(model), _) => model,
        (None, Some(loaded)) => {
            info!(
                "✅ No specific model configured, using currently loaded model: '{}'",
                loaded
            );
            return Ok(engine);
        }
        (None, None) => {
            info!("No transcript config found, falling back to 'small'");
            "small".to_string()
        }
    };

    let target_model = resolve_loadable_model(&engine, &configured_model).await?;

    if current_model.as_deref() == Some(target_model.as_str()) {
        info!("✅ Loaded model '{}' matches config, reusing", target_model);
        return Ok(engine);
    }

    if let Some(ref loaded) = current_model {
        info!(
            "🔄 Loaded model '{}' doesn't match resolved model '{}', reloading...",
            loaded, target_model
        );
    }
    engine
        .load_model(&target_model)
        .await
        .map_err(|e| format!("Failed to load model '{}': {}", target_model, e))?;
    info!("✅ Model '{}' loaded successfully", target_model);

    Ok(engine)
}

/// Pick a model that can actually be loaded: `preferred` if it is downloaded,
/// otherwise the first downloaded model. Errors only when nothing is
/// downloaded at all (or the preferred model is mid-download / corrupted and
/// nothing else is available).
async fn resolve_loadable_model(
    engine: &crate::whisper_engine::WhisperEngine,
    preferred: &str,
) -> Result<String, String> {
    use crate::whisper_engine::ModelStatus;

    let models = engine
        .discover_models()
        .await
        .map_err(|e| format!("Failed to discover models: {}", e))?;

    let preferred_info = models.iter().find(|m| m.name == preferred);
    if let Some(model) = preferred_info {
        if matches!(model.status, ModelStatus::Available) {
            return Ok(preferred.to_string());
        }
    }

    let fallback = models
        .iter()
        .find(|m| matches!(m.status, ModelStatus::Available))
        .map(|m| m.name.clone());

    let why = match preferred_info.map(|m| &m.status) {
        None => format!("Model '{}' is not in the catalog", preferred),
        Some(ModelStatus::Missing) => format!("Model '{}' is not downloaded", preferred),
        Some(ModelStatus::Downloading { progress }) => {
            format!("Model '{}' is still downloading ({}%)", preferred, progress)
        }
        Some(ModelStatus::Error(err)) => format!("Model '{}' has an error: {}", preferred, err),
        Some(ModelStatus::Corrupted { .. }) => format!("Model '{}' is corrupted", preferred),
        Some(ModelStatus::Available) => unreachable!("handled above"),
    };

    match fallback {
        Some(name) => {
            warn!("⚠️ {}; falling back to available model '{}'", why, name);
            Ok(name)
        }
        None => Err(format!(
            "{} and no other model is downloaded. Please download a model from settings.",
            why
        )),
    }
}
