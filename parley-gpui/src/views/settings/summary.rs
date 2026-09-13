//! "Summary" settings page: provider + model, API keys, the custom
//! OpenAI-compatible endpoint, default summary template, and the
//! language/auto-summary preferences the React app keeps in `ConfigContext`.
//!
//! Reads/writes the same `settings` row the Tauri app's
//! `api_save_model_config`/`api_get_model_config` commands use (via
//! `SettingsRepository`, already Tauri-free in core). Built-in AI models
//! come from `summary::summary_engine::models::get_available_models`, the
//! same catalog the sidecar downloads from.
//!
//! - API keys: same `settings`/`transcript_settings` columns as
//!   `SettingsRepository::save_api_key`/`get_api_key`/`delete_api_key`
//!   (the core logic behind `api_save_model_config`'s key-saving branch and
//!   `api_delete_api_key`), keyed by the same provider ids React's
//!   `ModelConfig.provider` uses (`openai`/`claude`/`groq`/`openrouter`) —
//!   note this page previously used `"anthropic"` for Claude's provider id,
//!   which doesn't match any `save_api_key` column and silently failed key
//!   lookups; fixed to `"claude"` here since API-key wiring depends on it.
//! - Custom OpenAI-compatible endpoint: same `settings.customOpenAIConfig`
//!   JSON blob as `ModelSettingsModal`'s `CustomOpenAISection`
//!   (`SettingsRepository::save_custom_openai_config`/`get_custom_openai_config`).
//! - Default summary template: `KEY_DEFAULT_SUMMARY_TEMPLATE` — NEW, since
//!   React's `useTemplates` keeps template selection as per-meeting-session
//!   local state only (see that key's doc comment in `parley-core`).
//! - Language / confidence indicator / auto-summary: the same `ui_config`
//!   JSON blob (`KEY_UI_CONFIG`) React's `ConfigContext.persistUiConfig`
//!   writes (`primaryLanguage`/`showConfidenceIndicator`/`isAutoSummary`).

use gpui_kit::component::{
    button::Button,
    combobox::{Combobox, ComboboxEvent, ComboboxState},
    h_flex,
    input::{Input, InputContentType, InputEvent, InputState},
    label::Label,
    searchable_list::SearchableVec,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    v_flex,
    ActiveTheme, Disableable, Icon, IndexPath, Sizable as _,
};
use gpui_kit::*;

use parley_core::summary::summary_engine::{model_manager::ModelStatus as BuiltinModelStatus, models::get_available_models};
use parley_core::summary::CustomOpenAIConfig;

use super::model_utils::{format_size_mb, merge_with_fallback};
use super::state::SettingsCache;
use super::SettingsView;

const PROVIDERS: &[(&str, &str)] = &[
    ("builtin-ai", "Built-in AI (local)"),
    ("ollama", "Ollama"),
    ("openai", "OpenAI"),
    ("claude", "Claude"),
    ("groq", "Groq"),
    ("openrouter", "OpenRouter"),
    ("custom-openai", "Custom OpenAI-compatible"),
];

/// Providers that take a plain API key via `SettingsRepository::save_api_key`.
/// `custom-openai` is handled separately (its key lives inside the JSON
/// `customOpenAIConfig` blob); `builtin-ai`/`ollama` need no key.
const API_KEY_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI API key"),
    ("claude", "Claude API key"),
    ("groq", "Groq API key"),
    ("openrouter", "OpenRouter API key"),
];

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let view = view.clone();

    let provider_options: Vec<(SharedString, SharedString)> = PROVIDERS
        .iter()
        .map(|(id, label)| (SharedString::from(*id), SharedString::from(*label)))
        .collect();

    let builtin_models = get_available_models();
    let model_options: Vec<(SharedString, SharedString)> = builtin_models
        .iter()
        .map(|m| (SharedString::from(m.name.clone()), SharedString::from(m.display_name.clone())))
        .collect();

    let provider = SettingsCache::global(cx).summary.provider.clone();
    let is_builtin = provider.is_empty() || provider == "builtin-ai";
    let is_custom_openai = provider == "custom-openai";

    let mut page = SettingPage::new("Summary")
        .icon(Icon::new(gpui_kit::assets::IconName::MessageSquare))
        .group(
            SettingGroup::new().title("Provider").items(vec![
                SettingItem::new(
                    "Provider",
                    SettingField::dropdown(
                        provider_options,
                        |cx: &App| {
                            let provider = SettingsCache::global(cx).summary.provider.clone();
                            SharedString::from(if provider.is_empty() {
                                "builtin-ai".to_string()
                            } else {
                                provider
                            })
                        },
                        {
                            let view = view.clone();
                            move |val: SharedString, cx: &mut App| {
                                set_summary(cx, &view, |cfg| cfg.provider = val.to_string());
                            }
                        },
                    ),
                )
                .description("Which service generates meeting summaries."),
            ]),
        );

    let is_ollama = provider == "ollama";

    if is_custom_openai {
        page = page.group(custom_openai_group(&view));
    } else if is_builtin {
        // No plain Model field for built-in AI — selection happens by
        // clicking a row in the manager list below (mirrors
        // `ModelSettingsModal` swapping the Model field for
        // `<BuiltInModelManager>` on this provider).
        let _ = model_options;
    } else if is_ollama {
        page = page.group(
            SettingGroup::new().title("Model").item(
                SettingItem::new(
                    "Model",
                    SettingField::dropdown(
                        SettingsCache::global(cx)
                            .ollama_installed
                            .iter()
                            .map(|m| (SharedString::from(m.name.clone()), SharedString::from(m.name.clone())))
                            .collect(),
                        |cx: &App| SharedString::from(SettingsCache::global(cx).summary.model.clone()),
                        {
                            let view = view.clone();
                            move |val: SharedString, cx: &mut App| {
                                set_summary(cx, &view, |cfg| cfg.model = val.to_string());
                            }
                        },
                    ),
                )
                .description("Installed on the Ollama endpoint below. Manage models in the section further down."),
            ),
        );
    } else {
        // Cloud provider (openai/claude/groq/openrouter): a searchable
        // picker populated live from that provider's model-list API,
        // falling back to a static list when unfetched or the fetch fails
        // — mirrors `ModelSettingsModal`'s `ProviderModelPicker`.
        page = page.group(
            SettingGroup::new()
                .title("Model")
                .item(cloud_model_picker_item(&view, provider.clone())),
        );
    }

    page = page.group(
        SettingGroup::new().title("Ollama").item(
            SettingItem::new(
                "Endpoint",
                SettingField::input(
                    |cx: &App| SharedString::from(SettingsCache::global(cx).summary.ollama_endpoint.clone()),
                    {
                        let view = view.clone();
                        move |val: SharedString, cx: &mut App| {
                            set_summary(cx, &view, |cfg| cfg.ollama_endpoint = val.to_string());
                        }
                    },
                )
                .default_value(SharedString::from("http://localhost:11434")),
            )
            .description("Used only when the provider above is set to Ollama."),
        ),
    );

    if is_ollama {
        page = page.group(ollama_manager_group(&view));
    }

    if is_builtin {
        page = page.group(builtin_manager_group(&view));
    }

    let mut key_items = Vec::new();
    for (id, label) in API_KEY_PROVIDERS {
        key_items.push(api_key_item(&view, id, label));
    }
    page = page.group(
        SettingGroup::new()
            .title("API keys")
            .description("Stored locally in SQLite (plaintext), same as the Tauri app. Leave blank to clear.")
            .items(key_items),
    );

    page = page.group(
        SettingGroup::new().title("Templates").item(
            SettingItem::new(
                "Default summary template",
                SettingField::dropdown(
                    SettingsCache::global(cx)
                        .templates
                        .iter()
                        .map(|(id, name, _)| (SharedString::from(id.clone()), SharedString::from(name.clone())))
                        .collect(),
                    |cx: &App| SharedString::from(SettingsCache::global(cx).default_template_id.clone()),
                    {
                        let view = view.clone();
                        move |val: SharedString, cx: &mut App| {
                            super::state::save_default_template(cx, &view, val.to_string());
                        }
                    },
                ),
            )
            .description("Pre-selected when generating a new meeting summary."),
        ),
    );

    page = page.group(
        SettingGroup::new().title("Language & summaries").items(vec![
            SettingItem::new(
                "Transcription language",
                SettingField::input(
                    |cx: &App| SharedString::from(SettingsCache::global(cx).ui_str("primaryLanguage")),
                    {
                        let view = view.clone();
                        move |val: SharedString, cx: &mut App| {
                            super::state::save_ui_field(
                                cx,
                                &view,
                                "primaryLanguage",
                                serde_json::Value::String(val.to_string()),
                            );
                        }
                    },
                )
                .default_value(SharedString::from("en")),
            )
            .description("ISO language code, or blank for auto-detect. Same `ui_config` row the React app uses."),
            SettingItem::new(
                "Show transcription confidence",
                SettingField::switch(
                    |cx: &App| SettingsCache::global(cx).ui_bool("showConfidenceIndicator", false),
                    {
                        let view = view.clone();
                        move |val: bool, cx: &mut App| {
                            super::state::save_ui_field(
                                cx,
                                &view,
                                "showConfidenceIndicator",
                                serde_json::Value::Bool(val),
                            );
                        }
                    },
                ),
            ),
            SettingItem::new(
                "Auto-summarize after recording",
                SettingField::switch(
                    |cx: &App| SettingsCache::global(cx).ui_bool("isAutoSummary", false),
                    {
                        let view = view.clone();
                        move |val: bool, cx: &mut App| {
                            super::state::save_ui_field(cx, &view, "isAutoSummary", serde_json::Value::Bool(val));
                        }
                    },
                ),
            ),
        ]),
    );

    page
}

fn set_summary(
    cx: &mut App,
    view: &Entity<SettingsView>,
    mutate: impl FnOnce(&mut super::state::SummaryConfig),
) {
    let updated = {
        let cache = cx.global_mut::<SettingsCache>();
        mutate(&mut cache.summary);
        cache.summary.clone()
    };
    let whisper_model = SettingsCache::global(cx).transcript.model.clone();
    super::state::save_summary(
        cx,
        updated.provider,
        updated.model,
        whisper_model,
        if updated.ollama_endpoint.is_empty() {
            None
        } else {
            Some(updated.ollama_endpoint)
        },
    );
    let _ = view.update(cx, |_, cx| cx.notify());
}

/// A masked API-key entry for `provider`. Saves on every change (same
/// auto-save UX as every other field on this page); an empty value clears
/// the stored key (`SettingsRepository::delete_api_key`, via
/// `state::save_api_key`).
fn api_key_item(view: &Entity<SettingsView>, provider: &'static str, label: &'static str) -> SettingItem {
    let view = view.clone();
    SettingItem::new(
        label,
        SettingField::render(move |_options, window, cx| {
            let has_key = SettingsCache::global(cx)
                .api_key_present
                .get(provider)
                .copied()
                .unwrap_or(false);

            struct KeyInputState {
                input: Entity<InputState>,
                _subscription: Subscription,
            }

            let key_id = SharedString::from(format!("summary-api-key-{}", provider));
            let state = window.use_keyed_state(key_id, cx, {
                let view = view.clone();
                move |window, cx| {
                    let input = cx.new(|cx| {
                        InputState::new(window, cx).placeholder(if has_key {
                            "•••••••••••••••• (saved — type to replace)"
                        } else {
                            "Not set"
                        })
                    });
                    let subscription = cx.subscribe(&input, {
                        let view = view.clone();
                        move |_, input, event: &InputEvent, cx| {
                            if matches!(event, InputEvent::Change) {
                                let value = input.read(cx).value().to_string();
                                super::state::save_api_key(cx, &view, provider.to_string(), value);
                            }
                        }
                    });
                    KeyInputState { input, _subscription: subscription }
                }
            });
            let input_entity = state.read(cx).input.clone();

            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(
                    Input::new(&input_entity)
                        .content_type(InputContentType::Password)
                        .small()
                        .w_64(),
                )
                .child(if has_key {
                    Label::new("Configured").text_color(cx.theme().success)
                } else {
                    Label::new("Not set").text_color(cx.theme().muted_foreground)
                })
                .into_any_element()
        }),
    )
}

/// Custom OpenAI-compatible endpoint config: mirrors `ModelSettingsModal`'s
/// `CustomOpenAISection` (endpoint/model/max tokens/temperature/top-p plus
/// its own API key), saved as one JSON blob via
/// `SettingsRepository::save_custom_openai_config`.
fn custom_openai_group(view: &Entity<SettingsView>) -> SettingGroup {
    let view = view.clone();

    fn current(cx: &App) -> CustomOpenAIConfig {
        SettingsCache::global(cx).custom_openai.clone().unwrap_or(CustomOpenAIConfig {
            endpoint: String::new(),
            api_key: None,
            model: String::new(),
            max_tokens: None,
            temperature: None,
            top_p: None,
        })
    }

    fn set(cx: &mut App, view: &Entity<SettingsView>, mutate: impl FnOnce(&mut CustomOpenAIConfig)) {
        let mut config = current(cx);
        mutate(&mut config);
        super::state::save_custom_openai(cx, view, config);
    }

    SettingGroup::new()
        .title("Custom OpenAI-compatible endpoint")
        .description("For self-hosted or third-party servers implementing the OpenAI chat-completions API.")
        .items(vec![
            SettingItem::new(
                "Endpoint URL",
                SettingField::input(
                    |cx: &App| SharedString::from(current(cx).endpoint),
                    {
                        let view = view.clone();
                        move |val: SharedString, cx: &mut App| {
                            set(cx, &view, |c| c.endpoint = val.to_string());
                        }
                    },
                )
                .default_value(SharedString::from("http://localhost:8000/v1")),
            ),
            SettingItem::new(
                "Model",
                SettingField::input(
                    |cx: &App| SharedString::from(current(cx).model),
                    {
                        let view = view.clone();
                        move |val: SharedString, cx: &mut App| {
                            set(cx, &view, |c| c.model = val.to_string());
                        }
                    },
                ),
            ),
            SettingItem::new(
                "API key",
                SettingField::input(
                    |cx: &App| SharedString::from(current(cx).api_key.unwrap_or_default()),
                    {
                        let view = view.clone();
                        move |val: SharedString, cx: &mut App| {
                            let text = val.to_string();
                            set(cx, &view, |c| {
                                c.api_key = if text.is_empty() { None } else { Some(text) };
                            });
                        }
                    },
                )
                .default_value(SharedString::default()),
            )
            .description("Optional — leave blank if the server doesn't require one."),
        ])
}

/// Searchable model picker for a cloud provider (openai/claude/groq/
/// openrouter), backed by that provider's live model list
/// (`SettingsCache::provider_models`) with a static fallback — mirrors
/// `ProviderModelPicker`. Triggers the fetch itself the first time it's
/// rendered for `provider` (React's "auto-fetch on initial load only").
fn cloud_model_picker_item(view: &Entity<SettingsView>, provider: String) -> SettingItem {
    let view = view.clone();
    SettingItem::new(
        "Model",
        SettingField::render(move |_options, window, cx| {
            let cache = SettingsCache::global(cx);
            let fetched = cache.provider_models.get(&provider).cloned().unwrap_or_default();
            let loading = cache.provider_models_loading.get(&provider).copied().unwrap_or(false);
            let error = cache.provider_models_error.get(&provider).cloned();
            let current_model = cache.summary.model.clone();

            // Trigger the live fetch exactly once per provider (not on
            // every render) — a `use_keyed_state` marker entity whose init
            // closure only runs the first time this key is seen.
            struct FetchTrigger;
            let _ = window.use_keyed_state(
                SharedString::from(format!("summary-cloud-model-fetch-{}", provider)),
                cx,
                {
                    let view = view.clone();
                    let provider = provider.clone();
                    move |_window, cx: &mut Context<FetchTrigger>| {
                        super::state::fetch_provider_models(cx, view.clone(), provider.clone());
                        FetchTrigger
                    }
                },
            );

            let options = merge_with_fallback(&provider, &fetched);

            struct ModelComboState {
                combo: Entity<ComboboxState<SearchableVec<String>>>,
                _subscription: Subscription,
            }

            // Re-key on the *fetched* count (not the merged list, which
            // already shows the static fallback at a stable length) so a
            // fresh combobox — with the live results as its options — is
            // built exactly once the fetch resolves.
            let combo_key = SharedString::from(format!(
                "summary-cloud-model-combobox-{}-{}",
                provider,
                fetched.len()
            ));
            let state = window.use_keyed_state(combo_key, cx, {
                let view = view.clone();
                let options = options.clone();
                let current_model = current_model.clone();
                move |window, cx| {
                    let selected = options
                        .iter()
                        .position(|m| *m == current_model)
                        .map(|ix| vec![IndexPath::default().row(ix)])
                        .unwrap_or_default();
                    let delegate = SearchableVec::new(options.clone());
                    let combo =
                        cx.new(|cx| ComboboxState::new(delegate, selected, window, cx).searchable(true));
                    let subscription = cx.subscribe(&combo, {
                        let view = view.clone();
                        move |_, _, event: &ComboboxEvent<SearchableVec<String>>, cx| {
                            if let ComboboxEvent::Confirm(values) = event {
                                if let Some(model) = values.first() {
                                    let model = model.clone();
                                    set_summary(cx, &view, |cfg| cfg.model = model);
                                }
                            }
                        }
                    });
                    ModelComboState { combo, _subscription: subscription }
                }
            });
            let combo_entity = state.read(cx).combo.clone();

            v_flex()
                .w_full()
                .gap_1()
                .child(
                    Combobox::new(&combo_entity)
                        .placeholder("Select a model")
                        .search_placeholder("Search models…")
                        .cleanable(false),
                )
                .children(loading.then(|| {
                    Label::new("Loading models…").text_xs().text_color(cx.theme().muted_foreground)
                }))
                .children(error.map(|e| {
                    Label::new(format!("Couldn't fetch live models — showing a fallback list ({e}).",))
                        .text_xs()
                        .text_color(cx.theme().danger)
                }))
                .into_any_element()
        }),
    )
}

/// Ollama model manager (mirrors `OllamaModelsList` + the pull/delete
/// actions from `ModelSettingsModal`): pull a model by name (progress via
/// `ollama-model-download-*` events), and list/delete what's installed on
/// the configured endpoint.
fn ollama_manager_group(view: &Entity<SettingsView>) -> SettingGroup {
    let view = view.clone();

    SettingGroup::new()
        .title("Ollama models")
        .description("Models installed on the endpoint above.")
        .item(SettingItem::render({
            let view = view.clone();
            move |_options, window, cx| {
                struct FetchTrigger;
                let _ = window.use_keyed_state(
                    SharedString::from("summary-ollama-models-fetch"),
                    cx,
                    {
                        let view = view.clone();
                        move |_window, cx: &mut Context<FetchTrigger>| {
                            super::state::refresh_ollama_models(view.clone(), cx);
                            FetchTrigger
                        }
                    },
                );

                let cache = SettingsCache::global(cx);
                let loading = cache.ollama_installed_loading;
                let error = cache.ollama_error.clone();
                let installed = cache.ollama_installed.clone();
                let pull_progress = cache.ollama_pull_progress.clone();
                let selected_model = cache.summary.model.clone();

                struct PullInputState {
                    input: Entity<InputState>,
                    _subscription: Subscription,
                }
                let pull_state = window.use_keyed_state(SharedString::from("summary-ollama-pull-input"), cx, {
                    move |window, cx| {
                        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Model name, e.g. gemma3:1b"));
                        let subscription = cx.subscribe(&input, |_, _, _: &InputEvent, _| {});
                        PullInputState { input, _subscription: subscription }
                    }
                });
                let pull_input = pull_state.read(cx).input.clone();
                let pull_value = pull_input.read(cx).value().to_string();

                struct SearchInputState {
                    input: Entity<InputState>,
                    _subscription: Subscription,
                }
                let search_state = window.use_keyed_state(SharedString::from("summary-ollama-search-input"), cx, {
                    move |window, cx| {
                        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search installed models…"));
                        let subscription = cx.subscribe(&input, |_, _, _: &InputEvent, _| {});
                        SearchInputState { input, _subscription: subscription }
                    }
                });
                let search_input = search_state.read(cx).input.clone();
                let search_value = search_input.read(cx).value().to_string();
                let installed_names: Vec<String> = installed.iter().map(|m| m.name.clone()).collect();
                let visible_names = super::model_utils::filter_models(&installed_names, &search_value);

                let mut column = v_flex().w_full().gap_3();

                column = column.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(Input::new(&pull_input).small().flex_1())
                        .child(Button::new("ollama-pull").outline().label("Pull model").disabled(pull_value.trim().is_empty()).on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let name = pull_value.trim().to_string();
                                if !name.is_empty() {
                                    super::state::pull_ollama_model(cx, &view, name);
                                }
                            }
                        })),
                );

                if !installed.is_empty() {
                    column = column.child(Input::new(&search_input).small());
                }

                if let Some(e) = &error {
                    column = column.child(Label::new(e.clone()).text_xs().text_color(cx.theme().danger));
                }

                if loading && installed.is_empty() {
                    column = column.child(Label::new("Loading installed models…").text_xs().text_color(cx.theme().muted_foreground));
                } else if installed.is_empty() {
                    column = column.child(Label::new("No models installed yet.").text_xs().text_color(cx.theme().muted_foreground));
                } else if visible_names.is_empty() {
                    column = column.child(Label::new("No installed models match your search.").text_xs().text_color(cx.theme().muted_foreground));
                } else {
                    for model in installed.iter().filter(|m| visible_names.iter().any(|n| **n == m.name)) {
                        let name = model.name.clone();
                        let progress = pull_progress.get(&name).copied();
                        let is_selected = selected_model == name;
                        let delete_view = view.clone();
                        let delete_name = name.clone();

                        let mut row = h_flex()
                            .w_full()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(Label::new(name.clone()).text_sm())
                                    .child(
                                        Label::new(if let Some(pct) = progress {
                                            format!("{} · downloading… {}%", model.size, pct)
                                        } else if is_selected {
                                            format!("{} · selected", model.size)
                                        } else {
                                            model.size.clone()
                                        })
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground),
                                    ),
                            );
                        row = row.child(
                            Button::new(SharedString::from(format!("ollama-delete-{}", name)))
                                .outline()
                                .label("Delete")
                                .disabled(progress.is_some())
                                .on_click(move |_, _, cx| {
                                    super::state::delete_ollama_model(cx, delete_view.clone(), delete_name.clone());
                                }),
                        );
                        column = column.child(row);
                    }
                }

                column.into_any_element()
            }
        }))
}

/// Built-in AI model manager (mirrors `BuiltInModelManager`): live status
/// of the local llama.cpp models, with download/cancel/delete.
fn builtin_manager_group(view: &Entity<SettingsView>) -> SettingGroup {
    let view = view.clone();

    SettingGroup::new()
        .title("Built-in AI models")
        .description("Runs entirely on-device; nothing leaves your machine.")
        .item(SettingItem::render({
            let view = view.clone();
            move |_options, window, cx| {
                struct FetchTrigger;
                let _ = window.use_keyed_state(SharedString::from("summary-builtin-models-fetch"), cx, {
                    let view = view.clone();
                    move |_window, cx: &mut Context<FetchTrigger>| {
                        super::state::refresh_builtin_models(view.clone(), cx);
                        FetchTrigger
                    }
                });

                let cache = SettingsCache::global(cx);
                let models = cache.builtin_models.clone();
                let progress = cache.builtin_download_progress.clone();
                let selected_model = cache.summary.model.clone();

                if models.is_empty() {
                    return Label::new("Loading built-in AI models…")
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .into_any_element();
                }

                let mut column = v_flex().w_full().gap_3();
                for model in &models {
                    let name = model.name.clone();
                    let is_available = matches!(model.status, BuiltinModelStatus::Available);
                    let is_downloading = progress.contains_key(&name)
                        || matches!(model.status, BuiltinModelStatus::Downloading { .. });
                    let pct = progress.get(&name).copied();
                    let is_selected = selected_model == name;

                    let status_text = match (&model.status, pct) {
                        (_, Some(p)) => format!("Downloading… {}%", p),
                        (BuiltinModelStatus::Available, _) => {
                            if is_selected { "Ready · selected".to_string() } else { "Ready".to_string() }
                        }
                        (BuiltinModelStatus::NotDownloaded, _) => "Not downloaded".to_string(),
                        (BuiltinModelStatus::Downloading { progress }, _) => format!("Downloading… {}%", progress),
                        (BuiltinModelStatus::Corrupted { .. }, _) => "Corrupted — re-download".to_string(),
                        (BuiltinModelStatus::Error(e), _) => format!("Error: {}", e),
                    };

                    let mut row = h_flex()
                        .w_full()
                        .justify_between()
                        .items_center()
                        .gap_3()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(Label::new(model.display_name.clone()).text_sm())
                                .child(
                                    Label::new(format!("{} · {}", format_size_mb(model.size_mb), status_text))
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        );

                    let mut actions = h_flex().gap_2();
                    if is_available && !is_downloading {
                        if !is_selected {
                            let select_view = view.clone();
                            let select_name = name.clone();
                            actions = actions.child(
                                Button::new(SharedString::from(format!("builtin-select-{}", name)))
                                    .outline()
                                    .label("Select")
                                    .on_click(move |_, _, cx| {
                                        set_summary(
                                            cx,
                                            &select_view,
                                            |cfg| cfg.model = select_name.clone(),
                                        );
                                    }),
                            );
                        }
                        let delete_view = view.clone();
                        let delete_name = name.clone();
                        actions = actions.child(
                            Button::new(SharedString::from(format!("builtin-delete-{}", name)))
                                .outline()
                                .label("Delete")
                                .on_click(move |_, _, cx| {
                                    super::state::delete_builtin_model(delete_view.clone(), cx, delete_name.clone());
                                }),
                        );
                    } else if is_downloading {
                        let cancel_view = view.clone();
                        let cancel_name = name.clone();
                        actions = actions.child(
                            Button::new(SharedString::from(format!("builtin-cancel-{}", name)))
                                .outline()
                                .label("Cancel")
                                .on_click(move |_, _, cx| {
                                    super::state::cancel_builtin_download(cx, cancel_view.clone(), cancel_name.clone());
                                }),
                        );
                    } else {
                        let download_view = view.clone();
                        let download_name = name.clone();
                        let label = if matches!(model.status, BuiltinModelStatus::Corrupted { .. }) {
                            "Re-download"
                        } else {
                            "Download"
                        };
                        actions = actions.child(
                            Button::new(SharedString::from(format!("builtin-download-{}", name)))
                                .outline()
                                .label(label)
                                .on_click(move |_, _, cx| {
                                    super::state::download_builtin_model(cx, &download_view, download_name.clone());
                                }),
                        );
                    }

                    row = row.child(actions);
                    column = column.child(row);
                }

                column.into_any_element()
            }
        }))
}
