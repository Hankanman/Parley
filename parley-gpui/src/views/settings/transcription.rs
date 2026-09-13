//! "Transcription" settings page: the Whisper model catalog (with
//! download), which model is active, and language preference.
//!
//! Reads/writes the same `transcript_settings` row the Tauri app's
//! `api_save_transcript_config`/`api_get_transcript_config` commands use
//! (via `SettingsRepository`, already Tauri-free in core), and drives model
//! downloads through `whisper_engine::download_model_with_progress`, the
//! same core function those commands call — progress arrives back as the
//! `model-download-progress`/`-complete`/`-error` core events, which
//! `SettingsView` subscribes to and mirrors into `SettingsCache`.

use gpui_kit::component::{
    button::Button,
    h_flex, v_flex,
    label::Label,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    ActiveTheme, Disableable, Icon,
};
use gpui_kit::*;

use parley_core::whisper_engine::ModelStatus;

use super::state::SettingsCache;
use super::SettingsView;

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let view = view.clone();
    let models = SettingsCache::global(cx).whisper_models.clone();

    let available: Vec<(SharedString, SharedString)> = models
        .iter()
        .filter(|m| matches!(m.status, ModelStatus::Available))
        .map(|m| (SharedString::from(m.name.clone()), SharedString::from(m.name.clone())))
        .collect();

    let mut catalog_items: Vec<SettingItem> = Vec::new();
    for model in models {
        catalog_items.push(model_row(&view, model));
    }

    SettingPage::new("Transcription")
        .icon(Icon::new(gpui_kit::assets::IconName::Mic))
        .group(
            SettingGroup::new().title("Active model").item(
                SettingItem::new(
                    "Whisper model",
                    SettingField::dropdown(
                        available,
                        |cx: &App| SharedString::from(SettingsCache::global(cx).transcript.model.clone()),
                        {
                            let view = view.clone();
                            move |val: SharedString, cx: &mut App| {
                                let model = val.to_string();
                                {
                                    let cache = cx.global_mut::<SettingsCache>();
                                    cache.transcript.provider = "localWhisper".to_string();
                                    cache.transcript.model = model.clone();
                                }
                                super::state::save_transcript(
                                    cx,
                                    "localWhisper".to_string(),
                                    model,
                                );
                                let _ = view.update(cx, |_, cx| cx.notify());
                            }
                        },
                    ),
                )
                .description("Used for local, on-device transcription. Only downloaded models can be selected."),
            ),
        )
        .group(SettingGroup::new().title("Model catalog").items(catalog_items))
}

fn model_row(view: &Entity<SettingsView>, model: parley_core::whisper_engine::ModelInfo) -> SettingItem {
    let view = view.clone();
    let name = model.name.clone();

    SettingItem::render(move |_options, _window, cx| {
        let status = SettingsCache::global(cx)
            .whisper_models
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.status.clone())
            .unwrap_or(model.status.clone());
        let progress = SettingsCache::global(cx).download_progress.get(&name).copied();

        let status_text = match (&status, progress) {
            (_, Some(pct)) => format!("Downloading… {}%", pct),
            (ModelStatus::Available, _) => "Available".to_string(),
            (ModelStatus::Missing, _) => "Missing".to_string(),
            (ModelStatus::Downloading { progress }, _) => format!("Downloading… {}%", progress),
            (ModelStatus::Corrupted { .. }, _) => "Corrupted — re-download".to_string(),
            (ModelStatus::Error(e), _) => format!("Error: {}", e),
        };

        let is_downloading = matches!(status, ModelStatus::Downloading { .. }) || progress.is_some();
        let is_corrupted = matches!(status, ModelStatus::Corrupted { .. });
        let is_available = matches!(status, ModelStatus::Available);
        let can_download = !is_available && !is_downloading;

        let download_name = name.clone();
        let download_view = view.clone();

        let mut actions = h_flex().gap_2();

        if is_downloading {
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
                    .label(if is_corrupted { "Re-download" } else { "Download" })
                    .disabled(!can_download)
                    .on_click(move |_, _, cx| {
                        start_download(cx, &download_view, download_name.clone());
                    }),
            );
        }

        if (is_available || is_corrupted) && !is_downloading {
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
                        Label::new(format!("{} · {} MB · {}", model.accuracy, model.size_mb, status_text))
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
        if let Err(e) =
            parley_core::whisper_engine::download_model_with_progress(sink, model_name.clone())
                .await
        {
            log::warn!("settings: failed to download model '{}': {}", model_name, e);
        }
    });
}

/// Cancel an in-progress Whisper model download.
fn cancel_download(cx: &mut App, view: &Entity<SettingsView>, model_name: String) {
    let services = crate::app_state::AppServices::global(cx);
    let io = services.io.clone();

    {
        let cache = cx.global_mut::<SettingsCache>();
        cache.download_progress.remove(&model_name);
    }
    let _ = view.update(cx, |_, cx| cx.notify());

    io.spawn(async move {
        if let Err(e) = parley_core::whisper_engine::whisper_cancel_download(model_name.clone()).await {
            log::warn!("settings: failed to cancel model download '{}': {}", model_name, e);
        }
    });
}

/// Delete a downloaded (or corrupted) Whisper model, then re-scan the
/// catalog so its status flips back to Missing.
fn delete_model(cx: &mut App, view: &Entity<SettingsView>, model_name: String) {
    let services = crate::app_state::AppServices::global(cx);
    let io = services.io.clone();
    let view = view.clone();

    cx.spawn(async move |cx| {
        let result = io
            .spawn(parley_core::whisper_engine::whisper_delete_corrupted_model(model_name.clone()))
            .await;

        if let Ok(Err(e)) = result {
            log::warn!("settings: failed to delete model '{}': {}", model_name, e);
        }
        let _ = cx.update(|cx| {
            super::state::load(view.clone(), cx);
        });
    })
    .detach();
}
