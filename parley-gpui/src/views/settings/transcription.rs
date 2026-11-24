//! "Transcription" settings page: the Whisper model catalog (with
//! download), which model is active, and language preference.
//!
//! Reads/writes the `transcript_settings` row via `SettingsRepository`, and
//! drives model downloads through
//! `whisper_engine::download_model_with_progress` — progress arrives back as the
//! `model-download-progress`/`-complete`/`-error` core events, which
//! `SettingsView` subscribes to and mirrors into `SettingsCache`.

use gpui_kit::component::{
    button::Button,
    h_flex, v_flex,
    label::Label,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    ActiveTheme, Disableable, Icon,
};
use gpui_kit::prelude::FluentBuilder as _;
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
            )
            .item(language_item(&view)),
        )
        .group(super::glossary::group(&view, cx))
        .group(
            SettingGroup::new()
                .title("Transcription models (Whisper)")
                .description(
                    "Speech-to-text models that turn meeting audio into the transcript. Larger models are \
                     more accurate but slower and use more memory. \"Compressed\" versions are much smaller \
                     downloads with nearly the same accuracy. The AI models that write summaries are under \
                     Settings → Summary.",
                )
                .items(catalog_items),
        )
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
        let in_use = SettingsCache::global(cx).transcript.model == name;
        let recommended = name == parley_core::config::DEFAULT_WHISPER_MODEL;
        let (display, compressed) = display_name(&name);

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
        } else if can_download || is_corrupted {
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
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Label::new(display).text_sm())
                            .when(compressed, |row| row.child(tag("Compressed", cx.theme().muted_foreground, cx)))
                            .when(recommended, |row| row.child(tag("Recommended", cx.theme().primary, cx)))
                            .when(in_use, |row| row.child(tag("In use", cx.theme().success, cx))),
                    )
                    .child(
                        Label::new(model.description.clone())
                            .text_xs()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Label::new(format!(
                            "{} accuracy · {} · {} MB · {} · {}",
                            model.accuracy, model.speed, model.size_mb, name, status_text
                        ))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(actions)
            .into_any_element()
    })
}

/// The live transcription language: every Whisper language, or auto-detect.
/// A blank value saved by the old free-text field shows as auto-detect.
fn language_item(view: &Entity<SettingsView>) -> SettingItem {
    let options: Vec<(SharedString, SharedString)> = parley_core::languages::WHISPER_LANGUAGES
        .iter()
        .map(|(code, name)| (SharedString::from(*code), SharedString::from(*name)))
        .collect();
    let view = view.clone();
    SettingItem::new(
        "Language",
        SettingField::scrollable_dropdown(
            options,
            |cx: &App| {
                SharedString::from(parley_core::languages::normalize(
                    &SettingsCache::global(cx).ui_str("primaryLanguage"),
                ))
            },
            move |val: SharedString, cx: &mut App| {
                parley_core::utils::apply_language_preference(Some(val.as_ref()));
                super::state::save_ui_field(
                    cx,
                    &view,
                    "primaryLanguage",
                    serde_json::Value::String(val.to_string()),
                );
            },
        ),
    )
    .description(
        "The language spoken in your meetings. Choosing it is faster and more accurate than \
         auto-detect, which works out the language from each stretch of speech.",
    )
}

/// A readable name for a catalog id, and whether it's a quantised
/// ("compressed") variant: `large-v3-turbo-q5_0` → ("Large v3 Turbo", true).
fn display_name(id: &str) -> (String, bool) {
    let (base, compressed) = match id.rsplit_once("-q") {
        Some((base, quant)) if quant.chars().next().is_some_and(|c| c.is_ascii_digit()) => (base, true),
        _ => (id, false),
    };
    let words: Vec<String> = base
        .split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) if !w.starts_with('v') || w.len() > 3 => first.to_uppercase().chain(c).collect(),
                _ => w.to_string(),
            }
        })
        .collect();
    (words.join(" "), compressed)
}

fn tag(text: &'static str, color: Hsla, cx: &App) -> impl IntoElement {
    div()
        .px_1p5()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(color.opacity(0.5))
        .text_xs()
        .text_color(color)
        .child(text)
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

#[cfg(test)]
mod display_name_tests {
    use super::display_name;

    #[test]
    fn catalog_ids_get_readable_names() {
        assert_eq!(display_name("large-v3-turbo-q5_0"), ("Large v3 Turbo".to_string(), true));
        assert_eq!(display_name("large-v3"), ("Large v3".to_string(), false));
        assert_eq!(display_name("tiny-q5_1"), ("Tiny".to_string(), true));
        assert_eq!(display_name("medium"), ("Medium".to_string(), false));
    }
}
