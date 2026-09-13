// Built-in AI summary engine module
// Provides local LLM inference via llama-helper sidecar

pub mod client;
pub mod model_manager;
pub mod models;
pub mod service;
pub mod sidecar;

// Re-export commonly used types
pub use client::{
    force_shutdown_sidecar, generate_with_builtin, is_sidecar_healthy, shutdown_sidecar_gracefully,
};
// Tauri commands live in the shell crate's
// `commands::summary::summary_engine::commands` and are referenced that way
// in generate_handler!.
pub use service::{init_model_manager, ModelManagerState};
pub use model_manager::{ModelInfo, ModelStatus};
pub use models::{get_available_models, get_default_model, get_model_by_name, ModelDef};
