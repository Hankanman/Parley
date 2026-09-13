use crate::llm_providers::{ListerConfig, ModelLister};
use once_cell::sync::Lazy;
use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// OpenAI model information returned to frontend
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAIModel {
    pub id: String,
}

/// API response model from OpenAI
#[derive(Debug, Deserialize)]
struct OpenAIApiModel {
    id: String,
    #[allow(dead_code)]
    object: String,
    #[allow(dead_code)]
    owned_by: String,
}

/// API response wrapper from OpenAI
#[derive(Debug, Deserialize)]
struct OpenAIApiResponse {
    data: Vec<OpenAIApiModel>,
}

/// Fallback models when API fetch fails (matches frontend hardcoded values)
const FALLBACK_MODELS: &[&str] = &[
    "gpt-5",
    "gpt-5-mini",
    "gpt-4o",
    "gpt-4.1",
    "gpt-4-turbo",
    "gpt-3.5-turbo",
    "gpt-4o-2024-11-20",
    "gpt-4o-2024-08-06",
    "gpt-4o-mini-2024-07-18",
    "gpt-4.1-2025-04-14",
    "gpt-4.1-nano-2025-04-14",
    "gpt-4.1-mini-2025-04-14",
    "o4-mini-2025-04-16",
    "o3-2025-04-16",
    "o3-mini-2025-01-31",
    "o1-2024-12-17",
    "o1-mini-2024-09-12",
    "gpt-4-turbo-2024-04-09",
    "gpt-4-0125-Preview",
    "gpt-4-vision-preview",
    "gpt-4-1106-Preview",
    "gpt-3.5-turbo-0125",
    "gpt-3.5-turbo-1106",
];

fn fallback_models() -> Vec<OpenAIModel> {
    FALLBACK_MODELS
        .iter()
        .map(|id| OpenAIModel { id: id.to_string() })
        .collect()
}

/// Check if model is a chat-capable model (filter out embedding, tts, etc.)
fn is_chat_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    // Include gpt-*, o1-*, o3-*, o4-* models
    // Exclude embedding, tts, whisper, dall-e, babbage, davinci (non-chat models)
    (id.starts_with("gpt-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
        || id.starts_with("chatgpt-"))
        && !id.contains("embedding")
        && !id.contains("tts")
        && !id.contains("whisper")
        && !id.contains("dall-e")
        && !id.contains("babbage")
        && !id.contains("davinci")
        && !id.contains("instruct")
        && !id.contains("realtime")
        && !id.contains("audio")
}

fn apply_auth(rb: RequestBuilder, api_key: &str) -> RequestBuilder {
    rb.header("Authorization", format!("Bearer {}", api_key))
}

fn parse_response(bytes: &[u8]) -> Result<Vec<OpenAIModel>, String> {
    let api_response: OpenAIApiResponse =
        serde_json::from_slice(bytes).map_err(|e| e.to_string())?;

    Ok(api_response
        .data
        .into_iter()
        .filter(|m| is_chat_model(&m.id))
        .map(|m| OpenAIModel { id: m.id })
        .collect())
}

static LISTER: Lazy<ModelLister<OpenAIModel>> = Lazy::new(|| {
    ModelLister::new(ListerConfig {
        provider: "OpenAI",
        endpoint: "https://api.openai.com/v1/models",
        ttl: Duration::from_secs(300),
        timeout: Duration::from_secs(5),
        requires_api_key: true,
        apply_auth,
        parse: parse_response,
        fallback: fallback_models,
    })
});

/// Fetch OpenAI models from API
///
/// # Arguments
/// * `api_key` - OpenAI API key
///
/// # Returns
/// Vector of available models, or fallback models on error
pub async fn get_openai_models(api_key: Option<String>) -> Result<Vec<OpenAIModel>, String> {
    LISTER.list(api_key).await
}

/// Clear the cached OpenAI model list.
pub fn clear_cache() {
    LISTER.clear();
}
