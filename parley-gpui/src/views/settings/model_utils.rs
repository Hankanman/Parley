//! Pure, side-effect-free helpers backing the Settings → Summary model
//! pickers: merging a live-fetched model list with a static fallback list
//! (mirrors `ModelSettingsModal`'s `modelOptions` memo — fallback only when
//! nothing was fetched), filtering a list by a search query, and formatting
//! a size in MB the same way across the Ollama/Whisper/built-in-AI model
//! managers. Kept free of `gpui`/`tokio` so it's plainly unit-testable.

/// Fallback model ids shown when OpenAI's `get_openai_models` fails or no
/// API key is configured yet — mirrors
/// `frontend/src/components/ModelSettings/constants.ts`'s
/// `OPENAI_FALLBACK_MODELS`.
pub const OPENAI_FALLBACK_MODELS: &[&str] = &[
    "gpt-4o",
    "gpt-4o-mini",
    "gpt-4-turbo",
    "gpt-4",
    "gpt-3.5-turbo",
    "o1",
    "o1-mini",
    "o3",
    "o3-mini",
];

/// Mirrors `CLAUDE_FALLBACK_MODELS`.
pub const CLAUDE_FALLBACK_MODELS: &[&str] = &[
    "claude-sonnet-4-5-20250929",
    "claude-haiku-4-5-20251001",
    "claude-opus-4-5-20251101",
    "claude-3-5-sonnet-latest",
];

/// Mirrors `GROQ_FALLBACK_MODELS`.
pub const GROQ_FALLBACK_MODELS: &[&str] = &[
    "llama-3.3-70b-versatile",
    "llama-3.1-70b-versatile",
    "mixtral-8x7b-32768",
    "gemma2-9b-it",
];

/// The static fallback list for `provider`, or `None` for providers that
/// have no fallback (Ollama/OpenRouter — an empty fetch there just means
/// "no models yet", not "use a canned list").
pub fn fallback_for_provider(provider: &str) -> Option<&'static [&'static str]> {
    match provider {
        "openai" => Some(OPENAI_FALLBACK_MODELS),
        "claude" => Some(CLAUDE_FALLBACK_MODELS),
        "groq" => Some(GROQ_FALLBACK_MODELS),
        _ => None,
    }
}

/// Merge a live-fetched model list with `provider`'s static fallback: the
/// fetch wins whenever it returned anything, otherwise fall back. Mirrors
/// `ModelSettingsModal`'s `modelOptions` memo.
pub fn merge_with_fallback(provider: &str, fetched: &[String]) -> Vec<String> {
    if !fetched.is_empty() {
        return fetched.to_vec();
    }
    fallback_for_provider(provider)
        .map(|fallback| fallback.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

/// Case-insensitive substring filter over `models`, matching
/// `OllamaModelsList`'s search box. An empty query returns everything.
pub fn filter_models<'a>(models: &'a [String], query: &str) -> Vec<&'a String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return models.iter().collect();
    }
    models
        .iter()
        .filter(|m| m.to_lowercase().contains(&query))
        .collect()
}

/// Format a size in megabytes as "NNN MB" below 1 GB, else "N.N GB" —
/// shared by the Ollama/built-in-AI model manager rows.
pub fn format_size_mb(size_mb: u64) -> String {
    if size_mb >= 1024 {
        format!("{:.1} GB", size_mb as f64 / 1024.0)
    } else {
        format!("{} MB", size_mb)
    }
}

#[cfg(test)]
mod tests {
    use super::{filter_models, format_size_mb, merge_with_fallback};

    #[test]
    fn merge_with_fallback_prefers_a_nonempty_fetch() {
        let fetched = vec!["gpt-5".to_string(), "gpt-5-mini".to_string()];
        assert_eq!(merge_with_fallback("openai", &fetched), fetched);
    }

    #[test]
    fn merge_with_fallback_falls_back_when_fetch_is_empty() {
        let result = merge_with_fallback("openai", &[]);
        assert_eq!(result, super::OPENAI_FALLBACK_MODELS.to_vec());
    }

    #[test]
    fn merge_with_fallback_is_empty_for_providers_without_a_fallback_list() {
        assert!(merge_with_fallback("ollama", &[]).is_empty());
        assert!(merge_with_fallback("openrouter", &[]).is_empty());
    }

    #[test]
    fn filter_models_is_case_insensitive_substring_match() {
        let models = vec!["Llama3:8b".to_string(), "gemma3:1b".to_string()];
        let matches = filter_models(&models, "LLAMA");
        assert_eq!(matches, vec![&models[0]]);
    }

    #[test]
    fn filter_models_returns_everything_for_a_blank_query() {
        let models = vec!["a".to_string(), "b".to_string()];
        assert_eq!(filter_models(&models, "   ").len(), 2);
    }

    #[test]
    fn format_size_mb_switches_units_at_one_gigabyte() {
        assert_eq!(format_size_mb(512), "512 MB");
        assert_eq!(format_size_mb(1024), "1.0 GB");
        assert_eq!(format_size_mb(2560), "2.5 GB");
    }
}
