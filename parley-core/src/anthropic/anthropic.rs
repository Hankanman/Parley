use crate::llm_providers::{ListerConfig, ModelLister};
use once_cell::sync::Lazy;
use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Anthropic (Claude) model information returned to frontend
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AnthropicModel {
    pub id: String,
    pub display_name: Option<String>,
}

/// API response model from Anthropic
#[derive(Debug, Deserialize)]
struct AnthropicApiModel {
    id: String,
    display_name: Option<String>,
    #[allow(dead_code)]
    created_at: Option<String>,
}

/// API response wrapper from Anthropic
#[derive(Debug, Deserialize)]
struct AnthropicApiResponse {
    data: Vec<AnthropicApiModel>,
}

/// Fallback models when API fetch fails (matches frontend hardcoded values)
const FALLBACK_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4-5-20250929", "Claude 4.5 Sonnet"),
    ("claude-haiku-4-5-20251001", "Claude 4.5 Haiku"),
    ("claude-opus-4-1-20250805", "Claude 4.1 Opus"),
    ("claude-sonnet-4-20250514", "Claude 4 Sonnet"),
];

fn fallback_models() -> Vec<AnthropicModel> {
    FALLBACK_MODELS
        .iter()
        .map(|(id, name)| AnthropicModel {
            id: id.to_string(),
            display_name: Some(name.to_string()),
        })
        .collect()
}

/// Check if model is a chat-capable model
fn is_chat_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    // Include Claude models only
    id.starts_with("claude-")
}

fn apply_auth(rb: RequestBuilder, api_key: &str) -> RequestBuilder {
    rb.header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
}

fn parse_response(bytes: &[u8]) -> Result<Vec<AnthropicModel>, String> {
    let api_response: AnthropicApiResponse =
        serde_json::from_slice(bytes).map_err(|e| e.to_string())?;

    Ok(api_response
        .data
        .into_iter()
        .filter(|m| is_chat_model(&m.id))
        .map(|m| AnthropicModel {
            id: m.id,
            display_name: m.display_name,
        })
        .collect())
}

static LISTER: Lazy<ModelLister<AnthropicModel>> = Lazy::new(|| {
    ModelLister::new(ListerConfig {
        provider: "Anthropic",
        endpoint: "https://api.anthropic.com/v1/models",
        ttl: Duration::from_secs(300),
        timeout: Duration::from_secs(5),
        requires_api_key: true,
        apply_auth,
        parse: parse_response,
        fallback: fallback_models,
    })
});

/// Fetch Anthropic models from API
///
/// # Arguments
/// * `api_key` - Anthropic API key
///
/// # Returns
/// Vector of available models, or fallback models on error
pub async fn get_anthropic_models(api_key: Option<String>) -> Result<Vec<AnthropicModel>, String> {
    LISTER.list(api_key).await
}

/// Clear the cached Anthropic model list.
pub fn clear_cache() {
    LISTER.clear();
}
