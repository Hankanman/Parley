// audio/transcription/engine.rs
//
// TranscriptionEngine enum and model initialization/validation logic.
// Whisper, GigaAM and Parakeet share the transcription pipeline.

use log::{info, warn};
use std::sync::Arc;

// Transcription engine abstraction.
#[derive(Clone)]
pub enum TranscriptionEngine {
    GigaAm(Arc<crate::gigaam::Engine>),
    Parakeet(Arc<crate::parakeet::Engine>),
    Whisper(Arc<crate::whisper_engine::WhisperEngine>), // Local Whisper (direct access)
}

impl TranscriptionEngine {
    pub fn max_segment_samples(&self) -> usize {
        match self {
            Self::Whisper(_) => 25 * 16_000,
            Self::GigaAm(_) => gigaam_core::MAX_SAMPLES,
            Self::Parakeet(_) => crate::parakeet::MAX_SAMPLES,
        }
    }
    pub fn is_whisper(&self) -> bool {
        matches!(self, Self::Whisper(_))
    }
    pub async fn transcribe(
        &self,
        samples: Vec<f32>,
        language: Option<String>,
        prompt: Option<String>,
        partial: bool,
    ) -> Result<(String, Option<f32>, bool), String> {
        match self {
            Self::GigaAm(engine) => engine.transcribe(samples, partial).await,
            Self::Parakeet(engine) => engine.transcribe(samples, partial).await,
            Self::Whisper(engine) => {
                let options = crate::whisper_engine::TranscribeOptions {
                    greedy: partial,
                    purpose: if partial {
                        crate::whisper_engine::DecodePurpose::Partial
                    } else {
                        crate::whisper_engine::DecodePurpose::Final
                    },
                    ..Default::default()
                };
                engine
                    .transcribe_audio_with_confidence_opts(samples, language, prompt, options)
                    .await
                    .map(|(t, c, p)| (t, Some(c), p))
                    .map_err(|e| e.to_string())
            }
        }
    }
    pub async fn is_model_loaded(&self) -> bool {
        match self {
            Self::Whisper(engine) => engine.is_model_loaded().await,
            Self::GigaAm(_) | Self::Parakeet(_) => true,
        }
    }

    pub async fn get_current_model(&self) -> Option<String> {
        match self {
            Self::Whisper(engine) => engine.get_current_model().await,
            Self::GigaAm(engine) => Some(engine.name.clone()),
            Self::Parakeet(engine) => Some(engine.name.clone()),
        }
    }

    pub fn provider_name(&self) -> &str {
        match self {
            Self::Whisper(_) => "Whisper (direct)",
            Self::GigaAm(_) => "GigaAM (native ONNX)",
            Self::Parakeet(_) => "Parakeet TDT v3 (native ONNX)",
        }
    }
}

/// Validate the selected local model before recording starts.
pub async fn validate_transcription_model_ready(
    pool: Option<&sqlx::SqlitePool>,
) -> Result<(), String> {
    let (provider, model) = resolve_selection(pool, None, None).await?;
    if provider == crate::parakeet::PROVIDER {
        let model = model.clone();
        return tokio::task::spawn_blocking(move || crate::parakeet::validate(&model))
            .await
            .map_err(|e| e.to_string())?;
    }
    if provider == crate::gigaam::PROVIDER {
        return crate::gigaam::validate(&model);
    }
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

/// Acquire the selected local engine for the lifetime of a recording task.
pub async fn get_or_init_transcription_engine(
    pool: Option<&sqlx::SqlitePool>,
) -> Result<TranscriptionEngine, String> {
    let (provider, model) = resolve_selection(pool, None, None).await?;
    if provider == crate::gigaam::PROVIDER {
        return Ok(TranscriptionEngine::GigaAm(
            crate::gigaam::acquire(&model).await?,
        ));
    }
    if provider == crate::parakeet::PROVIDER {
        return Ok(TranscriptionEngine::Parakeet(
            crate::parakeet::acquire(&model).await?,
        ));
    }
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
    // directly from the pool via `get_transcript_config` — this only needs
    // `provider`/`model`, not the API key. No pool yet (DB still initialising
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
    let configured_model: Option<String> = match saved_config {
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

/// Explicit model IDs override saved settings, so batch dialogs can select a
/// different provider without accidentally treating GigaAM as a Whisper model.
pub async fn resolve_selection(
    pool: Option<&sqlx::SqlitePool>,
    model: Option<&str>,
    provider: Option<&str>,
) -> Result<(String, String), String> {
    if let Some(model) = model.filter(|s| !s.is_empty()) {
        let inferred = provider_for_model(model);
        if let Some(p) = provider {
            if p != inferred && !(p == "whisper" && inferred == "localWhisper") {
                return Err(format!("Model {model} does not belong to provider {p}"));
            }
        }
        return Ok((inferred.into(), model.into()));
    }
    let saved = match pool {
        Some(p) => {
            crate::database::repositories::setting::SettingsRepository::get_transcript_config(p)
                .await
                .map_err(|e| e.to_string())?
        }
        None => None,
    };
    match saved {
        Some(c) if c.provider == crate::parakeet::PROVIDER => {
            let model = if c.model.is_empty() {
                crate::parakeet::DEFAULT_MODEL.into()
            } else {
                c.model
            };
            if !crate::parakeet::is_model(&model) {
                return Err(format!("Unknown Parakeet model: {model}"));
            }
            Ok((c.provider, model))
        }
        Some(c) if c.provider == crate::gigaam::PROVIDER => Ok((
            c.provider,
            if c.model.is_empty() {
                crate::gigaam::DEFAULT_MODEL.into()
            } else {
                c.model
            },
        )),
        Some(c) if c.provider == "localWhisper" || c.provider == "whisper" => {
            Ok(("localWhisper".into(), c.model))
        }
        Some(c) => Err(format!(
            "Unsupported local transcription provider: {}",
            c.provider
        )),
        None => Ok((
            "localWhisper".into(),
            crate::config::DEFAULT_WHISPER_MODEL.into(),
        )),
    }
}

pub fn provider_for_model(model: &str) -> &'static str {
    if crate::gigaam::is_model(model) {
        crate::gigaam::PROVIDER
    } else if crate::parakeet::is_model(model) {
        crate::parakeet::PROVIDER
    } else {
        "localWhisper"
    }
}

pub fn selection_locked() -> bool {
    crate::parakeet::is_busy()
        || crate::gigaam::is_busy()
        || crate::whisper_engine::LIVE_ENGINE_LEASE.is_live_leased()
        || crate::audio::import::is_import_in_progress()
        || crate::audio::retranscription::is_retranscription_in_progress()
}

pub async fn model_catalog() -> Result<Vec<crate::whisper_engine::ModelInfo>, String> {
    let mut models = crate::whisper_engine::whisper_get_available_models().await?;
    models.extend(crate::gigaam::catalog());
    models.extend(crate::parakeet::catalog());
    Ok(models)
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    #[tokio::test]
    async fn saved_parakeet_selection_survives_reload_and_batch_override() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE transcript_settings (id TEXT PRIMARY KEY, provider TEXT NOT NULL, model TEXT NOT NULL, whisperApiKey TEXT, deepgramApiKey TEXT, elevenLabsApiKey TEXT, groqApiKey TEXT, openaiApiKey TEXT)")
            .execute(&pool).await.unwrap();
        let repo =
            crate::database::repositories::setting::SettingsRepository::save_transcript_config;
        repo(
            &pool,
            crate::parakeet::PROVIDER,
            crate::parakeet::DEFAULT_MODEL,
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_selection(Some(&pool), None, None).await.unwrap(),
            (
                crate::parakeet::PROVIDER.into(),
                crate::parakeet::DEFAULT_MODEL.into()
            )
        );
        assert_eq!(
            resolve_selection(Some(&pool), Some("small"), None)
                .await
                .unwrap(),
            ("localWhisper".into(), "small".into())
        );
        assert!(
            repo(&pool, crate::parakeet::PROVIDER, "unknown")
                .await
                .is_err()
        );
        sqlx::query("UPDATE transcript_settings SET model = ''")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            resolve_selection(Some(&pool), None, None).await.unwrap().1,
            crate::parakeet::DEFAULT_MODEL
        );
        sqlx::query("UPDATE transcript_settings SET model = 'unknown'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(resolve_selection(Some(&pool), None, None).await.is_err());
    }

    #[tokio::test]
    async fn explicit_model_selects_provider_and_rejects_mismatch() {
        assert_eq!(
            resolve_selection(None, Some(crate::parakeet::DEFAULT_MODEL), None)
                .await
                .unwrap(),
            (
                crate::parakeet::PROVIDER.into(),
                crate::parakeet::DEFAULT_MODEL.into()
            )
        );
        assert!(
            resolve_selection(
                None,
                Some(crate::parakeet::DEFAULT_MODEL),
                Some("localWhisper")
            )
            .await
            .is_err()
        );
        assert_eq!(
            resolve_selection(None, Some(crate::parakeet::CPU_MODEL), None)
                .await
                .unwrap()
                .0,
            crate::parakeet::PROVIDER
        );
        for model in gigaam_core::MODELS {
            assert_eq!(
                resolve_selection(None, Some(model), None).await.unwrap(),
                (crate::gigaam::PROVIDER.into(), model.into())
            );
            assert!(
                resolve_selection(None, Some(model), Some("localWhisper"))
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            resolve_selection(None, Some("large-v3-turbo"), None)
                .await
                .unwrap()
                .0,
            "localWhisper"
        );
        assert_eq!(
            resolve_selection(None, None, None).await.unwrap().0,
            "localWhisper"
        );
    }
}
