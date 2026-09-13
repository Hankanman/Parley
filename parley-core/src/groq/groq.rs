use crate::llm_providers::{ListerConfig, ModelLister};
use once_cell::sync::Lazy;
use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Groq model information returned to frontend
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GroqModel {
    pub id: String,
    pub owned_by: Option<String>,
}

/// API response model from Groq (OpenAI-compatible format)
#[derive(Debug, Deserialize)]
struct GroqApiModel {
    id: String,
    owned_by: Option<String>,
    #[allow(dead_code)]
    object: String,
}

/// API response wrapper from Groq
#[derive(Debug, Deserialize)]
struct GroqApiResponse {
    data: Vec<GroqApiModel>,
}

/// Fallback models when API fetch fails (matches frontend hardcoded values)
const FALLBACK_MODELS: &[&str] = &["llama-3.3-70b-versatile"];

fn fallback_models() -> Vec<GroqModel> {
    FALLBACK_MODELS
        .iter()
        .map(|id| GroqModel {
            id: id.to_string(),
            owned_by: None,
        })
        .collect()
}

/// Check if model is a chat-capable model (filter out whisper, etc.)
fn is_chat_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    // Exclude whisper, tool-use specific models, and embedding models
    !id.contains("whisper")
        && !id.contains("embed")
        && !id.contains("guard")
        && !id.contains("tool-use")
}

fn apply_auth(rb: RequestBuilder, api_key: &str) -> RequestBuilder {
    rb.header("Authorization", format!("Bearer {}", api_key))
}

fn parse_response(bytes: &[u8]) -> Result<Vec<GroqModel>, String> {
    let api_response: GroqApiResponse =
        serde_json::from_slice(bytes).map_err(|e| e.to_string())?;

    Ok(api_response
        .data
        .into_iter()
        .filter(|m| is_chat_model(&m.id))
        .map(|m| GroqModel {
            id: m.id,
            owned_by: m.owned_by,
        })
        .collect())
}

static LISTER: Lazy<ModelLister<GroqModel>> = Lazy::new(|| {
    ModelLister::new(ListerConfig {
        provider: "Groq",
        endpoint: "https://api.groq.com/openai/v1/models",
        ttl: Duration::from_secs(300),
        timeout: Duration::from_secs(5),
        requires_api_key: true,
        apply_auth,
        parse: parse_response,
        fallback: fallback_models,
    })
});

/// Fetch Groq models from API
///
/// # Arguments
/// * `api_key` - Groq API key
///
/// # Returns
/// Vector of available models, or fallback models on error
pub async fn get_groq_models(api_key: Option<String>) -> Result<Vec<GroqModel>, String> {
    LISTER.list(api_key).await
}

/// Clear the cached Groq model list.
pub fn clear_cache() {
    LISTER.clear();
}
