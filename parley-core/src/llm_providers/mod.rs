//! Generic model-listing client shared by the OpenAI-shaped LLM providers.
//!
//! `openai`, `anthropic`, `groq`, and `openrouter` all expose a "list available
//! chat models" Tauri command that does the same five things: check an
//! in-memory cache with a TTL, skip the network call entirely when no API key
//! is configured (for providers that require one), issue a timeout-bounded
//! GET request with a provider-specific auth header, parse the
//! provider-specific JSON shape into a common model struct, and fall back to
//! a hardcoded model list on any failure (missing key, network error,
//! non-2xx status, parse error, or an empty result).
//!
//! [`ModelLister`] implements that control flow once. Each provider supplies
//! only what actually differs between them via [`ListerConfig`]: the
//! endpoint URL, how to apply auth to the request, how to parse the response
//! body, and the fallback list.

use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Applies provider-specific auth to an outgoing request (e.g. a `Bearer`
/// header vs. an `x-api-key` header). Providers that don't require
/// authentication (e.g. OpenRouter's public models endpoint) use [`no_auth`].
pub type AuthApplier = fn(reqwest::RequestBuilder, &str) -> reqwest::RequestBuilder;

/// Parses a raw JSON response body into the provider's model list, applying
/// any provider-specific chat-model filtering along the way.
pub type ParseFn<T> = fn(&[u8]) -> Result<Vec<T>, String>;

/// Builds the hardcoded fallback list used when the API can't be reached.
pub type FallbackFn<T> = fn() -> Vec<T>;

/// No-op auth applier for providers whose model-listing endpoint doesn't
/// require an API key.
pub fn no_auth(rb: reqwest::RequestBuilder, _key: &str) -> reqwest::RequestBuilder {
    rb
}

struct CacheEntry<T> {
    models: Vec<T>,
    fetched_at: Instant,
}

/// Static, per-provider configuration for [`ModelLister`].
pub struct ListerConfig<T: 'static> {
    /// Human-readable provider name, used only for logging.
    pub provider: &'static str,
    pub endpoint: &'static str,
    pub ttl: Duration,
    pub timeout: Duration,
    /// Whether a missing/empty API key should short-circuit straight to the
    /// fallback list without making a request.
    pub requires_api_key: bool,
    pub apply_auth: AuthApplier,
    pub parse: ParseFn<T>,
    pub fallback: FallbackFn<T>,
}

/// Generic cached model-listing client. See the module docs for the shared
/// control flow; construct one `static` per provider via [`ListerConfig`].
pub struct ModelLister<T: 'static> {
    config: ListerConfig<T>,
    cache: RwLock<Option<CacheEntry<T>>>,
}

impl<T: Clone + Send + Sync + 'static> ModelLister<T> {
    pub fn new(config: ListerConfig<T>) -> Self {
        Self {
            config,
            cache: RwLock::new(None),
        }
    }

    /// Fetch the model list, honoring the cache and falling back to the
    /// configured fallback list on any error.
    pub async fn list(&self, api_key: Option<String>) -> Result<Vec<T>, String> {
        let key = match api_key {
            Some(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => {
                if self.config.requires_api_key {
                    log::info!(
                        "No {} API key provided, returning fallback models",
                        self.config.provider
                    );
                    return Ok((self.config.fallback)());
                }
                String::new()
            }
        };

        // Check cache first
        {
            let cache = self.cache.read().map_err(|e| e.to_string())?;
            if let Some(entry) = cache.as_ref() {
                if entry.fetched_at.elapsed() < self.config.ttl {
                    log::info!(
                        "Returning cached {} models ({} models)",
                        self.config.provider,
                        entry.models.len()
                    );
                    return Ok(entry.models.clone());
                }
            }
        }

        log::info!("Fetching {} models from API...", self.config.provider);
        let client = reqwest::Client::new();
        let request = (self.config.apply_auth)(client.get(self.config.endpoint), &key)
            .timeout(self.config.timeout);

        let response = match request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                log::warn!(
                    "Failed to fetch {} models: {}. Using fallback.",
                    self.config.provider,
                    e
                );
                return Ok((self.config.fallback)());
            }
        };

        if !response.status().is_success() {
            log::warn!(
                "{} API returned status {}. Using fallback models.",
                self.config.provider,
                response.status()
            );
            return Ok((self.config.fallback)());
        }

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                log::warn!(
                    "Failed to read {} response body: {}. Using fallback.",
                    self.config.provider,
                    e
                );
                return Ok((self.config.fallback)());
            }
        };

        let models = match (self.config.parse)(&bytes) {
            Ok(m) if !m.is_empty() => m,
            Ok(_) => {
                log::warn!(
                    "No chat models returned from {} API. Using fallback.",
                    self.config.provider
                );
                return Ok((self.config.fallback)());
            }
            Err(e) => {
                log::warn!(
                    "Failed to parse {} response: {}. Using fallback.",
                    self.config.provider,
                    e
                );
                return Ok((self.config.fallback)());
            }
        };

        log::info!(
            "Fetched {} {} models from API",
            models.len(),
            self.config.provider
        );

        {
            let mut cache = self.cache.write().map_err(|e| e.to_string())?;
            *cache = Some(CacheEntry {
                models: models.clone(),
                fetched_at: Instant::now(),
            });
        }

        Ok(models)
    }

    /// Clear the cached model list, forcing the next `list()` call to hit the API.
    pub fn clear(&self) {
        if let Ok(mut cache) = self.cache.write() {
            *cache = None;
        }
    }
}

/// Clear a single provider's cached model list by name. Not currently wired
/// to any key-change invalidation — available for future use.
pub fn clear(provider: &str) {
    match provider {
        "openai" => crate::openai::openai::clear_cache(),
        "anthropic" => crate::anthropic::anthropic::clear_cache(),
        "groq" => crate::groq::groq::clear_cache(),
        "openrouter" => crate::openrouter::openrouter::clear_cache(),
        other => log::warn!("llm_providers::clear: unknown provider '{}'", other),
    }
}
