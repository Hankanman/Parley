//! "Transcription" settings page: the local model catalog (with
//! download), which model is active, and language preference.
//!
//! Reads/writes the `transcript_settings` row via `SettingsRepository`, and
//! drives model downloads through
//! `whisper_engine::download_model_with_progress` — progress arrives back as the
//! `model-download-progress`/`-complete`/`-error` core events, which
//! `SettingsView` subscribes to and mirrors into `SettingsCache`.

use gpui_kit::component::{
    ActiveTheme, Disableable, Icon,
    button::Button,
    h_flex,
    label::Label,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    v_flex,
};
use gpui_kit::*;

use parley_core::whisper_engine::ModelStatus;

use super::SettingsView;
use super::state::SettingsCache;

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let view = view.clone();
    let provider = SettingsCache::global(cx).transcript.provider.clone();
    let gigaam = provider == parley_core::gigaam::PROVIDER;
    let parakeet = provider == parley_core::parakeet::PROVIDER;
    let models: Vec<_> = SettingsCache::global(cx)
        .whisper_models
        .iter()
        .filter(|m| {
            parley_core::audio::transcription::engine::provider_for_model(&m.name) == provider
        })
        .cloned()
        .collect();

    let available: Vec<(SharedString, SharedString)> = models
        .iter()
        .filter(|m| matches!(m.status, ModelStatus::Available))
        .map(|m| {
            (
                SharedString::from(m.name.clone()),
                SharedString::from(m.name.clone()),
            )
        })
        .collect();

    let mut catalog_items: Vec<SettingItem> = Vec::new();
    for model in models {
        catalog_items.push(model_row(&view, model));
    }

    let page = SettingPage::new("Transcription")
        .icon(Icon::new(gpui_kit::assets::IconName::Mic))
        .group(SettingGroup::new().title("Engine").item(SettingItem::new(
            "Speech recognition",
            SettingField::dropdown(
                vec![(SharedString::from("localWhisper"), SharedString::from("Whisper")),
                     (SharedString::from("localGigaAM"), SharedString::from("GigaAM v3 · Russian")),
                     (SharedString::from("localParakeet"), SharedString::from("Parakeet TDT v3 · Multilingual"))],
                |cx: &App| SharedString::from(SettingsCache::global(cx).transcript.provider.clone()),
                {
                    let view = view.clone();
                    move |value: SharedString, cx: &mut App| {
                        if parley_core::audio::transcription::engine::selection_locked() { return; }
                        let provider = value.to_string();
                        let is_gigaam = provider == parley_core::gigaam::PROVIDER;
                        let model = SettingsCache::global(cx).whisper_models.iter()
                            .find(|m| parley_core::audio::transcription::engine::provider_for_model(&m.name) == provider && matches!(m.status, ModelStatus::Available))
                            .map(|m| m.name.clone()).unwrap_or_else(|| if is_gigaam { parley_core::gigaam::DEFAULT_MODEL.into() } else if provider == parley_core::parakeet::PROVIDER { parley_core::parakeet::DEFAULT_MODEL.into() } else { parley_core::config::DEFAULT_WHISPER_MODEL.into() });
                        {
                            let cache = cx.global_mut::<SettingsCache>();
                            cache.transcript.provider = provider.clone();
                            cache.transcript.model = model.clone();
                        }
                        super::state::save_transcript(cx, provider, model);
                        let _ = view.update(cx, |_, cx| cx.notify());
                    }
                }
            )
        ).description(if gigaam { parley_core::gigaam::runtime_status() } else if parakeet { parley_core::parakeet::runtime_status() } else { "Local Whisper recognition".into() })))
        .group(
            SettingGroup::new().title("Active model").item(
                SettingItem::new(
                    if gigaam { "GigaAM model" } else if parakeet { "Parakeet model" } else { "Whisper model" },
                    SettingField::dropdown(
                        available,
                        |cx: &App| SharedString::from(SettingsCache::global(cx).transcript.model.clone()),
                        {
                            let view = view.clone();
                            move |val: SharedString, cx: &mut App| {
                                if parley_core::audio::transcription::engine::selection_locked() { return; }
                                let model = val.to_string();
                                let provider = parley_core::audio::transcription::engine::provider_for_model(&model);
                                {
                                    let cache = cx.global_mut::<SettingsCache>();
                                    cache.transcript.provider = provider.to_string();
                                    cache.transcript.model = model.clone();
                                }
                                super::state::save_transcript(
                                    cx,
                                    provider.to_string(),
                                    model,
                                );
                                let _ = view.update(cx, |_, cx| cx.notify());
                            }
                        },
                    ),
                )
                .description("Used for local, on-device transcription. Only installed models can be selected. Parakeet supports 25 languages including Russian. Choose FP32 for CUDA or INT8 for CPU."),
            ),
        )
        ;
    let page = if gigaam || parakeet {
        page
    } else {
        page.group(super::glossary::group(&view, cx))
    };
    let page = if let Some(error) = SettingsCache::global(cx).download_error.clone() {
        page.group(
            SettingGroup::new()
                .title("Model operation")
                .item(SettingItem::render(move |_, _, _| {
                    Label::new(error.clone()).text_sm().into_any_element()
                })),
        )
    } else {
        page
    };
    page.group(
        SettingGroup::new()
            .title("Model catalog")
            .items(catalog_items),
    )
}

fn model_row(
    view: &Entity<SettingsView>,
    model: parley_core::whisper_engine::ModelInfo,
) -> SettingItem {
    let view = view.clone();
    let name = model.name.clone();

    SettingItem::render(move |_options, _window, cx| {
        let status = SettingsCache::global(cx)
            .whisper_models
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.status.clone())
            .unwrap_or(model.status.clone());
        let progress = SettingsCache::global(cx)
            .download_progress
            .get(&name)
            .copied();

        let status_text = match (&status, progress) {
            (_, Some(pct)) => format!("Downloading… {}%", pct),
            (ModelStatus::Available, _) => "Available".to_string(),
            (ModelStatus::Missing, _) => "Missing".to_string(),
            (ModelStatus::Downloading { progress }, _) => format!("Downloading… {}%", progress),
            (ModelStatus::Corrupted { .. }, _) => "Corrupted — re-download".to_string(),
            (ModelStatus::Error(e), _) => format!("Error: {}", e),
        };

        let is_downloading =
            matches!(status, ModelStatus::Downloading { .. }) || progress.is_some();
        let is_corrupted = matches!(status, ModelStatus::Corrupted { .. });
        let is_available = matches!(status, ModelStatus::Available);
        let can_download = !is_available && !is_downloading;

        let download_name = name.clone();
        let download_view = view.clone();

        let mut actions = h_flex().gap_2();

        if parley_core::gigaam::is_model(&name) {
            actions = actions.child(
                Label::new(if is_available {
                    "Installed · Russian"
                } else {
                    "Not installed"
                })
                .text_xs(),
            );
        } else if is_downloading {
            let cancel_name = name.clone();
            let cancel_view = view.clone();
            actions = actions.child(
                Button::new(SharedString::from(format!("cancel-{}", name)))
                    .outline()
                    .label("Cancel")
                    .on_click(move |_, _, cx| {
                        cancel_download(cx, &cancel_view, cancel_name.clone());
                    }),
            );
        } else {
            actions = actions.child(
                Button::new(SharedString::from(format!("download-{}", name)))
                    .outline()
                    .label(if is_corrupted {
                        "Re-download"
                    } else {
                        "Download"
                    })
                    .disabled(!can_download)
                    .on_click(move |_, _, cx| {
                        start_download(cx, &download_view, download_name.clone());
                    }),
            );
        }

        if !parley_core::gigaam::is_model(&name)
            && (is_available || is_corrupted)
            && !is_downloading
        {
            let delete_name = name.clone();
            let delete_view = view.clone();
            actions = actions.child(
                Button::new(SharedString::from(format!("delete-{}", name)))
                    .outline()
                    .label("Delete")
                    .on_click(move |_, _, cx| {
                        delete_model(cx, &delete_view, delete_name.clone());
                    }),
            );
        }

        h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new(name.clone()).text_sm())
                    .child(
                        Label::new(format!(
                            "{} · {} MB · {}",
                            model.accuracy, model.size_mb, status_text
                        ))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(actions)
            .into_any_element()
    })
}

fn start_download(cx: &mut App, view: &Entity<SettingsView>, model_name: String) {
    let services = crate::app_state::AppServices::global(cx);
    let sink = services.sink.clone();
    let io = services.io.clone();

    {
        let cache = cx.global_mut::<SettingsCache>();
        cache.download_progress.insert(model_name.clone(), 0);
        cache.download_error = None;
    }
    let _ = view.update(cx, |_, cx| cx.notify());

    io.spawn(async move {
        if let Err(e) = if parley_core::parakeet::is_model(&model_name) {
            parley_core::parakeet::download(sink, model_name.clone()).await
        } else {
            parley_core::whisper_engine::download_model_with_progress(sink, model_name.clone())
                .await
        } {
            log::warn!("settings: failed to download model '{}': {}", model_name, e);
        }
    });
}

/// Cancel an in-progress local model download.
fn cancel_download(cx: &mut App, view: &Entity<SettingsView>, model_name: String) {
    let services = crate::app_state::AppServices::global(cx);
    let io = services.io.clone();

    {
        let cache = cx.global_mut::<SettingsCache>();
        cache.download_progress.remove(&model_name);
    }
    let _ = view.update(cx, |_, cx| cx.notify());

    io.spawn(async move {
        if let Err(e) = if parley_core::parakeet::is_model(&model_name) {
            parley_core::parakeet::cancel(&model_name).await
        } else {
            parley_core::whisper_engine::whisper_cancel_download(model_name.clone()).await
        } {
            log::warn!(
                "settings: failed to cancel model download '{}': {}",
                model_name,
                e
            );
        }
    });
}

/// Delete a downloaded (or corrupted) local model, then re-scan the
/// catalog so its status flips back to Missing.
fn delete_model(cx: &mut App, view: &Entity<SettingsView>, model_name: String) {
    let services = crate::app_state::AppServices::global(cx);
    let io = services.io.clone();
    let view = view.clone();

    cx.spawn(async move |cx| {
        let delete_name = model_name.clone();
        let result = io
            .spawn(async move {
                if parley_core::parakeet::is_model(&delete_name) {
                    parley_core::parakeet::delete(&delete_name).await
                } else {
                    parley_core::whisper_engine::whisper_delete_corrupted_model(delete_name)
                        .await
                        .map(|_| ())
                }
            })
            .await;

        let error = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(e) => Some(e.to_string()),
        };
        let _ = cx.update(|cx| {
            cx.global_mut::<SettingsCache>().download_error = error;
            super::state::load(view.clone(), cx);
        });
    })
    .detach();
}
