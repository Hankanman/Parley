// Commit name to recover the serial whisper engine processing for smaller meetings [Slower processing but dooes not fail] - "before parallel processing implementation"

use crate::config::WHISPER_MODEL_CATALOG;
use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelStatus {
    Available,
    Missing,
    Downloading {
        progress: u8,
    },
    Error(String),
    Corrupted {
        file_size: u64,
        expected_min_size: u64,
    },
}

/// Which long-lived `WhisperState` a decode should use (see #49). The final
/// path (`worker.rs`) and the streaming partial-decode path
/// (`partial_worker.rs`) run concurrently against the same loaded model, so
/// each gets its own state to avoid serializing on one another or racing on
/// a shared `whisper_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DecodePurpose {
    /// The authoritative, committed transcript.
    #[default]
    Final,
    /// A discarded streaming preview of the in-progress utterance.
    Partial,
}

/// Per-call overrides for `transcribe_audio_with_confidence_opts`.
///
/// Defaults (`TranscribeOptions::default()`) reproduce the historical
/// behavior of `transcribe_audio_with_confidence`: full adaptive thread
/// budget, beam search at the hardware-adaptive beam size, final-path state.
#[derive(Debug, Clone, Copy, Default)]
pub struct TranscribeOptions {
    /// Upper bound on the number of whisper threads to use for this call.
    /// The effective thread count is `min(adaptive_default, max_threads)`,
    /// floored at 1 — see `effective_threads`. `None` uses the full
    /// hardware-adaptive default.
    pub max_threads: Option<i32>,
    /// Use greedy (beam_size 1, no patience search) sampling instead of the
    /// hardware-adaptive beam size. Intended for streaming partial decodes,
    /// which are discarded previews, not the committed transcript — greedy
    /// decoding is faster and the lower quality is acceptable there.
    pub greedy: bool,
    /// Which pooled `WhisperState` to decode on (see `DecodePurpose`).
    pub purpose: DecodePurpose,
}

/// Cap `default` (the hardware-adaptive thread count) at `requested`, if
/// given, flooring the result at 1 so a caller can never request zero or
/// negative threads. Pure function so the cap logic is unit-testable
/// without spinning up a whisper context.
/// Re-check-under-write-lock + lazily-insert helper backing the
/// `DecodePurpose` state pool (#49): if `purpose` is already present,
/// return its `Arc` (a caller may have raced us between dropping the read
/// lock and taking the write lock); otherwise construct a fresh value with
/// `make`, insert it, and return it. Generic and pure of any whisper-rs
/// type so it's unit-testable without a real model loaded.
fn pool_get_or_insert<T, E>(
    pool: &mut HashMap<DecodePurpose, Arc<AsyncMutex<T>>>,
    purpose: DecodePurpose,
    make: impl FnOnce() -> Result<T, E>,
) -> Result<Arc<AsyncMutex<T>>, E> {
    if let Some(existing) = pool.get(&purpose) {
        return Ok(existing.clone());
    }
    let value = Arc::new(AsyncMutex::new(make()?));
    pool.insert(purpose, value.clone());
    Ok(value)
}

fn effective_threads(default: i32, requested: Option<i32>) -> i32 {
    let capped = match requested {
        Some(requested) => default.min(requested),
        None => default,
    };
    capped.max(1)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub path: PathBuf,
    pub size_mb: u32,
    pub accuracy: String,
    pub speed: String,
    pub status: ModelStatus,
    pub description: String,
}

pub struct WhisperEngine {
    models_dir: PathBuf,
    // Wrapped in an Arc so a cheap clone can be handed to spawn_blocking for
    // inference/model-loading without holding the RwLock guard (or the
    // WhisperContext itself) across the blocking call.
    current_context: Arc<RwLock<Option<Arc<WhisperContext>>>>,
    current_model: Arc<RwLock<Option<String>>>,
    // Long-lived `WhisperState`s, keyed by `DecodePurpose`, reused across
    // decodes instead of calling `ctx.create_state()` per call (#49).
    // Created lazily on first use after `load_model`, dropped in
    // `unload_model` (and thus whenever the model changes, since
    // `load_model` unloads the previous one first). Each entry is behind
    // its own `tokio::sync::Mutex` so the final and partial paths never
    // block on each other — only concurrent decodes for the *same*
    // purpose serialize.
    state_pool: Arc<RwLock<HashMap<DecodePurpose, Arc<AsyncMutex<WhisperState>>>>>,
    available_models: Arc<RwLock<HashMap<String, ModelInfo>>>,
    // Tracks in-flight downloads and cooperative cancellation requests
    // (shared with summary_engine::model_manager's downloader).
    download_guard: Arc<crate::utils::download::DownloadGuard>,
}

/// Check a file's header for a GGML/GGUF magic number. Plain `std::fs`
/// (blocking) rather than `tokio::fs` — it's an 8-byte read, and this needs
/// to be callable from both async (`WhisperEngine::discover_models`) and
/// sync (`whisper_engine::commands::discover_models_standalone`, used
/// before the engine - and its async runtime access - is initialized)
/// contexts without extra ceremony.
fn validate_model_file_magic(model_path: &std::path::Path) -> Result<()> {
    use std::io::Read;

    let mut file =
        std::fs::File::open(model_path).map_err(|e| anyhow!("Failed to open model file: {}", e))?;

    // Read the first 8 bytes to check for GGML magic number
    let mut buffer = [0u8; 8];
    file.read_exact(&mut buffer)
        .map_err(|e| anyhow!("Failed to read model file header: {}", e))?;

    // Check for GGML magic number (various versions and endianness)
    if buffer.starts_with(b"ggml")
        || buffer.starts_with(b"GGUF")
        || buffer.starts_with(b"ggmf")
        || buffer.starts_with(b"lmgg")
        || buffer.starts_with(b"FUGU")
        || buffer.starts_with(b"fmgg")
    {
        Ok(())
    } else {
        Err(anyhow!(
            "Invalid model file: missing GGML/GGUF magic number. Found: {:?}",
            String::from_utf8_lossy(&buffer[..4])
        ))
    }
}

/// Scan one [`WHISPER_MODEL_CATALOG`] entry against `models_dir`, applying
/// the standard size + magic-number corruption checks. Shared by
/// `WhisperEngine::discover_models` and the standalone fallback
/// (`whisper_engine::commands::discover_models_standalone`) used when the
/// engine hasn't been initialized yet, so both agree on what counts as
/// `Available` / `Corrupted` / `Missing`.
///
/// `downloading_progress` is the in-memory `Downloading { progress }` value
/// for this model, if the caller has one (only `WhisperEngine` does, via
/// its `available_models` cache) - it's what keeps an in-progress download
/// from being misreported as a corrupted file. Pass `None` when no such
/// state is available.
pub fn scan_catalog_entry(
    models_dir: &std::path::Path,
    filename: &str,
    size_mb: u32,
    downloading_progress: Option<u8>,
) -> ModelStatus {
    let model_path = models_dir.join(filename);

    let Ok(metadata) = std::fs::metadata(&model_path) else {
        return ModelStatus::Missing;
    };

    let file_size_bytes = metadata.len();
    let file_size_mb = file_size_bytes / (1024 * 1024);
    // Allow 90% of expected size as minimum for more accurate corruption detection
    let expected_min_size_mb = (size_mb as f64 * 0.9) as u64;

    if file_size_mb >= expected_min_size_mb && file_size_mb > 1 {
        // File size looks good, but let's also check if it's a valid GGML file
        match validate_model_file_magic(&model_path) {
            Ok(_) => ModelStatus::Available,
            Err(_) => {
                log::warn!(
                    "Model file {} has correct size but appears corrupted (failed validation)",
                    filename
                );
                ModelStatus::Corrupted {
                    file_size: file_size_bytes,
                    expected_min_size: expected_min_size_mb * 1024 * 1024,
                }
            }
        }
    } else if file_size_mb > 0 {
        // File exists but is smaller than expected
        if let Some(progress) = downloading_progress {
            log::debug!(
                "Model {} appears to be downloading ({} MB so far, {}% complete)",
                filename,
                file_size_mb,
                progress
            );
            ModelStatus::Downloading { progress }
        } else {
            log::warn!(
                "Model file {} exists but is corrupted ({} MB, expected ~{} MB)",
                filename,
                file_size_mb,
                size_mb
            );
            ModelStatus::Corrupted {
                file_size: file_size_bytes,
                expected_min_size: expected_min_size_mb * 1024 * 1024,
            }
        }
    } else {
        ModelStatus::Missing
    }
}

impl WhisperEngine {
    /// Detect available GPU acceleration capabilities
    fn detect_gpu_acceleration() -> bool {
        // Check for CUDA support
        if cfg!(feature = "cuda") {
            log::info!("CUDA feature enabled - attempting GPU acceleration");
            return true;
        }

        // Check for Vulkan support
        if cfg!(feature = "vulkan") {
            log::info!("Vulkan feature enabled - attempting GPU acceleration");
            return true;
        }

        // Fall back to CPU
        log::info!("No GPU acceleration features detected - using CPU processing");
        false
    }

    pub fn new() -> Result<Self> {
        Self::new_with_models_dir(None)
    }

    /// Create a new WhisperEngine with optional custom models directory
    /// If models_dir is None, uses default location (app data dir for production, local for dev)
    pub fn new_with_models_dir(models_dir: Option<PathBuf>) -> Result<Self> {
        // PERFORMANCE: Suppress verbose whisper.cpp and Metal logs
        // These C library logs bypass Rust logging and clutter output
        // Set environment variables to reduce C library verbosity
        std::env::set_var("GGML_METAL_LOG_LEVEL", "1"); // 0=off, 1=error, 2=warn, 3=info
        std::env::set_var("WHISPER_LOG_LEVEL", "1"); // Reduce whisper.cpp verbosity

        let models_dir = if let Some(dir) = models_dir {
            // Use provided directory (for production with app_data_dir)
            dir
        } else {
            // Fallback: determine based on debug/release mode
            let current_dir = std::env::current_dir()
                .map_err(|e| anyhow!("Failed to get current directory: {}", e))?;

            // Development: Use frontend/models or backend directories
            // Production: Use system directories (should be overridden by caller)
            if cfg!(debug_assertions) {
                // Development mode - try frontend and backend directories
                if current_dir.join("models").exists() {
                    current_dir.join("models")
                } else if current_dir.join("../models").exists() {
                    current_dir.join("../models")
                } else if current_dir
                    .join("backend/whisper-server-package/models")
                    .exists()
                {
                    current_dir.join("backend/whisper-server-package/models")
                } else if current_dir
                    .join("../backend/whisper-server-package/models")
                    .exists()
                {
                    current_dir.join("../backend/whisper-server-package/models")
                } else {
                    // Create models directory in current directory for development
                    current_dir.join("models")
                }
            } else {
                // Production mode fallback (shouldn't reach here, caller should provide path)
                log::warn!("WhisperEngine: No models directory provided, using fallback path");
                crate::paths::app_data_dir()
                    .map_err(|e| anyhow!(e))?
                    .join("models")
            }
        };

        log::info!(
            "WhisperEngine using models directory: {}",
            models_dir.display()
        );
        log::info!("Debug mode: {}", cfg!(debug_assertions));

        // Log acceleration capabilities
        let gpu_support = Self::detect_gpu_acceleration();
        log::info!(
            "Hardware acceleration support: {}",
            if gpu_support { "enabled" } else { "disabled" }
        );

        #[cfg(feature = "openblas")]
        log::info!("OpenBLAS CPU optimization: enabled");

        #[cfg(feature = "cuda")]
        log::info!("NVIDIA CUDA support: enabled");

        #[cfg(feature = "vulkan")]
        log::info!("Vulkan GPU support: enabled");

        #[cfg(feature = "openmp")]
        log::info!("OpenMP parallel processing: enabled");

        let engine = Self {
            models_dir,
            current_context: Arc::new(RwLock::new(None)),
            state_pool: Arc::new(RwLock::new(HashMap::new())),
            current_model: Arc::new(RwLock::new(None)),
            available_models: Arc::new(RwLock::new(HashMap::new())),
            // Initialize download tracking
            download_guard: Arc::new(crate::utils::download::DownloadGuard::new()),
        };

        Ok(engine)
    }

    pub async fn discover_models(&self) -> Result<Vec<ModelInfo>> {
        let models_dir = &self.models_dir;
        let mut models = Vec::new();
        // Use centralized model catalog from config.rs
        let model_configs = WHISPER_MODEL_CATALOG;

        // Snapshot current statuses once up front rather than re-locking per
        // catalog entry - only used to tell "still downloading" apart from
        // "corrupted" for an undersized file below.
        let current_statuses = self.available_models.read().await.clone();

        for &(name, filename, size_mb, accuracy, speed, description) in model_configs {
            let downloading_progress = match current_statuses.get(name).map(|m| &m.status) {
                Some(ModelStatus::Downloading { progress }) => Some(*progress),
                _ => None,
            };

            let status = scan_catalog_entry(models_dir, filename, size_mb, downloading_progress);

            let model_info = ModelInfo {
                name: name.to_string(),
                path: models_dir.join(filename),
                size_mb,
                accuracy: accuracy.to_string(),
                speed: speed.to_string(),
                status,
                description: description.to_string(),
            };

            models.push(model_info);
        }

        // Update internal cache
        let mut available_models = self.available_models.write().await;
        available_models.clear();
        for model in &models {
            available_models.insert(model.name.clone(), model.clone());
        }

        Ok(models)
    }

    pub async fn load_model(&self, model_name: &str) -> Result<()> {
        let models = self.available_models.read().await;
        let model_info = models
            .get(model_name)
            .ok_or_else(|| anyhow!("Model {} not found", model_name))?;

        match model_info.status {
            ModelStatus::Available => {
                // FIX 5: Check if this model is already loaded
                // Clone into an owned value so the read guard is dropped
                // before we potentially call unload_model() below — holding
                // it across that call deadlocks, since unload_model() needs
                // a write lock on this same field.
                let currently_loaded = self.current_model.read().await.clone();
                if let Some(current_model) = currently_loaded {
                    if current_model == model_name {
                        log::info!("Model {} is already loaded, skipping reload", model_name);
                        return Ok(());
                    }

                    // FIX 5: Unload current model before loading new one
                    log::info!(
                        "Unloading current model '{}' before loading '{}'",
                        current_model,
                        model_name
                    );
                    self.unload_model().await;
                }

                log::info!("Loading model: {}", model_name);

                // PERFORMANCE OPTIMIZATION: Use comprehensive hardware profile for optimal GPU configuration
                let hardware_profile = crate::audio::HardwareProfile::detect();
                let adaptive_config = hardware_profile.get_whisper_config();

                // Enable flash attention for high-end GPUs (CUDA on NVIDIA)
                // Flash attention provides 20-40% speedup but requires stable GPU drivers
                let flash_attn_enabled = matches!(
                    (
                        &hardware_profile.gpu_type,
                        &hardware_profile.performance_tier,
                    ),
                    (
                        crate::audio::GpuType::Cuda,
                        crate::audio::PerformanceTier::Ultra | crate::audio::PerformanceTier::High,
                    )
                ); // Conservative: disable for other GPU types and lower tiers

                let context_param = WhisperContextParameters {
                    use_gpu: adaptive_config.use_gpu,
                    gpu_device: 0,
                    flash_attn: flash_attn_enabled,
                    ..Default::default()
                };

                // PERFORMANCE: Suppress verbose C library logs during model loading
                // This hides the excessive Metal/GGML initialization logs in release builds
                //
                // Parsing model weights and allocating GPU buffers is a slow,
                // synchronous operation (can take seconds for large models) —
                // run it on a blocking-pool thread so it doesn't stall the
                // async runtime.
                let model_path = model_info.path.clone();
                let model_name_owned = model_name.to_string();
                let ctx = tokio::task::spawn_blocking(move || {
                    // let _suppressor = crate::whisper_engine::StderrSuppressor::new();

                    // Load whisper context with hardware-optimized parameters
                    WhisperContext::new_with_params(&model_path, context_param)
                        .map_err(|e| anyhow!("Failed to load model {}: {}", model_name_owned, e))
                    // Suppressor dropped here, stderr restored
                })
                .await
                .map_err(|e| anyhow!("Model loading task panicked: {}", e))??;

                // Update current context and model
                *self.current_context.write().await = Some(Arc::new(ctx));
                *self.current_model.write().await = Some(model_name.to_string());

                // Enhanced acceleration status reporting
                let acceleration_status = match (&hardware_profile.gpu_type, flash_attn_enabled) {
                    (crate::audio::GpuType::Cuda, true) => {
                        "CUDA GPU with Flash Attention (Ultra-Fast)"
                    }
                    (crate::audio::GpuType::Cuda, false) => "CUDA GPU acceleration",
                    (crate::audio::GpuType::Vulkan, _) => "Vulkan GPU acceleration",
                    (crate::audio::GpuType::None, _) => "CPU processing only",
                };

                log::info!("Successfully loaded model: {} with {} (Performance Tier: {:?}, Beam Size: {}, Threads: {:?})",
                          model_name, acceleration_status, hardware_profile.performance_tier,
                          adaptive_config.beam_size, adaptive_config.max_threads);
                Ok(())
            }
            ModelStatus::Missing => Err(anyhow!("Model {} is not downloaded", model_name)),
            ModelStatus::Downloading { .. } => {
                Err(anyhow!("Model {} is currently downloading", model_name))
            }
            ModelStatus::Error(ref err) => Err(anyhow!("Model {} has error: {}", model_name, err)),
            ModelStatus::Corrupted { .. } => Err(anyhow!(
                "Model {} is corrupted and cannot be loaded",
                model_name
            )),
        }
    }

    pub async fn unload_model(&self) -> bool {
        let mut ctx_guard = self.current_context.write().await;
        let unloaded = ctx_guard.take().is_some();
        if unloaded {
            log::info!("📉Whisper model unloaded");
        }

        let mut model_name_guard = self.current_model.write().await;
        model_name_guard.take();

        // Pooled states borrow the (now-gone) context's underlying weights
        // via their own `Arc<WhisperInnerContext>` clone — drop them here
        // so nothing keeps the model's memory alive past unload, and so the
        // next `load_model` starts from an empty pool (see #49).
        self.state_pool.write().await.clear();

        unloaded
    }

    pub async fn get_current_model(&self) -> Option<String> {
        self.current_model.read().await.clone()
    }

    pub async fn is_model_loaded(&self) -> bool {
        self.current_context.read().await.is_some()
    }

    /// Get the pooled `WhisperState` for `purpose`, creating it lazily on
    /// first use. Reused across calls (#49) instead of `ctx.create_state()`
    /// per decode — `WhisperState` is safe to reuse across `full()` calls
    /// and owns its own reference to the context, so it isn't invalidated
    /// by anything short of `unload_model`/`load_model`.
    async fn get_or_create_state(
        &self,
        purpose: DecodePurpose,
    ) -> Result<Arc<AsyncMutex<WhisperState>>> {
        if let Some(state) = self.state_pool.read().await.get(&purpose) {
            return Ok(state.clone());
        }

        let ctx = {
            let ctx_lock = self.current_context.read().await;
            ctx_lock
                .as_ref()
                .cloned()
                .ok_or_else(|| anyhow!("No model loaded. Please load a model first."))?
        };

        let mut pool = self.state_pool.write().await;
        Ok(pool_get_or_insert(&mut pool, purpose, || ctx.create_state())?)
    }

    // Enhanced function to clean repetitive text patterns and meaningless outputs
    /// whisper.cpp brackets short or uncertain segments with "..." continuation
    /// markers (e.g. "...yeah..."). They're a decode artifact, not real
    /// punctuation, so collapse any run of 3+ dots to a space. Shorter runs
    /// (abbreviations, decimals like "3.5") are left alone.
    fn strip_continuation_ellipses(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut dots = 0usize;
        let flush = |out: &mut String, dots: usize| {
            if dots >= 3 {
                out.push(' ');
            } else {
                for _ in 0..dots {
                    out.push('.');
                }
            }
        };
        for ch in text.chars() {
            if ch == '.' {
                dots += 1;
            } else {
                flush(&mut out, dots);
                dots = 0;
                out.push(ch);
            }
        }
        flush(&mut out, dots);
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn clean_repetitive_text(text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }

        // Strip whisper's "..." continuation markers before any other analysis
        // (they otherwise survive as literal "...word..." fragments in the UI).
        let stripped = Self::strip_continuation_ellipses(text);
        let text = stripped.as_str();
        if text.is_empty() {
            return String::new();
        }

        // Check for obviously meaningless patterns first
        if Self::is_meaningless_output(text) {
            // Performance optimization: reduce meaningless output logging to debug level
            perf_debug!("Detected meaningless output, returning empty: '{}'", text);
            return String::new();
        }

        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() < 3 {
            return text.to_string();
        }

        // Enhanced repetition detection with sliding window
        let cleaned_words = Self::remove_word_repetitions(&words);

        // Remove phrase repetitions with more sophisticated detection
        let cleaned_words = Self::remove_phrase_repetitions(&cleaned_words);

        // Check for overall repetition ratio
        let final_text = cleaned_words.join(" ");
        if Self::calculate_repetition_ratio(&final_text) > 0.7 {
            // Performance optimization: reduce repetition ratio logging to debug level
            perf_debug!(
                "High repetition ratio detected, filtering out: '{}'",
                final_text
            );
            return String::new();
        }

        final_text
    }

    // Check for obviously meaningless patterns
    fn is_meaningless_output(text: &str) -> bool {
        let text_lower = text.to_lowercase();

        // Check for common meaningless patterns
        let meaningless_patterns = [
            "thank you for watching",
            "thanks for watching",
            "like and subscribe",
            "music playing",
            "applause",
            "laughter",
            "um um um",
            "uh uh uh",
            "ah ah ah",
        ];

        for pattern in &meaningless_patterns {
            if text_lower.contains(pattern) {
                return true;
            }
        }

        // Check if text is mostly the same character or very short repetitive patterns
        let unique_chars: HashSet<char> = text.chars().collect();
        if unique_chars.len() <= 3 && text.len() > 10 {
            return true;
        }

        false
    }

    // Enhanced word repetition removal
    fn remove_word_repetitions<'a>(words: &'a [&'a str]) -> Vec<&'a str> {
        let mut cleaned_words = Vec::new();
        let mut i = 0;

        while i < words.len() {
            let current_word = words[i];
            let mut repeat_count = 1;

            // Count consecutive repetitions of the same word
            while i + repeat_count < words.len() && words[i + repeat_count] == current_word {
                repeat_count += 1;
            }

            // Be more aggressive: if word is repeated 2+ times, only keep one instance
            if repeat_count >= 2 {
                cleaned_words.push(current_word);
                i += repeat_count;
            } else {
                cleaned_words.push(current_word);
                i += 1;
            }
        }

        cleaned_words
    }

    // Enhanced phrase repetition removal with variable length detection
    fn remove_phrase_repetitions<'a>(words: &'a [&'a str]) -> Vec<&'a str> {
        if words.len() < 4 {
            return words.to_vec();
        }

        let mut final_words = Vec::new();
        let mut i = 0;

        while i < words.len() {
            let mut phrase_found = false;

            // Check for 2-word to 5-word phrase repetitions
            for phrase_len in 2..=std::cmp::min(5, (words.len() - i) / 2) {
                if i + phrase_len * 2 <= words.len() {
                    let phrase1 = &words[i..i + phrase_len];
                    let phrase2 = &words[i + phrase_len..i + phrase_len * 2];

                    if phrase1 == phrase2 {
                        // Add the phrase once and skip the repetition
                        final_words.extend_from_slice(phrase1);
                        i += phrase_len * 2;
                        phrase_found = true;
                        break;
                    }
                }
            }

            if !phrase_found {
                final_words.push(words[i]);
                i += 1;
            }
        }

        final_words
    }

    // Calculate repetition ratio in text
    fn calculate_repetition_ratio(text: &str) -> f32 {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() < 4 {
            return 0.0;
        }

        let mut word_counts = HashMap::new();
        for word in &words {
            *word_counts.entry(word.to_lowercase()).or_insert(0) += 1;
        }

        let total_words = words.len() as f32;
        let repeated_words: usize = word_counts
            .values()
            .map(|&count| if count > 1 { count - 1 } else { 0 })
            .sum();

        repeated_words as f32 / total_words
    }

    /// Transcribe audio with streaming support for partial results and adaptive quality.
    ///
    /// `context_prompt` conditions the decoder on the tail of the preceding
    /// transcript (whisper's initial-prompt mechanism), which improves
    /// cross-segment consistency for casing, punctuation, and proper nouns.
    ///
    /// The returned confidence is derived from whisper's own decoder
    /// signals: the mean token probability of each segment, discounted by
    /// the segment's no-speech probability. Roughly: clear speech lands
    /// around 0.6–0.95, marginal/quiet speech 0.3–0.6, and hallucinated
    /// text on near-silence below 0.3.
    pub async fn transcribe_audio_with_confidence(
        &self,
        audio_data: Vec<f32>,
        language: Option<String>,
        context_prompt: Option<String>,
    ) -> Result<(String, f32, bool)> {
        self.transcribe_audio_with_confidence_opts(
            audio_data,
            language,
            context_prompt,
            TranscribeOptions::default(),
        )
        .await
    }

    /// Same as `transcribe_audio_with_confidence`, with per-call overrides
    /// (thread budget cap, greedy sampling) via `options`. See
    /// `TranscribeOptions` for details.
    ///
    /// Added to give the streaming partial-decode worker
    /// (`audio/transcription/partial_worker.rs`) a reduced thread budget so
    /// it doesn't starve the authoritative final-transcription path on
    /// CPU-only hardware, which runs concurrently on the same Whisper
    /// context (issue #26).
    pub async fn transcribe_audio_with_confidence_opts(
        &self,
        audio_data: Vec<f32>,
        language: Option<String>,
        context_prompt: Option<String>,
        options: TranscribeOptions,
    ) -> Result<(String, f32, bool)> {
        // Reuse the long-lived state for this purpose instead of creating a
        // fresh `WhisperState` per decode (#49). Final and partial decodes
        // never contend on the same lock since each purpose gets its own.
        let state_lock = self.get_or_create_state(options.purpose).await?;

        let duration_seconds = audio_data.len() as f64 / 16000.0;
        let is_partial = duration_seconds < 15.0; // Consider chunks under 15s as partial

        // PERFORMANCE: Suppress verbose C library logs during transcription
        // This hides whisper_full_with_state debug logs and beam search details
        //
        // Whisper inference — including building `FullParams`, which is CPU-
        // bound and can take seconds — runs on a blocking-pool thread so it
        // doesn't stall the async runtime. `FullParams::set_language` borrows
        // from `language`, so building it here (rather than outside and
        // moving it in) keeps that borrow entirely local to this closure;
        // `language` is captured by value, so nothing non-'static crosses
        // the spawn_blocking boundary.
        let (result, total_confidence, segment_count) =
            tokio::task::spawn_blocking(move || -> Result<(String, f32, u32)> {
                // let _suppressor = crate::whisper_engine::StderrSuppressor::new();

                // Get adaptive configuration based on hardware
                let hardware_profile = crate::audio::HardwareProfile::detect();
                let adaptive_config = hardware_profile.get_whisper_config();

                // ADAPTIVE parameters - optimized for current hardware.
                // `options.greedy` (set by the partial-decode worker) skips
                // beam search entirely — partials are discarded previews,
                // not the committed transcript, so the speed/quality
                // tradeoff favors greedy decoding there.
                let mut params = if options.greedy {
                    FullParams::new(SamplingStrategy::Greedy { best_of: 1 })
                } else {
                    FullParams::new(SamplingStrategy::BeamSearch {
                        beam_size: adaptive_config.beam_size as i32,
                        patience: 1.0,
                    })
                };

                // Thread budget: cap the hardware-adaptive default at the
                // caller's requested `options.max_threads`, if any (issue
                // #26 — lets the partial-decode worker run at half budget
                // so it doesn't starve the final path on CPU-only hardware).
                let default_threads = adaptive_config.max_threads.unwrap_or(4) as i32;
                params.set_n_threads(effective_threads(default_threads, options.max_threads));

                // Configure with adaptive settings
                // If language is "auto" or None, use automatic language detection (pass None)
                // If language is "auto-translate", enable translation to English
                // Otherwise, use the specified language code
                let (language_code, should_translate) = match language.as_deref() {
                    Some("auto") | None => (None, false),
                    Some("auto-translate") => (None, true),
                    Some(lang) => (Some(lang), false),
                };
                params.set_language(language_code);
                params.set_translate(should_translate);

                // CRITICAL: Disable timestamp tokens to prevent whisper.cpp chunking heuristics
                // The "single timestamp ending - skip entire chunk" optimization incorrectly discards
                // complete, valid transcriptions. Disabling timestamps forces whisper to return ALL text.
                params.set_no_timestamps(true); // Prevent timestamp-based segment skipping
                params.set_token_timestamps(true); // Keep for any timestamp-aware features

                // PERFORMANCE: Disable ALL whisper.cpp internal printing
                // This reduces C library log spam significantly
                params.set_print_special(false); // Don't print special tokens
                params.set_print_progress(false); // Don't print progress
                params.set_print_realtime(false); // Don't print realtime info
                params.set_print_timestamps(false); // Don't print timestamps

                // Additional suppression to reduce C library verbosity
                params.set_suppress_blank(true);
                params.set_suppress_nst(true);
                params.set_temperature(adaptive_config.temperature);
                params.set_max_initial_ts(1.0);
                params.set_entropy_thold(2.4);
                params.set_logprob_thold(-1.0);
                // BALANCED FIX: Lowered from 0.75 to 0.55 to allow quiet speech detection
                // Previous value was too aggressive and rejected valid quiet speech
                // 0.55 is balanced - prevents hallucinations while preserving quiet speech
                params.set_no_speech_thold(0.55);
                params.set_max_len(200);
                params.set_single_segment(false);

                // Condition the decoder on the preceding transcript tail.
                // NOTE: whisper-rs's set_initial_prompt leaks its CString
                // (into_raw without a matching free) — a few hundred bytes
                // per segment, bounded by prompt truncation below; accepted
                // until upstream fixes it. It also panics on interior NULs,
                // hence the sanitization.
                let sanitized_prompt = context_prompt
                    .as_deref()
                    .map(|p| p.replace('\0', " "))
                    .filter(|p| !p.trim().is_empty());
                if let Some(ref prompt) = sanitized_prompt {
                    params.set_initial_prompt(prompt);
                }

                // whisper.cpp silently returns zero segments (no error) for
                // input shorter than 1 s ("input is too short"), so
                // sub-second utterances — "yes", "okay", "no" — would vanish.
                // Zero-pad the tail up to a safe floor before decoding.
                let audio_data = pad_to_min_whisper_input(audio_data);

                // `blocking_lock()` — never held across an `.await`, this
                // closure runs entirely inside `spawn_blocking`.
                let mut state = state_lock.blocking_lock();
                state.full(params, &audio_data)?;
                let num_segments = state.full_n_segments();
                // Suppressor dropped here, stderr restored

                let mut result = String::new();
                // Weighted by token count so a long confident segment isn't
                // dragged down by a two-token trailer.
                let mut weighted_confidence = 0.0f32;
                let mut total_tokens = 0u32;

                for i in 0..num_segments {
                    let Some(segment) = state.get_segment(i) else {
                        continue;
                    };
                    let segment_text = match segment.to_str_lossy() {
                        Ok(text) => text.into_owned(),
                        Err(_) => continue,
                    };

                    // Real decoder confidence: mean probability of the
                    // segment's text tokens (specials excluded), discounted
                    // by the decoder's own no-speech probability for the
                    // window this segment came from.
                    let mut prob_sum = 0.0f32;
                    let mut token_count = 0u32;
                    for t in 0..segment.n_tokens() {
                        let Some(token) = segment.get_token(t) else {
                            continue;
                        };
                        // Skip special/control tokens ("<|...|>", "[_...]")
                        // — they carry near-1.0 probabilities that would
                        // inflate the average.
                        let is_special = token
                            .to_str()
                            .map(|s| s.starts_with("<|") || s.starts_with("[_"))
                            .unwrap_or(true);
                        if is_special {
                            continue;
                        }
                        prob_sum += token.token_probability();
                        token_count += 1;
                    }

                    let no_speech = segment.no_speech_probability().clamp(0.0, 1.0);
                    if token_count > 0 {
                        let avg_p = prob_sum / token_count as f32;
                        let segment_confidence = avg_p * (1.0 - no_speech);
                        weighted_confidence += segment_confidence * token_count as f32;
                        total_tokens += token_count;
                    }

                    let cleaned_text = segment_text.trim();
                    if !cleaned_text.is_empty() {
                        if !result.is_empty() {
                            result.push(' ');
                        }
                        result.push_str(cleaned_text);
                    }
                }

                Ok((result, weighted_confidence, total_tokens))
            })
            .await
            .map_err(|e| anyhow!("Transcription task panicked: {}", e))??;

        let final_result = result.trim().to_string();
        let cleaned_result = Self::clean_repetitive_text(&final_result);

        let avg_confidence = if segment_count > 0 {
            (total_confidence / segment_count as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        Ok((cleaned_result, avg_confidence, is_partial))
    }

    pub async fn get_models_directory(&self) -> PathBuf {
        self.models_dir.clone()
    }

    pub async fn delete_model(&self, model_name: &str) -> Result<String> {
        log::info!("Attempting to delete model: {}", model_name);

        // Get model info to find the file path
        let model_info = {
            let models = self.available_models.read().await;
            models.get(model_name).cloned()
        };

        let model_info = model_info.ok_or_else(|| anyhow!("Model '{}' not found", model_name))?;

        // Check if model is corrupted before allowing deletion
        log::info!("Model '{}' has status: {:?}", model_name, model_info.status);
        match &model_info.status {
            ModelStatus::Corrupted {
                file_size,
                expected_min_size,
            } => {
                log::info!(
                    "Deleting corrupted model '{}' (file size: {} bytes, expected min: {} bytes)",
                    model_name,
                    file_size,
                    expected_min_size
                );

                // Delete the file
                if model_info.path.exists() {
                    fs::remove_file(&model_info.path).await.map_err(|e| {
                        anyhow!(
                            "Failed to delete file '{}': {}",
                            model_info.path.display(),
                            e
                        )
                    })?;
                    log::info!(
                        "Successfully deleted corrupted file: {}",
                        model_info.path.display()
                    );
                } else {
                    log::warn!(
                        "File '{}' does not exist, nothing to delete",
                        model_info.path.display()
                    );
                }

                // Update model status to Missing
                {
                    let mut models = self.available_models.write().await;
                    if let Some(model) = models.get_mut(model_name) {
                        model.status = ModelStatus::Missing;
                    }
                }

                Ok(format!(
                    "Successfully deleted corrupted model '{}'",
                    model_name
                ))
            }
            ModelStatus::Available => {
                // Allow deletion of available models for testing/cleanup
                log::info!("Deleting available model '{}' (for cleanup)", model_name);

                if model_info.path.exists() {
                    fs::remove_file(&model_info.path).await.map_err(|e| {
                        anyhow!(
                            "Failed to delete file '{}': {}",
                            model_info.path.display(),
                            e
                        )
                    })?;
                    log::info!(
                        "Successfully deleted available model file: {}",
                        model_info.path.display()
                    );
                } else {
                    log::warn!(
                        "File '{}' does not exist, nothing to delete",
                        model_info.path.display()
                    );
                }

                // Update model status to Missing
                {
                    let mut models = self.available_models.write().await;
                    if let Some(model) = models.get_mut(model_name) {
                        model.status = ModelStatus::Missing;
                    }
                }

                Ok(format!("Successfully deleted model '{}'", model_name))
            }
            _ => Err(anyhow!(
                "Can only delete corrupted or available models. Model '{}' has status: {:?}",
                model_name,
                model_info.status
            )),
        }
    }

    pub async fn download_model(
        &self,
        model_name: &str,
        progress_callback: Option<Box<dyn Fn(u8) + Send>>,
    ) -> Result<()> {
        log::info!("Starting download for model: {}", model_name);

        // Atomically check-and-insert so two concurrent calls for the same
        // model can't both pass the check and race to write the same file.
        if self.download_guard.begin(model_name).await.is_err() {
            log::warn!("Download already in progress for model: {}", model_name);
            return Err(anyhow!(
                "Download already in progress for model: {}",
                model_name
            ));
        }

        // Official ggerganov/whisper.cpp model URLs from Hugging Face
        let model_url = match model_name {
            // Standard f16 models
            "tiny" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
            "base" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
            "small" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
            "medium" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
            "large-v3-turbo" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
            "large-v3" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",

            // Q5_1 quantized models
            "tiny-q5_1" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny-q5_1.bin",
            "base-q5_1" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base-q5_1.bin",
            "small-q5_1" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small-q5_1.bin",

            // Q5_0 quantized models
            "medium-q5_0" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium-q5_0.bin",
            "large-v3-turbo-q5_0" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
            "large-v3-q5_0" => "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-q5_0.bin",

            _ => {
                self.download_guard.finish(model_name).await;
                return Err(anyhow!("Unsupported model: {}", model_name));
            }
        };

        log::info!("Model URL for {}: {}", model_name, model_url);

        // Generate correct filename - all models follow ggml-{model_name}.bin pattern
        let filename = format!("ggml-{}.bin", model_name);
        let file_path = self.models_dir.join(&filename);

        log::info!("Downloading to file path: {}", file_path.display());

        // Create models directory if it doesn't exist
        if !self.models_dir.exists() {
            fs::create_dir_all(&self.models_dir)
                .await
                .map_err(|e| anyhow!("Failed to create models directory: {}", e))?;
        }

        // Update model status to downloading
        {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                model_info.status = ModelStatus::Downloading { progress: 0 };
            }
        }

        let client = Client::new();

        // The shared downloader's progress callback is sync (it can't
        // `.await`), so the in-memory status mirror is updated via
        // `try_write` here — best-effort, since the callback also fires the
        // caller-supplied `progress_callback` (which drives the Tauri event
        // the frontend actually renders) on every tick regardless.
        let models_for_progress = self.available_models.clone();
        let model_name_for_progress = model_name.to_string();

        let result = crate::utils::download::download_file(
            crate::utils::download::DownloadRequest {
                client: &client,
                url: model_url,
                dest_path: &file_path,
                // Gains Range-based resume "for free" from the shared
                // downloader; previously this always started from scratch.
                resume: true,
                chunk_timeout: None,
            },
            &self.download_guard,
            model_name,
            move |downloaded, total, _speed_mbps| {
                let progress = if total > 0 {
                    ((downloaded as f64 / total as f64) * 100.0).min(100.0) as u8
                } else {
                    0
                };

                if let Ok(mut models) = models_for_progress.try_write() {
                    if let Some(model_info) = models.get_mut(&model_name_for_progress) {
                        model_info.status = ModelStatus::Downloading { progress };
                    }
                }

                if let Some(ref callback) = progress_callback {
                    callback(progress);
                }
            },
        )
        .await;

        match result {
            Ok(outcome) => {
                log::info!(
                    "Download completed for model: {} ({} bytes{})",
                    model_name,
                    outcome.downloaded_bytes,
                    if outcome.resumed { ", resumed" } else { "" }
                );

                // Validate the completed file's header before trusting it as
                // usable (reuses the same GGML/GGUF magic-number check
                // discover_models runs).
                if let Err(e) = validate_model_file_magic(&file_path) {
                    log::warn!("Downloaded model {} failed validation: {}", model_name, e);
                    if let Err(remove_err) = fs::remove_file(&file_path).await {
                        log::warn!("Failed to clean up invalid download file: {}", remove_err);
                    }
                    {
                        let mut models = self.available_models.write().await;
                        if let Some(model_info) = models.get_mut(model_name) {
                            model_info.status = ModelStatus::Missing;
                        }
                    }
                    self.download_guard.finish(model_name).await;
                    return Err(anyhow!(
                        "Downloaded model {} failed validation: {}",
                        model_name,
                        e
                    ));
                }

                // Update model status to available
                {
                    let mut models = self.available_models.write().await;
                    if let Some(model_info) = models.get_mut(model_name) {
                        model_info.status = ModelStatus::Available;
                        model_info.path = file_path.clone();
                    }
                }

                self.download_guard.finish(model_name).await;
                Ok(())
            }
            Err(e) => {
                self.download_guard.finish(model_name).await;

                if crate::utils::download::is_cancelled(&e) {
                    // Status is already reset by cancel_download() (which
                    // runs concurrently to set it); nothing else to do here.
                    return Err(anyhow!("Download cancelled by user"));
                }

                // Any other failure (bad HTTP status, network error, stalled
                // stream, truncated download) leaves the model retryable
                // instead of stuck showing "Downloading".
                {
                    let mut models = self.available_models.write().await;
                    if let Some(model_info) = models.get_mut(model_name) {
                        model_info.status = ModelStatus::Missing;
                    }
                }
                Err(e)
            }
        }
    }

    pub async fn cancel_download(&self, model_name: &str) -> Result<()> {
        log::info!("Cancelling download for model: {}", model_name);

        // Set cancellation flag to interrupt the download loop
        self.download_guard.request_cancel(model_name).await;

        // Update model status to Missing (so it can be retried)
        {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                model_info.status = ModelStatus::Missing;
            }
        }

        // Clean up partially downloaded files
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await; // Brief delay to let download loop detect cancellation

        let filename = format!("ggml-{}.bin", model_name);
        let file_path = self.models_dir.join(&filename);
        if file_path.exists() {
            if let Err(e) = fs::remove_file(&file_path).await {
                log::warn!("Failed to clean up cancelled download file: {}", e);
            } else {
                log::info!(
                    "Cleaned up cancelled download file: {}",
                    file_path.display()
                );
            }
        }

        Ok(())
    }
}

/// whisper.cpp refuses (with zero segments, not an error) any input shorter
/// than 1 s of 16 kHz audio. Pad with trailing silence to a little over that
/// so the mel window is comfortably inside the decoder's minimum.
pub(crate) const WHISPER_MIN_INPUT_SAMPLES: usize = 16_000 + 1_600; // 1.1 s @ 16 kHz

pub(crate) fn pad_to_min_whisper_input(mut audio: Vec<f32>) -> Vec<f32> {
    if audio.len() < WHISPER_MIN_INPUT_SAMPLES {
        audio.resize(WHISPER_MIN_INPUT_SAMPLES, 0.0);
    }
    audio
}

#[cfg(test)]
mod min_input_tests {
    use super::*;

    #[test]
    fn pads_short_input_to_floor() {
        let out = pad_to_min_whisper_input(vec![0.5; 4_000]);
        assert_eq!(out.len(), WHISPER_MIN_INPUT_SAMPLES);
        assert_eq!(out[3_999], 0.5);
        assert_eq!(out[4_000], 0.0);
    }

    #[test]
    fn leaves_long_input_untouched() {
        let input = vec![0.25; 40_000];
        let out = pad_to_min_whisper_input(input.clone());
        assert_eq!(out, input);
    }
}

#[cfg(test)]
mod state_pool_tests {
    use super::{pool_get_or_insert, AsyncMutex, DecodePurpose};
    use std::collections::HashMap;
    use std::sync::Arc;

    // No real whisper model is available in this environment, so these
    // exercise the pooling algorithm (`pool_get_or_insert`) against a
    // trivial stand-in type rather than an actual `WhisperState` — it's
    // generic over the pooled value, so the behavior under test (distinct
    // entries per purpose, same entry on repeated calls, lazy construction)
    // is identical either way.
    #[tokio::test]
    async fn distinct_states_per_purpose() {
        let mut pool: HashMap<DecodePurpose, Arc<AsyncMutex<u32>>> = HashMap::new();
        let final_state =
            pool_get_or_insert(&mut pool, DecodePurpose::Final, || Ok::<_, ()>(1)).unwrap();
        let partial_state =
            pool_get_or_insert(&mut pool, DecodePurpose::Partial, || Ok::<_, ()>(2)).unwrap();

        assert!(!Arc::ptr_eq(&final_state, &partial_state));
        assert_eq!(*final_state.lock().await, 1);
        assert_eq!(*partial_state.lock().await, 2);
    }

    #[tokio::test]
    async fn same_state_on_repeated_calls() {
        let mut pool: HashMap<DecodePurpose, Arc<AsyncMutex<u32>>> = HashMap::new();
        let mut construct_calls = 0;

        let first = pool_get_or_insert(&mut pool, DecodePurpose::Final, || {
            construct_calls += 1;
            Ok::<_, ()>(42)
        })
        .unwrap();
        let second = pool_get_or_insert(&mut pool, DecodePurpose::Final, || {
            construct_calls += 1;
            Ok::<_, ()>(99) // would prove reuse if this ever got called again
        })
        .unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(construct_calls, 1, "constructor should only run once");
        assert_eq!(*second.lock().await, 42);
    }

    #[test]
    fn default_purpose_is_final() {
        assert_eq!(DecodePurpose::default(), DecodePurpose::Final);
    }
}

#[cfg(test)]
mod thread_budget_tests {
    use super::effective_threads;

    #[test]
    fn no_request_keeps_default() {
        assert_eq!(effective_threads(8, None), 8);
    }

    #[test]
    fn request_below_default_is_honored() {
        assert_eq!(effective_threads(8, Some(3)), 3);
    }

    #[test]
    fn request_above_default_is_capped_at_default() {
        assert_eq!(effective_threads(4, Some(100)), 4);
    }

    #[test]
    fn zero_or_negative_request_floors_at_one() {
        assert_eq!(effective_threads(8, Some(0)), 1);
        assert_eq!(effective_threads(8, Some(-5)), 1);
    }

    #[test]
    fn partial_worker_half_budget_example() {
        // Mirrors partial_worker.rs: max(1, adaptive_threads / 2).
        let adaptive_threads = 6;
        let half = (adaptive_threads / 2).max(1);
        assert_eq!(effective_threads(adaptive_threads, Some(half)), 3);
    }

    #[test]
    fn low_tier_single_core_floor() {
        // Low-tier hardware default is already 2; half-budget request of 1
        // should not be crushed further than the floor.
        assert_eq!(effective_threads(2, Some(1)), 1);
    }
}
