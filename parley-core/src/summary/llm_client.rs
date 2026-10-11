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
    /// OpenAI-style `response_format` (JSON mode / JSON-schema structured
    /// output). Only set for [`generate_structured`] calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
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
    /// Tool definitions — structured output on Claude is a single forced
    /// tool whose `input_schema` is the response schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

// Claude-specific response structure
#[derive(Deserialize, Debug)]
pub struct ClaudeChatResponse {
    pub content: Vec<ClaudeChatContent>,
}

/// One Claude content block: `text` for prose, `tool_use` (with `input`)
/// for the forced structured-output tool.
#[derive(Deserialize, Debug)]
pub struct ClaudeChatContent {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
}

/// A JSON Schema a structured call's response must satisfy.
#[derive(Debug, Clone, Copy)]
pub struct ResponseSchema<'a> {
    /// Short identifier (`[a-zA-Z0-9_-]`) — OpenAI's `json_schema.name` and
    /// Claude's tool name.
    pub name: &'a str,
    pub schema: &'a serde_json::Value,
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

        // A model name left over from another provider (e.g. "gpt-5" after
        // switching to built-in AI) would otherwise fail every call — and
        // with no context size to go on, chunk the transcript to dust first.
        if provider == LLMProvider::BuiltInAI
            && crate::summary::summary_engine::models::get_model_by_name(model_name).is_none()
        {
            return Err(format!(
                "\"{model_name}\" isn't a built-in model. Choose one in Settings → Summary."
            ));
        }

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
        match self.provider {
            LLMProvider::Ollama => {
                match crate::ollama::metadata::METADATA_CACHE
                    .get_or_fetch(&self.model, self.ollama_endpoint.as_deref())
                    .await
                {
                    Ok(metadata) => {
                        let optimal = single_pass_threshold(metadata.context_size as usize);
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
                        single_pass_threshold(model_def.context_size as usize)
                    }
                    None => {
                        warn!("Unknown built-in model {}, using default 2048", self.model);
                        single_pass_threshold(2048)
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

/// Reserved for the prompt scaffolding around the transcript.
const PROMPT_OVERHEAD: usize = 300;

/// Headroom kept free on top of prompt and output (mirrors the built-in
/// engine's `SAFETY_BUFFER_TOKENS` in `pick_context_size`).
const CONTEXT_SAFETY_BUFFER: usize = 1024;

/// Largest transcript that fits a model context window in one call: the
/// window minus prompt scaffolding, the generated output (which shares the
/// window) and a safety buffer. Anything longer must go through map-reduce,
/// otherwise generation runs out of context mid-summary.
fn single_pass_threshold(context_size: usize) -> usize {
    use crate::summary::summary_engine::models::DEFAULT_MAX_TOKENS;
    context_size
        .saturating_sub(PROMPT_OVERHEAD)
        .saturating_sub(DEFAULT_MAX_TOKENS as usize)
        .saturating_sub(CONTEXT_SAFETY_BUFFER)
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
    generate_completion(
        config,
        system_prompt,
        user_prompt,
        None,
        cancellation_token,
        on_delta,
    )
    .await
}

/// Like [`generate_summary`], but asks the provider to constrain its output
/// to `schema`:
///
/// - **Built-in AI**: the sidecar compiles the schema to a llama.cpp GBNF
///   grammar, so sampling can only produce matching JSON.
/// - **OpenAI / OpenRouter / Ollama / custom OpenAI-compatible**:
///   `response_format: {type: "json_schema", strict: true}` (Ollama's `/v1`
///   endpoint maps this onto its native `format` schema).
/// - **Groq**: JSON mode (`json_object`) — schema support varies by model.
/// - **Claude**: a single forced tool whose `input_schema` is the schema;
///   the tool call's `input` is returned as JSON text.
///
/// Returns the raw JSON text. It is NOT validated here — callers parse and
/// validate it, and must treat it as untrusted (transcripts can steer it).
/// An error means the provider rejected the request (e.g. a model without
/// structured-output support); callers fall back to free-form generation.
pub async fn generate_structured(
    config: &LlmConfig,
    system_prompt: &str,
    user_prompt: &str,
    schema: ResponseSchema<'_>,
    cancellation_token: Option<&CancellationToken>,
    on_delta: Option<StreamSink<'_>>,
) -> Result<String, String> {
    generate_completion(
        config,
        system_prompt,
        user_prompt,
        Some(schema),
        cancellation_token,
        on_delta,
    )
    .await
}

async fn generate_completion(
    config: &LlmConfig,
    system_prompt: &str,
    user_prompt: &str,
    schema: Option<ResponseSchema<'_>>,
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

        let schema_text = match schema {
            Some(s) => Some(
                serde_json::to_string(s.schema)
                    .map_err(|e| format!("failed to serialize response schema: {e}"))?,
            ),
            None => None,
        };
        return crate::summary::summary_engine::generate_with_builtin(
            app_data_dir,
            &config.model,
            system_prompt,
            user_prompt,
            cancellation_token,
            on_delta,
            schema_text.as_deref(),
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

        match send_chat_request(config, system_prompt, user_prompt, schema, cancellation_token)
            .await
        {
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
    schema: Option<ResponseSchema<'_>>,
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
            response_format: schema.map(|s| openai_response_format(provider, s)),
        })
    } else {
        let (tools, tool_choice) = match schema {
            Some(s) => {
                let (tools, choice) = claude_forced_tool(s);
                (Some(tools), Some(choice))
            }
            None => (None, None),
        };
        serde_json::json!(ClaudeRequest {
            system: system_prompt.to_string(),
            model: config.model.clone(),
            max_tokens: config.max_tokens.unwrap_or(CLAUDE_DEFAULT_MAX_TOKENS),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            }],
            tools,
            tool_choice,
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

        claude_response_text(chat_response, schema.is_some()).map_err(fatal)
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

/// The OpenAI-style `response_format` for a structured call. Groq only gets
/// JSON mode: its json_schema support is per-model and errors on the rest,
/// while JSON mode works everywhere (the schema is also spelled out in the
/// prompt, and the result is validated either way).
fn openai_response_format(provider: LLMProvider, schema: ResponseSchema<'_>) -> serde_json::Value {
    if provider == LLMProvider::Groq {
        return serde_json::json!({ "type": "json_object" });
    }
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": schema.name,
            "strict": true,
            "schema": schema.schema,
        }
    })
}

/// Claude structured output: one tool whose input schema is the response
/// schema, and a `tool_choice` forcing the model to call it.
fn claude_forced_tool(schema: ResponseSchema<'_>) -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!([{
            "name": schema.name,
            "description": "Record the structured meeting summary.",
            "input_schema": schema.schema,
        }]),
        serde_json::json!({ "type": "tool", "name": schema.name }),
    )
}

/// Pull the answer out of a Claude response: the forced tool call's `input`
/// (serialized back to JSON text) for structured calls, else the first text
/// block.
fn claude_response_text(response: ClaudeChatResponse, structured: bool) -> Result<String, String> {
    if structured {
        if let Some(input) = response
            .content
            .iter()
            .find(|c| c.kind == "tool_use")
            .and_then(|c| c.input.as_ref())
        {
            return serde_json::to_string(input)
                .map_err(|e| format!("Failed to serialize tool input: {e}"));
        }
    }
    response
        .content
        .iter()
        .find_map(|c| c.text.as_deref())
        .map(|t| t.trim().to_string())
        .ok_or_else(|| "No content in LLM response".to_string())
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

#[cfg(test)]
mod structured_request_tests {
    use super::*;

    #[test]
    fn openai_format_uses_strict_json_schema() {
        let schema = serde_json::json!({"type": "object"});
        let rf = openai_response_format(
            LLMProvider::OpenAI,
            ResponseSchema { name: "meeting_summary", schema: &schema },
        );
        assert_eq!(rf["type"], "json_schema");
        assert_eq!(rf["json_schema"]["strict"], true);
        assert_eq!(rf["json_schema"]["name"], "meeting_summary");
        assert_eq!(rf["json_schema"]["schema"], schema);
    }

    #[test]
    fn groq_gets_plain_json_mode() {
        let schema = serde_json::json!({"type": "object"});
        let rf = openai_response_format(
            LLMProvider::Groq,
            ResponseSchema { name: "x", schema: &schema },
        );
        assert_eq!(rf, serde_json::json!({"type": "json_object"}));
    }

    #[test]
    fn claude_structured_reads_tool_input() {
        let response: ClaudeChatResponse = serde_json::from_value(serde_json::json!({
            "content": [
                {"type": "text", "text": "Sure."},
                {"type": "tool_use", "id": "t1", "name": "meeting_summary",
                 "input": {"title": "Q3 planning"}}
            ]
        }))
        .unwrap();
        let text = claude_response_text(response, true).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["title"],
            "Q3 planning"
        );
    }

    #[test]
    fn claude_plain_reads_first_text_block() {
        let response: ClaudeChatResponse = serde_json::from_value(serde_json::json!({
            "content": [{"type": "text", "text": "  # Title\nBody  "}]
        }))
        .unwrap();
        assert_eq!(claude_response_text(response, false).unwrap(), "# Title\nBody");
    }

    #[test]
    fn plain_requests_omit_structured_fields() {
        let body = serde_json::json!(ChatRequest {
            model: "m".into(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            response_format: None,
        });
        assert!(body.get("response_format").is_none());
    }
}

#[cfg(test)]
mod threshold_tests {
    use super::*;

    #[test]
    fn threshold_reserves_output_and_safety_buffer() {
        // 32768 - 300 (prompt) - 4096 (output) - 1024 (safety)
        assert_eq!(single_pass_threshold(32768), 27348);
        assert_eq!(single_pass_threshold(65536), 60116);
    }

    #[test]
    fn thirty_k_transcript_is_not_single_pass_at_32k_ctx() {
        let threshold = single_pass_threshold(32768);
        // Processor goes single-pass only when total_tokens < threshold.
        assert!(!(30_000 < threshold));
        assert!(30_000 < single_pass_threshold(65536));
    }

    #[test]
    fn tiny_context_does_not_underflow() {
        assert_eq!(single_pass_threshold(2048), 0);
    }
}
