//! Settings page: Recording, Transcription, Summary and Appearance, built
//! on gpui-kit's `Settings`/`SettingPage`/`SettingGroup` components.
//!
//! Settings are cached in the `state::SettingsCache` global (loaded once
//! from SQLite/the whisper engine/PipeWire on `SettingsView::new`) since
//! `SettingField` getters/setters are synchronous — see `state.rs`'s doc
//! comment for why.

mod about;
mod ask_search;
mod appearance;
mod calendar;
mod export_folder;
mod glossary;
mod integrations;
mod model_utils;
mod notifications;
mod recording;
mod state;
mod summary;
mod templates;
mod transcription;

// Re-exported for `main.rs` to call once at startup after `state::load` —
// see `apply_saved_theme`'s doc comment in `appearance.rs`.
#[allow(unused_imports)]
pub use appearance::init_theme;

use gpui_kit::component::setting::Settings;
use gpui_kit::*;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;

pub struct SettingsView {
    template_manager: Entity<templates::TemplateManager>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.entity();
        state::load(view.clone(), cx);
        glossary::load(view.clone(), cx);

        // Model download progress: mirrored into `SettingsCache` so the
        // Transcription page's model rows can render a live percentage.
        let core_events = AppServices::global(cx).core_events.clone();
        cx.subscribe(&core_events, |_this, _emitter, event: &CoreEvent, cx| {
            match event.name.as_str() {
                "summary-templates-changed" => cx.notify(),
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

        let template_manager = cx.new(|cx| templates::TemplateManager::new(window, cx));
        Self { template_manager }
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
            templates::page(&self.template_manager),
            notifications::page(&view, cx),
            calendar::page(&view, cx),
            integrations::page(&view, cx),
            appearance::page(&view, cx),
            about::page(cx),
        ];

        let settings = Settings::new("app-settings").pages(pages);
        // Like PARLEY_DEV_ROUTE, this permits isolated layout checks without
        // clicking through the real desktop or changing the user's settings.
        #[cfg(debug_assertions)]
        let settings = match std::env::var("PARLEY_DEV_SETTINGS_PAGE").ok().as_deref() {
            Some("templates") => settings.default_selected_index(gpui_kit::component::setting::SelectIndex { page_ix: 3, group_ix: None }),
            Some("summary") => settings.default_selected_index(gpui_kit::component::setting::SelectIndex { page_ix: 2, group_ix: None }),
            _ => settings,
        };
        div().size_full().child(settings)
    }
}
