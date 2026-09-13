//! Settings page: Recording, Transcription, Summary and Appearance, built
//! on gpui-kit's `Settings`/`SettingPage`/`SettingGroup` components.
//!
//! Settings are cached in the `state::SettingsCache` global (loaded once
//! from SQLite/the whisper engine/PipeWire on `SettingsView::new`) since
//! `SettingField` getters/setters are synchronous — see `state.rs`'s doc
//! comment for why.

mod appearance;
mod beta;
mod calendar;
mod integrations;
mod model_utils;
mod notifications;
mod recording;
mod state;
mod summary;
mod transcription;

// Re-exported for `main.rs` to call once at startup after `state::load` —
// see `apply_saved_theme`'s doc comment in `appearance.rs`.
#[allow(unused_imports)]
pub use appearance::init_theme;

use gpui_kit::component::setting::Settings;
use gpui_kit::*;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;

pub struct SettingsView;

impl SettingsView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.entity();
        state::load(view.clone(), cx);

        // Model download progress: mirrored into `SettingsCache` so the
        // Transcription page's model rows can render a live percentage.
        let core_events = AppServices::global(cx).core_events.clone();
        cx.subscribe(&core_events, |_this, _emitter, event: &CoreEvent, cx| {
            match event.name.as_str() {
                "model-download-progress" => {
                    if let Some(payload) = event.decode::<DownloadProgress>() {
                        cx.update_global::<state::SettingsCache, _>(|cache, _| {
                            cache
                                .download_progress
                                .insert(payload.model_name, payload.progress);
                        });
                        cx.notify();
                    }
                }
                "model-download-complete" => {
                    if let Some(payload) = event.decode::<DownloadComplete>() {
                        cx.update_global::<state::SettingsCache, _>(|cache, _| {
                            cache.download_progress.remove(&payload.model_name);
                        });
                        // Re-scan the catalog so the newly-downloaded
                        // model's status flips to Available.
                        state::load(cx.entity(), cx);
                    }
                }
                "model-download-error" => {
                    if let Some(payload) = event.decode::<DownloadError>() {
                        cx.update_global::<state::SettingsCache, _>(|cache, _| {
                            cache.download_progress.remove(&payload.model_name);
                            cache.download_error = Some(payload.error);
                        });
                        cx.notify();
                    }
                }
                // Ollama model manager (Settings → Summary, Ollama section):
                // same three-event shape as the Whisper events above, just a
                // different event-name prefix.
                "ollama-model-download-progress" => {
                    if let Some(payload) = event.decode::<DownloadProgress>() {
                        cx.update_global::<state::SettingsCache, _>(|cache, _| {
                            cache
                                .ollama_pull_progress
                                .insert(payload.model_name, payload.progress);
                        });
                        cx.notify();
                    }
                }
                "ollama-model-download-complete" => {
                    if let Some(payload) = event.decode::<DownloadComplete>() {
                        cx.update_global::<state::SettingsCache, _>(|cache, _| {
                            cache.ollama_pull_progress.remove(&payload.model_name);
                        });
                        state::refresh_ollama_models(cx.entity(), cx);
                    }
                }
                "ollama-model-download-error" => {
                    if let Some(payload) = event.decode::<DownloadError>() {
                        cx.update_global::<state::SettingsCache, _>(|cache, _| {
                            cache.ollama_pull_progress.remove(&payload.model_name);
                            cache.ollama_error = Some(payload.error);
                        });
                        cx.notify();
                    }
                }
                // Built-in AI model manager (Settings → Summary,
                // "Built-in AI" section).
                "builtin-ai-download-progress" => {
                    if let Some(payload) = event.decode::<BuiltinDownloadProgress>() {
                        let done = matches!(payload.status.as_str(), "completed" | "cancelled" | "error");
                        cx.update_global::<state::SettingsCache, _>(|cache, _| {
                            if done {
                                cache.builtin_download_progress.remove(&payload.model);
                            } else {
                                cache
                                    .builtin_download_progress
                                    .insert(payload.model.clone(), payload.progress);
                            }
                        });
                        if done {
                            state::refresh_builtin_models(cx.entity(), cx);
                        } else {
                            cx.notify();
                        }
                    }
                }
                _ => {}
            }
        })
        .detach();

        Self
    }
}

#[derive(serde::Deserialize)]
struct DownloadProgress {
    #[serde(rename = "modelName")]
    model_name: String,
    progress: u8,
}

#[derive(serde::Deserialize)]
struct DownloadComplete {
    #[serde(rename = "modelName")]
    model_name: String,
}

#[derive(serde::Deserialize)]
struct DownloadError {
    #[serde(rename = "modelName")]
    model_name: String,
    error: String,
}

#[derive(serde::Deserialize)]
struct BuiltinDownloadProgress {
    model: String,
    progress: u8,
    status: String,
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let pages = vec![
            recording::page(&view, cx),
            transcription::page(&view, cx),
            summary::page(&view, cx),
            notifications::page(&view, cx),
            calendar::page(&view, cx),
            integrations::page(&view, cx),
            beta::page(&view, cx),
            appearance::page(&view, cx),
        ];

        div()
            .size_full()
            .child(Settings::new("app-settings").pages(pages))
    }
}
