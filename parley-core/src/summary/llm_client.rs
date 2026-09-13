//! Provider-agnostic LLM client.
//!
//! [`LlmConfig`] bundles everything one generation call needs — provider,
//! model, credentials, endpoints, sampling — and [`LlmConfig::resolve`] is
//! the single place that reads them from settings, shared by the summary
//! service, both action-item extractors, and the live extractor.
//! [`generate_summary`] executes one call against whichever backend the
//! config names, with bounded retry on transient failures.

use crate::database::repositories::setting::SettingsRepository;
use once_cell::sync::Lazy;
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const REQUEST_TIMEOUT_DURATION: Duration = Duration::from_secs(300);

/// Attempts per call: the first try plus two retries on transient failures
/// (connection errors, 429, 5xx). Timeouts are not retried — at 300s each,
/// stacking them would hold the user's summary hostage for 15 minutes.
const MAX_ATTEMPTS: u32 = 3;

/// Claude requires an explicit `max_tokens`; this default is high enough
/// that a full multi-section meeting report never truncates mid-sentence,
/// and every current Claude model supports at least this much output.
const CLAUDE_DEFAULT_MAX_TOKENS: u32 = 8192;

/// One shared HTTP client for every LLM call in the process — connection
/// pooling actually works this way, instead of each caller building a
/// fresh `Client` per request.
static HTTP_CLIENT: Lazy<Client> = Lazy::new(Client::new);

// Generic structure for OpenAI-compatible API chat messages
#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

// Generic structure for OpenAI-compatible API chat requests
#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
}

// Generic structure for OpenAI-compatible API chat responses
#[derive(Deserialize, Debug)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
}

#[derive(Deserialize, Debug)]
pub struct Choice {
    pub message: MessageContent,
}

#[derive(Deserialize, Debug)]
pub struct MessageContent {
    pub content: String,
}

// Claude-specific request structure
#[derive(Debug, Serialize)]
pub struct ClaudeRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: String,
    pub messages: Vec<ChatMessage>,
}

// Claude-specific response structure
#[derive(Deserialize, Debug)]
pub struct ClaudeChatResponse {
    pub content: Vec<ClaudeChatContent>,
}

#[derive(Deserialize, Debug)]
pub struct ClaudeChatContent {
    pub text: String,
}

/// LLM Provider enumeration for multi-provider support
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LLMProvider {
    OpenAI,
    Claude,
    Groq,
    Ollama,
    OpenRouter,
    BuiltInAI,
    CustomOpenAI,
}

impl LLMProvider {
    /// Parse provider from string (case-insensitive)
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "openai" => Ok(Self::OpenAI),
            "claude" => Ok(Self::Claude),
            "groq" => Ok(Self::Groq),
            "ollama" => Ok(Self::Ollama),
            "openrouter" => Ok(Self::OpenRouter),
            "builtin-ai" | "local-llama" | "localllama" => Ok(Self::BuiltInAI),
            "custom-openai" => Ok(Self::CustomOpenAI),
            _ => Err(format!("Unsupported LLM provider: {}", s)),
        }
    }
}

/// Everything one LLM call needs, resolved once from settings.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub provider: LLMProvider,
    pub model: String,
    pub api_key: String,
    pub ollama_endpoint: Option<String>,
    pub custom_openai_endpoint: Option<String>,
    /// Completion budget. Set from the CustomOpenAI config when that
    /// provider is active; `None` lets the provider default apply (Claude
    /// substitutes [`CLAUDE_DEFAULT_MAX_TOKENS`], which it requires).
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// Required by the BuiltInAI provider to locate its GGUF models.
    pub app_data_dir: Option<PathBuf>,
}

impl LlmConfig {
    /// Resolve provider credentials and endpoints from settings. The single
    /// source of truth for "which endpoint, with which key" — every caller
    /// (summary service, extractors, live extractor) goes through here so
    /// they can never disagree.
    pub async fn resolve(
        pool: &SqlitePool,
        provider_name: &str,
        model_name: &str,
        app_data_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        let provider = LLMProvider::from_str(provider_name)?;

        let keyless = matches!(
            provider,
            LLMProvider::Ollama | LLMProvider::BuiltInAI | LLMProvider::CustomOpenAI
        );
        let api_key = if keyless {
            String::new()
        } else {
            SettingsRepository::get_api_key(pool, provider_name)
                .await
                .map_err(|e| format!("failed to read API key for {provider_name}: {e}"))?
                .filter(|k| !k.is_empty())
                .ok_or_else(|| format!("API key not found for {provider_name}"))?
        };

        let ollama_endpoint = if provider == LLMProvider::Ollama {
            SettingsRepository::get_model_config(pool)
                .await
                .ok()
                .flatten()
                .and_then(|c| c.ollama_endpoint)
        } else {
            None
        };

        let (custom_openai_endpoint, api_key, max_tokens, temperature, top_p) =
            if provider == LLMProvider::CustomOpenAI {
                let config = SettingsRepository::get_custom_openai_config(pool)
                    .await
                    .map_err(|e| format!("failed to read custom OpenAI config: {e}"))?
                    .ok_or_else(|| {
                        "custom OpenAI provider selected but not configured".to_string()
                    })?;
                (
                    Some(config.endpoint),
                    config.api_key.unwrap_or_default(),
                    config.max_tokens.map(|t| t as u32),
                    config.temperature,
                    config.top_p,
                )
            } else {
                (None, api_key, None, None, None)
            };

        Ok(Self {
            provider,
            model: model_name.to_string(),
            api_key,
            ollama_endpoint,
            custom_openai_endpoint,
            max_tokens,
            temperature,
            top_p,
            app_data_dir,
        })
    }

    /// Largest transcript (in rough tokens) sent in a single call before the
    /// processor switches to map-reduce chunking.
    ///
    /// Local providers are sized from real model metadata. Cloud providers
    /// get per-provider values reflecting the *smallest* context a user is
    /// likely to hit there — chunking a long transcript is recoverable,
    /// overflowing the model's window is a hard API error.
    pub async fn token_threshold(&self) -> usize {
        /// Reserved for the prompt scaffolding around the transcript.
        const PROMPT_OVERHEAD: usize = 300;

        match self.provider {
            LLMProvider::Ollama => {
                match crate::ollama::metadata::METADATA_CACHE
                    .get_or_fetch(&self.model, self.ollama_endpoint.as_deref())
                    .await
                {
                    Ok(metadata) => {
                        let optimal = metadata.context_size.saturating_sub(PROMPT_OVERHEAD);
                        info!(
                            "✓ Using dynamic context for {}: {} tokens (chunk size: {})",
                            self.model, metadata.context_size, optimal
                        );
                        optimal
                    }
                    Err(e) => {
                        warn!(
                            "Failed to fetch context for {}: {}. Using default 4000",
                            self.model, e
                        );
                        4000
                    }
                }
            }
            LLMProvider::BuiltInAI => {
                use crate::summary::summary_engine::models;
                match models::get_model_by_name(&self.model) {
                    Some(model_def) => {
                        (model_def.context_size as usize).saturating_sub(PROMPT_OVERHEAD)
                    }
                    None => {
                        warn!("Unknown built-in model {}, using default 2048", self.model);
                        2048 - PROMPT_OVERHEAD
                    }
                }
            }
            // Cloud providers: no per-model metadata endpoint, so use the
            // smallest context a model on that provider realistically has.
            LLMProvider::OpenAI => 100_000,
            LLMProvider::Claude => 150_000,
            LLMProvider::OpenRouter => 60_000,
            // Groq hosts models from 8k (gemma2) up to 128k; 8k is the only
            // value safe for all of them, and Groq is fast enough that the
            // extra chunking passes are cheap.
            LLMProvider::Groq => 8_000,
            // Unknown server — commonly a local llama.cpp/vLLM with a
            // limited window. Conservative beats a hard overflow.
            LLMProvider::CustomOpenAI => 32_000,
        }
    }
}

/// Outcome classification for one HTTP attempt.
struct AttemptError {
    message: String,
    retryable: bool,
}

/// Callback receiving incremental output text during generation.
///
/// Only providers that stream honor it — currently BuiltInAI, whose sidecar
/// emits token chunks as it generates. HTTP providers ignore the sink and
/// deliver everything at once; callers must treat the returned String as
/// authoritative either way.
pub type StreamSink<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// Generates one completion using the configured provider, retrying
/// transient failures (connection errors, 429, 5xx) up to [`MAX_ATTEMPTS`]
/// with backoff. Timeouts and other 4xx errors fail immediately.
///
/// # Returns
/// The generated text or an error message.
pub async fn generate_summary(
    config: &LlmConfig,
    system_prompt: &str,
    user_prompt: &str,
    cancellation_token: Option<&CancellationToken>,
    on_delta: Option<StreamSink<'_>>,
) -> Result<String, String> {
    if let Some(token) = cancellation_token {
        if token.is_cancelled() {
            return Err("Summary generation was cancelled".to_string());
        }
    }

    // BuiltInAI runs through the local sidecar, which has its own failure
    // modes (deterministic, not transient) — no retry loop.
    if config.provider == LLMProvider::BuiltInAI {
        let app_data_dir = config
            .app_data_dir
            .as_ref()
            .ok_or_else(|| "app_data_dir is required for BuiltInAI provider".to_string())?;

        return crate::summary::summary_engine::generate_with_builtin(
            app_data_dir,
            &config.model,
            system_prompt,
            user_prompt,
            cancellation_token,
            on_delta,
        )
        .await
        .map_err(|e| e.to_string());
    }

    for attempt in 1..=MAX_ATTEMPTS {
        if let Some(token) = cancellation_token {
            if token.is_cancelled() {
                return Err("Summary generation was cancelled".to_string());
            }
        }

        match send_chat_request(config, system_prompt, user_prompt, cancellation_token).await {
            Ok(text) => return Ok(text),
            Err(e) if e.retryable && attempt < MAX_ATTEMPTS => {
                let delay = Duration::from_secs(2u64.pow(attempt - 1));
                warn!(
                    "LLM attempt {}/{} failed ({}); retrying in {:?}",
                    attempt, MAX_ATTEMPTS, e.message, delay
                );
                if let Some(token) = cancellation_token {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = token.cancelled() => {
                            return Err("Summary generation was cancelled".to_string());
                        }
                    }
                } else {
                    tokio::time::sleep(delay).await;
                }
            }
            Err(e) => return Err(e.message),
        }
    }
    unreachable!("retry loop always returns")
}

/// One HTTP round trip: build the provider-specific request, send it, parse
/// the response. Classifies failures as retryable or not.
async fn send_chat_request(
    config: &LlmConfig,
    system_prompt: &str,
    user_prompt: &str,
    cancellation_token: Option<&CancellationToken>,
) -> Result<String, AttemptError> {
    let fatal = |message: String| AttemptError {
        message,
        retryable: false,
    };

    let provider = config.provider;
    let (api_url, mut headers) = match provider {
        LLMProvider::OpenAI => (
            "https://api.openai.com/v1/chat/completions".to_string(),
            header::HeaderMap::new(),
        ),
        LLMProvider::Groq => (
            "https://api.groq.com/openai/v1/chat/completions".to_string(),
            header::HeaderMap::new(),
        ),
        LLMProvider::OpenRouter => (
            "https://openrouter.ai/api/v1/chat/completions".to_string(),
            header::HeaderMap::new(),
        ),
        LLMProvider::Ollama => {
            let host = config
                .ollama_endpoint
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            (
                format!("{}/v1/chat/completions", host),
                header::HeaderMap::new(),
            )
        }
        LLMProvider::CustomOpenAI => {
            let endpoint = config
                .custom_openai_endpoint
                .as_deref()
                .ok_or_else(|| fatal("Custom OpenAI endpoint not configured".to_string()))?;
            (
                format!("{}/chat/completions", endpoint.trim_end_matches('/')),
                header::HeaderMap::new(),
            )
        }
        LLMProvider::Claude => {
            let mut header_map = header::HeaderMap::new();
            header_map.insert(
                "x-api-key",
                config
                    .api_key
                    .parse()
                    .map_err(|_| fatal("Invalid API key format".to_string()))?,
            );
            header_map.insert(
                "anthropic-version",
                "2023-06-01"
                    .parse()
                    .map_err(|_| fatal("Invalid anthropic version".to_string()))?,
            );
            (
                "https://api.anthropic.com/v1/messages".to_string(),
                header_map,
            )
        }
        LLMProvider::BuiltInAI => {
            unreachable!("BuiltInAI is handled before the HTTP path")
        }
    };

    // Bearer auth for non-Claude providers. Skipped entirely when there's no
    // key (Ollama, keyless custom endpoints) — some servers reject an empty
    // `Bearer` credential outright.
    if provider != LLMProvider::Claude && !config.api_key.is_empty() {
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", config.api_key)
                .parse()
                .map_err(|_| fatal("Invalid authorization header".to_string()))?,
        );
    }
    headers.insert(
        header::CONTENT_TYPE,
        "application/json"
            .parse()
            .map_err(|_| fatal("Invalid content type".to_string()))?,
    );

    let request_body = if provider != LLMProvider::Claude {
        serde_json::json!(ChatRequest {
            model: config.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_prompt.to_string(),
                }
            ],
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
        })
    } else {
        serde_json::json!(ClaudeRequest {
            system: system_prompt.to_string(),
            model: config.model.clone(),
            max_tokens: config.max_tokens.unwrap_or(CLAUDE_DEFAULT_MAX_TOKENS),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            }]
        })
    };

    info!(
        "🐞 LLM Request to {}: model={}",
        provider_name(&provider),
        config.model
    );

    let request_future = HTTP_CLIENT
        .post(api_url)
        .headers(headers)
        .json(&request_body)
        .timeout(REQUEST_TIMEOUT_DURATION)
        .send();

    let map_send_error = |e: reqwest::Error| {
        if e.is_timeout() {
            AttemptError {
                message: format!(
                    "LLM request timed out after {} seconds",
                    REQUEST_TIMEOUT_DURATION.as_secs()
                ),
                // A 300s timeout is not worth stacking; fail now.
                retryable: false,
            }
        } else {
            AttemptError {
                message: format!("Failed to send request to LLM: {}", e),
                retryable: true,
            }
        }
    };

    // Race between cancellation and request completion.
    let response = if let Some(token) = cancellation_token {
        tokio::select! {
            result = request_future => result.map_err(map_send_error)?,
            _ = token.cancelled() => {
                return Err(fatal("Summary generation was cancelled".to_string()));
            }
        }
    } else {
        request_future.await.map_err(map_send_error)?
    };

    let status = response.status();
    if !status.is_success() {
        let error_body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        // Rate limits and server-side failures are transient; every other
        // 4xx (bad key, unknown model, oversized prompt) will fail the same
        // way on retry.
        let retryable = status.as_u16() == 429 || status.is_server_error();
        return Err(AttemptError {
            message: format!("LLM API request failed ({}): {}", status, error_body),
            retryable,
        });
    }

    if provider == LLMProvider::Claude {
        let chat_response = response
            .json::<ClaudeChatResponse>()
            .await
            .map_err(|e| fatal(format!("Failed to parse LLM response: {}", e)))?;

        let content = chat_response
            .content
            .first()
            .ok_or_else(|| fatal("No content in LLM response".to_string()))?
            .text
            .trim();
        Ok(content.to_string())
    } else {
        let chat_response = response
            .json::<ChatResponse>()
            .await
            .map_err(|e| fatal(format!("Failed to parse LLM response: {}", e)))?;

        let content = chat_response
            .choices
            .first()
            .ok_or_else(|| fatal("No content in LLM response".to_string()))?
            .message
            .content
            .trim();
        Ok(content.to_string())
    }
}

/// Helper function to get provider name for logging
fn provider_name(provider: &LLMProvider) -> &str {
    match provider {
        LLMProvider::OpenAI => "OpenAI",
        LLMProvider::Claude => "Claude",
        LLMProvider::Groq => "Groq",
        LLMProvider::Ollama => "Ollama",
        LLMProvider::BuiltInAI => "Built-in AI",
        LLMProvider::OpenRouter => "OpenRouter",
        LLMProvider::CustomOpenAI => "Custom OpenAI",
    }
}
