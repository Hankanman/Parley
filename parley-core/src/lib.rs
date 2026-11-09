//! UI-independent core of the Parley desktop app: audio
//! capture/mixing/VAD, transcription, speaker diarization, summary
//! generation, and SQLite persistence. Depends on no UI framework;
//! the GPUI desktop shell (`parley-gpui`) sits on top of this.

// Performance optimization: Conditional logging macros for hot paths
#[cfg(debug_assertions)]
macro_rules! perf_debug {
    ($($arg:tt)*) => {
        log::debug!($($arg)*)
    };
}

#[cfg(not(debug_assertions))]
macro_rules! perf_debug {
    ($($arg:tt)*) => {};
}

// perf_debug! is auto-visible through `crate::` paths.

pub mod analytics;
pub mod anthropic;
pub mod audio;
pub mod bookmarks;
pub mod bootstrap;
pub mod calendar;
pub mod chapters;
pub mod chat;
pub mod config;
pub mod database;
pub mod events;
pub mod export;
pub mod followup;
pub mod gigaam;
pub mod glossary;
pub mod groq;
pub mod llm_providers;
pub mod mcp_config;
pub mod ollama;
pub mod onboarding;
pub mod openai;
pub mod openrouter;
pub mod parakeet;
pub mod paths;
pub mod speaker_diarization;
pub mod state;
pub mod summary;
pub mod utils;
pub mod whisper_engine;
