use crate::llm_providers::{no_auth, ListerConfig, ModelLister};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
    pub context_length: Option<u32>,
    pub prompt_price: Option<String>,
    pub completion_price: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterApiModel {
    id: String,
    name: Option<String>,
    context_length: Option<u32>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
    #[serde(default)]
    pricing: Option<Pricing>,
}

#[derive(Debug, Deserialize, Default)]
struct TopProvider {
    context_length: Option<u32>,
}

#[derive(Debug, Deserialize, Default)]
struct Pricing {
    prompt: Option<String>,
    completion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterResponse {
    data: Vec<OpenRouterApiModel>,
}

/// Small fallback list used when the OpenRouter API can't be reached.
/// OpenRouter's catalog changes constantly, so this only needs to keep the
/// picker usable, not exhaustive.
const FALLBACK_MODELS: &[(&str, &str)] = &[
    ("openrouter/auto", "Auto (best available model)"),
    ("anthropic/claude-3.5-sonnet", "Claude 3.5 Sonnet"),
    ("openai/gpt-4o", "GPT-4o"),
    ("meta-llama/llama-3.1-70b-instruct", "Llama 3.1 70B Instruct"),
    ("google/gemini-pro-1.5", "Gemini Pro 1.5"),
];

fn fallback_models() -> Vec<OpenRouterModel> {
    FALLBACK_MODELS
        .iter()
        .map(|(id, name)| OpenRouterModel {
            id: id.to_string(),
            name: name.to_string(),
            context_length: None,
            prompt_price: None,
            completion_price: None,
        })
        .collect()
}

fn parse_response(bytes: &[u8]) -> Result<Vec<OpenRouterModel>, String> {
    let api_response: OpenRouterResponse =
        serde_json::from_slice(bytes).map_err(|e| e.to_string())?;

    Ok(api_response
        .data
        .into_iter()
        .map(|m| OpenRouterModel {
            id: m.id,
            name: m.name.unwrap_or_else(|| "Unknown".to_string()),
            context_length: m
                .top_provider
                .as_ref()
                .and_then(|tp| tp.context_length)
                .or(m.context_length),
            prompt_price: m.pricing.as_ref().and_then(|p| p.prompt.clone()),
            completion_price: m.pricing.as_ref().and_then(|p| p.completion.clone()),
        })
        .collect())
}

static LISTER: Lazy<ModelLister<OpenRouterModel>> = Lazy::new(|| {
    ModelLister::new(ListerConfig {
        provider: "OpenRouter",
        endpoint: "https://openrouter.ai/api/v1/models",
        ttl: Duration::from_secs(300),
        timeout: Duration::from_secs(5),
        requires_api_key: false,
        apply_auth: no_auth,
        parse: parse_response,
        fallback: fallback_models,
    })
});

/// Fetch OpenRouter models from API (public endpoint, no API key required).
///
/// # Returns
/// Vector of available models, or fallback models on error
pub async fn get_openrouter_models() -> Result<Vec<OpenRouterModel>, String> {
    LISTER.list(None).await
}

/// Clear the cached OpenRouter model list.
pub fn clear_cache() {
    LISTER.clear();
}
