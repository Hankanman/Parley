//! "Ask Parley search" group on the Summary settings page (#108): turn
//! semantic search on or off, and pick the Ollama embedding model when
//! Ollama is the summary provider. Backed by the `ask_embedding_model` app
//! setting (`parley_core::chat::across::embeddings`): an empty string turns
//! semantic search off; with Ollama, the value names the model; with any
//! other provider the built-in nomic-embed-text model is used regardless.
//!
//! Kept in its own small global, loaded the first time the page renders,
//! like `export_folder`.

use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem};
use gpui_kit::*;
use parley_core::chat::across::builtin_embedder;
use parley_core::chat::across::embeddings::{DEFAULT_OLLAMA_MODEL, KEY_ASK_EMBEDDING_MODEL};
use parley_core::database::repositories::setting::SettingsRepository;

use super::state::SettingsCache;
use super::SettingsView;
use crate::app_state::AppServices;
use crate::runtime::Io;

#[derive(Default)]
struct AskSearchCache {
    /// The stored setting; `None` until loaded or when never set (on, with
    /// the default model).
    model: Option<String>,
}

impl Global for AskSearchCache {}

impl AskSearchCache {
    fn enabled(&self) -> bool {
        self.model.as_deref().is_none_or(|m| !m.trim().is_empty())
    }

    fn ollama_model(&self) -> String {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(DEFAULT_OLLAMA_MODEL)
            .to_string()
    }
}

pub fn group(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingGroup {
    if !cx.has_global::<AskSearchCache>() {
        cx.set_global(AskSearchCache::default());
        load(view.clone(), cx);
    }
    let ollama = SettingsCache::global(cx).summary.provider.eq_ignore_ascii_case("ollama");
    let enabled = cx.global::<AskSearchCache>().enabled();

    let mut items = vec![SettingItem::new(
        "Semantic search",
        SettingField::switch(|cx: &App| cx.global::<AskSearchCache>().enabled(), {
            let view = view.clone();
            move |val: bool, cx: &mut App| {
                // On stores the Ollama model name (used only with Ollama);
                // off stores the empty string the core reads as "off".
                let value = if val { cx.global::<AskSearchCache>().ollama_model() } else { String::new() };
                save(cx, &view, value);
            }
        }),
    )
    .description(if ollama {
        "Find passages by meaning, not just keywords, using an embedding model on your Ollama server."
            .to_string()
    } else {
        format!(
            "Find passages by meaning, not just keywords, using a small built-in model ({}).",
            builtin_status()
        )
    })];

    if ollama && enabled {
        items.push(
            SettingItem::new(
                "Ollama embedding model",
                SettingField::input(
                    |cx: &App| SharedString::from(cx.global::<AskSearchCache>().ollama_model()),
                    {
                        let view = view.clone();
                        move |val: SharedString, cx: &mut App| {
                            let value = val.trim().to_string();
                            if !value.is_empty() {
                                save(cx, &view, value);
                            }
                        }
                    },
                ),
            )
            .description("Pull it first, e.g. `ollama pull nomic-embed-text`. Changing it re-indexes your meetings."),
        );
    }

    SettingGroup::new().title("Ask Parley search").items(items)
}

/// The built-in model's state, for the description.
fn builtin_status() -> &'static str {
    if builtin_embedder::model_is_ready() {
        "downloaded"
    } else if builtin_embedder::model_is_downloading() {
        "downloading"
    } else {
        "about 146 MB, downloaded in the background"
    }
}

fn load(view: Entity<SettingsView>, cx: &mut App) {
    let Some(pool) = AppServices::global(cx).pool() else {
        return;
    };
    let io = Io::global(cx);
    cx.spawn(async move |cx| {
        let loaded = io
            .spawn(async move {
                SettingsRepository::get_setting::<String>(&pool, KEY_ASK_EMBEDDING_MODEL)
                    .await
                    .ok()
                    .flatten()
            })
            .await;
        let _ = cx.update(|cx| {
            if let Ok(model) = loaded {
                cx.global_mut::<AskSearchCache>().model = model;
            }
            let _ = view.update(cx, |_, cx| cx.notify());
        });
    })
    .detach();
}

fn save(cx: &mut App, view: &Entity<SettingsView>, value: String) {
    cx.global_mut::<AskSearchCache>().model = Some(value.clone());
    let _ = view.update(cx, |_, cx| cx.notify());
    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, Ask search setting not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = SettingsRepository::set_setting(&pool, KEY_ASK_EMBEDDING_MODEL, &value).await {
            log::warn!("settings: failed to save Ask search setting: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::AskSearchCache;

    #[test]
    fn unset_means_on_with_the_default_model() {
        let cache = AskSearchCache::default();
        assert!(cache.enabled());
        assert_eq!(cache.ollama_model(), "nomic-embed-text");
        let off = AskSearchCache { model: Some(String::new()) };
        assert!(!off.enabled());
        let custom = AskSearchCache { model: Some("bge-m3".into()) };
        assert!(custom.enabled());
        assert_eq!(custom.ollama_model(), "bge-m3");
    }
}
