//! Shared, loaded-once cache backing every settings page.
//!
//! `gpui-kit`'s `SettingField` getters/setters are plain synchronous
//! `Fn(&App) -> T` / `Fn(T, &mut App)` closures — there's nowhere to
//! `.await` a DB round trip in them. So settings are read from SQLite once
//! (on `SettingsView::new`, via `Io`) into this `Global`, and every field
//! reads/writes the cache directly (instant, no flicker) while a setter
//! also fires an async save through `Io` to persist it. This is the same
//! shape the Tauri frontend uses (React state + a debounced/immediate save
//! call), just without React.
use std::collections::HashMap;

use gpui_kit::{App, BorrowAppContext, Entity, Global};
use serde::{Deserialize, Serialize};

use parley_core::audio::pw::PwDevice;
use parley_core::audio::recording_preferences::RecordingPreferences;
use parley_core::calendar::models::CalendarSourceRow;
use parley_core::calendar::repository::CalendarRepository;
use parley_core::database::repositories::setting::{
    SettingsRepository, KEY_BETA_FEATURES, KEY_DEFAULT_SUMMARY_TEMPLATE, KEY_NOTIFICATION_SETTINGS,
    KEY_THEME_PREFERENCE, KEY_UI_CONFIG,
};
use parley_core::mcp_config::McpServerInfo;
use parley_core::summary::CustomOpenAIConfig;
use parley_core::whisper_engine::ModelInfo;

use crate::app_state::AppServices;
use crate::runtime::Io;

use super::SettingsView;

/// Saved transcript config (`transcript_settings` table): which provider is
/// active, and — for `localWhisper` — which model.
#[derive(Clone, Default)]
pub struct TranscriptConfig {
    pub provider: String,
    pub model: String,
}

/// Saved summary/model config (`settings` table): the built-in-AI or remote
/// provider/model pair the summary engine uses, plus the Ollama endpoint.
#[derive(Clone, Default)]
pub struct SummaryConfig {
    pub provider: String,
    pub model: String,
    pub ollama_endpoint: String,
}

/// Mirrors `frontend/src-tauri/src/notifications/settings.rs`'s
/// `NotificationSettings`/`NotificationPreferences` field-for-field (same
/// snake_case names, no `#[serde(rename_all)]` on either side) so this app
/// reads/writes the exact same `app_settings` row under
/// `KEY_NOTIFICATION_SETTINGS` — including the subset
/// `parley-gpui/src/notifications.rs` already parses out of it. That
/// struct isn't reusable directly: it lives in the Tauri shell crate
/// (`frontend/src-tauri`), which `parley-gpui` deliberately does not
/// depend on (only `parley-core`).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    pub recording_notifications: bool,
    pub time_based_reminders: bool,
    pub meeting_reminders: bool,
    pub respect_do_not_disturb: bool,
    pub notification_sound: bool,
    pub system_permission_granted: bool,
    pub consent_given: bool,
    pub manual_dnd_mode: bool,
    pub notification_preferences: NotificationPreferences,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPreferences {
    pub show_recording_started: bool,
    pub show_recording_stopped: bool,
    pub show_recording_paused: bool,
    pub show_recording_resumed: bool,
    pub show_transcription_complete: bool,
    pub show_meeting_reminders: bool,
    pub show_system_errors: bool,
    pub meeting_reminder_minutes: Vec<u64>,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            recording_notifications: true,
            time_based_reminders: true,
            meeting_reminders: true,
            respect_do_not_disturb: true,
            notification_sound: true,
            system_permission_granted: false,
            consent_given: false,
            manual_dnd_mode: false,
            notification_preferences: NotificationPreferences::default(),
        }
    }
}

impl Default for NotificationPreferences {
    fn default() -> Self {
        Self {
            show_recording_started: false,
            show_recording_stopped: false,
            show_recording_paused: true,
            show_recording_resumed: true,
            show_transcription_complete: true,
            show_meeting_reminders: true,
            show_system_errors: true,
            meeting_reminder_minutes: vec![15, 5],
        }
    }
}

/// Feature toggles that don't belong to another settings struct — today just
/// "Live action items" (Settings → Recording). Stored under the historical
/// `KEY_BETA_FEATURES` key so existing choices carry over; the old
/// `import_and_retranscribe` field is ignored on read and dropped on the
/// next save (import was never actually gated on it).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureToggles {
    pub live_action_items: bool,
}

impl Default for FeatureToggles {
    fn default() -> Self {
        Self {
            live_action_items: false,
        }
    }
}

#[derive(Clone, Default)]
pub struct SettingsCache {
    pub loaded: bool,
    pub recording: RecordingPreferences,
    pub transcript: TranscriptConfig,
    pub summary: SummaryConfig,
    pub whisper_models: Vec<ModelInfo>,
    pub mic_devices: Vec<PwDevice>,
    pub system_devices: Vec<PwDevice>,
    /// modelName -> percent complete, for a model currently downloading.
    pub download_progress: HashMap<String, u8>,
    pub download_error: Option<String>,

    /// provider id ("openai"/"claude"/"groq"/"openrouter"/"custom-openai")
    /// -> whether an API key is currently saved for it. Keys themselves are
    /// never cached in memory once saved — only presence, so the field can
    /// render "configured"/"not set" without holding the plaintext key.
    pub api_key_present: HashMap<String, bool>,
    pub custom_openai: Option<CustomOpenAIConfig>,

    /// (id, name, description) tuples from `summary::templates::list_templates`.
    pub templates: Vec<(String, String, String)>,
    pub default_template_id: String,

    /// Raw `ui_config` JSON blob (`KEY_UI_CONFIG`) — kept as a `Value`
    /// rather than a typed struct because that key is documented as
    /// "shape owned by the frontend, opaque blob" and the React app stores
    /// additional fields here (e.g. `providerModelMap`) that a typed struct
    /// on this side would silently drop on save. Only the fields this app
    /// edits (`primaryLanguage`, `showConfidenceIndicator`, `isAutoSummary`)
    /// are read/written; everything else round-trips untouched.
    pub ui_config: serde_json::Value,

    pub features: FeatureToggles,
    pub notifications: NotificationSettings,

    pub calendar_sources: Vec<CalendarSourceRow>,
    pub calendar_busy: HashMap<String, bool>,

    pub mcp_info: Option<McpServerInfo>,

    /// "light" | "dark" | "system".
    pub theme_pref: String,

    /// Live-fetched summary model lists, keyed by provider id
    /// ("openai"/"claude"/"groq"/"openrouter"/"ollama") — populated
    /// on-demand by `fetch_provider_models` (mirrors `ModelSettingsModal`'s
    /// per-provider `models`/`openaiModels`/... state).
    pub provider_models: HashMap<String, Vec<String>>,
    pub provider_models_loading: HashMap<String, bool>,
    pub provider_models_error: HashMap<String, String>,

    /// Ollama model manager (Settings → Summary, Ollama section): models
    /// currently installed on the configured endpoint, mirroring
    /// `OllamaModelsList`. `ollama_pull_progress` is modelName -> percent
    /// for a pull currently in progress.
    pub ollama_installed: Vec<parley_core::ollama::OllamaModel>,
    pub ollama_installed_loading: bool,
    pub ollama_pull_progress: HashMap<String, u8>,
    pub ollama_error: Option<String>,

    /// Built-in AI model manager (Settings → Summary, "Built-in AI"
    /// section): live status of the local llama.cpp models, mirroring
    /// `BuiltInModelManager`. `builtin_download_progress` is modelName ->
    /// percent for a download currently in progress.
    pub builtin_models: Vec<parley_core::summary::summary_engine::ModelInfo>,
    pub builtin_download_progress: HashMap<String, u8>,
}

impl Global for SettingsCache {}

impl SettingsCache {
    pub fn global(cx: &App) -> &SettingsCache {
        cx.global::<SettingsCache>()
    }

    pub fn ui_str(&self, key: &str) -> String {
        self.ui_config
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    }

    pub fn ui_bool(&self, key: &str, default: bool) -> bool {
        self.ui_config
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }
}

const API_KEY_PROVIDERS: &[&str] = &["openai", "claude", "groq", "openrouter"];

/// Load every setting this view needs from SQLite/the whisper engine/the
/// PipeWire registry, then populate the `SettingsCache` global and redraw
/// `view`. Best-effort per field: a failure on one doesn't block the rest.
pub fn load(view: Entity<SettingsView>, cx: &mut App) {
    if !cx.has_global::<SettingsCache>() {
        cx.set_global(SettingsCache::default());
    }

    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();

    // Every core call below needs a live tokio reactor (sqlx is
    // `runtime-tokio`), which GPUI's own executor doesn't provide — so the
    // whole batch runs as one future on `Io`'s tokio runtime, and only the
    // finished result crosses back over to a GPUI task to update the cache.
    cx.spawn(async move |cx| {
        let loaded = io
            .spawn(async move {
                let recording =
                    parley_core::audio::recording_preferences::load_recording_preferences(
                        pool.clone(),
                    )
                    .await
                    .unwrap_or_default();

                let transcript = match &pool {
                    Some(pool) => match SettingsRepository::get_transcript_config(pool).await {
                        Ok(Some(cfg)) => TranscriptConfig {
                            provider: cfg.provider,
                            model: cfg.model,
                        },
                        _ => TranscriptConfig {
                            provider: "localWhisper".to_string(),
                            model: parley_core::config::DEFAULT_WHISPER_MODEL.to_string(),
                        },
                    },
                    None => TranscriptConfig {
                        provider: "localWhisper".to_string(),
                        model: parley_core::config::DEFAULT_WHISPER_MODEL.to_string(),
                    },
                };

                let summary = match &pool {
                    Some(pool) => match SettingsRepository::get_model_config(pool).await {
                        Ok(Some(cfg)) => SummaryConfig {
                            provider: cfg.provider,
                            model: cfg.model,
                            ollama_endpoint: cfg.ollama_endpoint.unwrap_or_default(),
                        },
                        _ => SummaryConfig::default(),
                    },
                    None => SummaryConfig::default(),
                };

                let whisper_models = parley_core::whisper_engine::whisper_get_available_models()
                    .await
                    .unwrap_or_default();

                let (mic_devices, system_devices) =
                    match parley_core::audio::list_audio_devices().await {
                        Ok(devices) => {
                            let mic = devices
                                .iter()
                                .filter(|d| {
                                    d.kind == parley_core::audio::pw::PwDeviceKind::Microphone
                                })
                                .cloned()
                                .collect();
                            let sys = devices
                                .into_iter()
                                .filter(|d| {
                                    d.kind == parley_core::audio::pw::PwDeviceKind::System
                                })
                                .collect();
                            (mic, sys)
                        }
                        Err(e) => {
                            log::warn!("settings: failed to enumerate audio devices: {}", e);
                            (Vec::new(), Vec::new())
                        }
                    };

                let mut api_key_present = HashMap::new();
                let mut custom_openai = None;
                let mut default_template_id = String::new();
                let mut ui_config = serde_json::Value::Object(Default::default());
                let mut features = FeatureToggles::default();
                let mut notifications = NotificationSettings::default();
                let mut calendar_sources = Vec::new();
                let mut theme_pref = "system".to_string();

                if let Some(pool) = &pool {
                    for provider in API_KEY_PROVIDERS {
                        let has_key = SettingsRepository::get_api_key(pool, provider)
                            .await
                            .ok()
                            .flatten()
                            .map(|k| !k.is_empty())
                            .unwrap_or(false);
                        api_key_present.insert(provider.to_string(), has_key);
                    }

                    custom_openai = SettingsRepository::get_custom_openai_config(pool)
                        .await
                        .ok()
                        .flatten();
                    api_key_present.insert(
                        "custom-openai".to_string(),
                        custom_openai
                            .as_ref()
                            .and_then(|c| c.api_key.as_ref())
                            .map(|k| !k.is_empty())
                            .unwrap_or(false),
                    );

                    default_template_id = SettingsRepository::get_setting::<String>(
                        pool,
                        KEY_DEFAULT_SUMMARY_TEMPLATE,
                    )
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                    if let Ok(Some(json)) =
                        SettingsRepository::get_setting_json(pool, KEY_UI_CONFIG).await
                    {
                        if let Ok(value) = serde_json::from_str(&json) {
                            ui_config = value;
                        }
                    }

                    features = SettingsRepository::get_setting::<FeatureToggles>(pool, KEY_BETA_FEATURES)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();

                    notifications = SettingsRepository::get_setting::<NotificationSettings>(
                        pool,
                        KEY_NOTIFICATION_SETTINGS,
                    )
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                    calendar_sources = CalendarRepository::list_sources(pool)
                        .await
                        .unwrap_or_default();

                    theme_pref =
                        SettingsRepository::get_setting::<String>(pool, KEY_THEME_PREFERENCE)
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "system".to_string());
                }

                let templates = parley_core::summary::templates::list_templates();
                if default_template_id.is_empty() {
                    if let Some((id, _, _)) = templates.first() {
                        default_template_id = id.clone();
                    }
                }

                let mcp_info = parley_core::mcp_config::get_mcp_server_info().ok();

                (
                    recording,
                    transcript,
                    summary,
                    whisper_models,
                    mic_devices,
                    system_devices,
                    api_key_present,
                    custom_openai,
                    templates,
                    default_template_id,
                    ui_config,
                    features,
                    notifications,
                    calendar_sources,
                    mcp_info,
                    theme_pref,
                )
            })
            .await;

        let Ok((
            recording,
            transcript,
            summary,
            whisper_models,
            mic_devices,
            system_devices,
            api_key_present,
            custom_openai,
            templates,
            default_template_id,
            ui_config,
            features,
            notifications,
            calendar_sources,
            mcp_info,
            theme_pref,
        )) = loaded
        else {
            log::warn!("settings: load task panicked");
            return;
        };

        let _ = cx.update(|cx| {
            cx.update_global::<SettingsCache, _>(|cache, _| {
                cache.loaded = true;
                cache.recording = recording;
                cache.transcript = transcript;
                cache.summary = summary;
                cache.whisper_models = whisper_models;
                cache.mic_devices = mic_devices;
                cache.system_devices = system_devices;
                cache.api_key_present = api_key_present;
                cache.custom_openai = custom_openai;
                cache.templates = templates;
                cache.default_template_id = default_template_id;
                cache.ui_config = ui_config;
                cache.features = features;
                cache.notifications = notifications;
                cache.calendar_sources = calendar_sources;
                cache.mcp_info = mcp_info;
                cache.theme_pref = theme_pref;
            });
        });
        let _ = view.update(cx, |_, cx| cx.notify());
    })
    .detach();
}

/// Persist `preferences` (recording tab) to SQLite. Fire-and-forget.
pub fn save_recording(cx: &mut App, preferences: RecordingPreferences) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();
    io.spawn(async move {
        if let Err(e) =
            parley_core::audio::recording_preferences::save_recording_preferences(
                pool,
                &preferences,
            )
            .await
        {
            log::warn!("settings: failed to save recording preferences: {}", e);
        }
    });
}

/// Persist the transcript (Whisper) config to SQLite.
pub fn save_transcript(cx: &mut App, provider: String, model: String) {
    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, transcript config not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = SettingsRepository::save_transcript_config(&pool, &provider, &model).await
        {
            log::warn!("settings: failed to save transcript config: {}", e);
        }
    });
}

/// Persist the summary/model config to SQLite.
pub fn save_summary(
    cx: &mut App,
    provider: String,
    model: String,
    whisper_model: String,
    ollama_endpoint: Option<String>,
) {
    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, model config not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = SettingsRepository::save_model_config(
            &pool,
            &provider,
            &model,
            &whisper_model,
            ollama_endpoint.as_deref(),
        )
        .await
        {
            log::warn!("settings: failed to save model config: {}", e);
        }
    });
}

/// Save (or overwrite) the API key for `provider`, updating the in-memory
/// "configured" flag immediately and persisting through the same
/// `SettingsRepository::save_api_key` the Tauri `api_save_model_config`
/// command uses.
pub fn save_api_key(cx: &mut App, view: &Entity<SettingsView>, provider: String, api_key: String) {
    cx.global_mut::<SettingsCache>()
        .api_key_present
        .insert(provider.clone(), !api_key.is_empty());
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, API key not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if api_key.is_empty() {
            if let Err(e) = SettingsRepository::delete_api_key(&pool, &provider).await {
                log::warn!("settings: failed to clear API key for {}: {}", provider, e);
            }
        } else if let Err(e) = SettingsRepository::save_api_key(&pool, &provider, &api_key).await {
            log::warn!("settings: failed to save API key for {}: {}", provider, e);
        }
    });
}

/// Save the custom OpenAI-compatible endpoint config (`api_save_custom_openai_config`'s
/// core logic).
pub fn save_custom_openai(cx: &mut App, view: &Entity<SettingsView>, config: CustomOpenAIConfig) {
    cx.global_mut::<SettingsCache>().custom_openai = Some(config.clone());
    cx.global_mut::<SettingsCache>()
        .api_key_present
        .insert(
            "custom-openai".to_string(),
            config.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false),
        );
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, custom OpenAI config not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = SettingsRepository::save_custom_openai_config(&pool, &config).await {
            log::warn!("settings: failed to save custom OpenAI config: {}", e);
        }
    });
}

/// Save the default summary template id (`KEY_DEFAULT_SUMMARY_TEMPLATE`).
pub fn save_default_template(cx: &mut App, view: &Entity<SettingsView>, template_id: String) {
    cx.global_mut::<SettingsCache>().default_template_id = template_id.clone();
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, default template not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) =
            SettingsRepository::set_setting(&pool, KEY_DEFAULT_SUMMARY_TEMPLATE, &template_id)
                .await
        {
            log::warn!("settings: failed to save default template: {}", e);
        }
    });
}

/// Merge `key: value` into the cached `ui_config` blob and persist the
/// whole blob (a full replace, like the Tauri `api_save_ui_config`
/// command) — every other field already in the blob (e.g. React's
/// `providerModelMap`) is preserved since we mutate the cached `Value`
/// in place rather than constructing a fresh one.
pub fn save_ui_field(
    cx: &mut App,
    view: &Entity<SettingsView>,
    key: &'static str,
    value: serde_json::Value,
) {
    let updated = {
        let cache = cx.global_mut::<SettingsCache>();
        if !cache.ui_config.is_object() {
            cache.ui_config = serde_json::Value::Object(Default::default());
        }
        if let Some(obj) = cache.ui_config.as_object_mut() {
            obj.insert(key.to_string(), value);
        }
        cache.ui_config.clone()
    };
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, UI config not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = SettingsRepository::set_setting(&pool, KEY_UI_CONFIG, &updated).await {
            log::warn!("settings: failed to save UI config: {}", e);
        }
    });
}

/// Save feature toggles (`KEY_BETA_FEATURES`).
pub fn save_features(cx: &mut App, view: &Entity<SettingsView>, features: FeatureToggles) {
    cx.global_mut::<SettingsCache>().features = features.clone();
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, feature toggles not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = SettingsRepository::set_setting(&pool, KEY_BETA_FEATURES, &features).await
        {
            log::warn!("settings: failed to save feature toggles: {}", e);
        }
    });
}

/// Save notification settings (`KEY_NOTIFICATION_SETTINGS`) — the same row
/// `parley-gpui/src/notifications.rs` reads back for tray/DBus
/// notifications, and the same shape the Tauri `set_notification_settings`
/// command persists.
pub fn save_notifications(cx: &mut App, view: &Entity<SettingsView>, settings: NotificationSettings) {
    cx.global_mut::<SettingsCache>().notifications = settings.clone();
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, notification settings not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) =
            SettingsRepository::set_setting(&pool, KEY_NOTIFICATION_SETTINGS, &settings).await
        {
            log::warn!("settings: failed to save notification settings: {}", e);
        }
    });
}

/// Save the theme preference (`KEY_THEME_PREFERENCE`).
pub fn save_theme_pref(cx: &mut App, mode: String) {
    cx.global_mut::<SettingsCache>().theme_pref = mode.clone();

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, theme preference not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = SettingsRepository::set_setting(&pool, KEY_THEME_PREFERENCE, &mode).await {
            log::warn!("settings: failed to save theme preference: {}", e);
        }
    });
}

/// Add a calendar ICS source, refresh it immediately, then reload the
/// cache's `calendar_sources` list. Mirrors the Tauri `calendar_add_source`
/// + an immediate `calendar_refresh_source` (the React `CalendarSettings`
/// UI does the same on add).
pub fn calendar_add(cx: &mut App, view: Entity<SettingsView>, url: String, label: Option<String>) {
    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, calendar source not added");
        return;
    };
    let io = Io::global(cx).clone();
    cx.spawn(async move |cx| {
        let result = io
            .spawn(async move {
                let normalized = parley_core::calendar::normalize_calendar_url(&url)?;
                let source = CalendarRepository::add_source(
                    &pool,
                    &normalized,
                    label.as_deref().filter(|s| !s.trim().is_empty()),
                )
                .await
                .map_err(|e| e.to_string())?;
                let _ = parley_core::calendar::refresh_source(&pool, &source.id).await;
                CalendarRepository::list_sources(&pool)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;

        if let Ok(Ok(sources)) = result {
            let _ = cx.update(|cx| {
                cx.update_global::<SettingsCache, _>(|cache, _| {
                    cache.calendar_sources = sources;
                });
            });
            let _ = view.update(cx, |_, cx| cx.notify());
        } else if let Ok(Err(e)) = result {
            log::warn!("settings: failed to add calendar source: {}", e);
        }
    })
    .detach();
}

/// Remove a calendar source, then reload the list.
pub fn calendar_remove(cx: &mut App, view: Entity<SettingsView>, source_id: String) {
    let Some(pool) = AppServices::global(cx).pool() else {
        return;
    };
    let io = Io::global(cx).clone();
    cx.spawn(async move |cx| {
        let sources = io
            .spawn(async move {
                let _ = CalendarRepository::remove_source(&pool, &source_id).await;
                CalendarRepository::list_sources(&pool).await
            })
            .await;

        if let Ok(Ok(sources)) = sources {
            let _ = cx.update(|cx| {
                cx.update_global::<SettingsCache, _>(|cache, _| {
                    cache.calendar_sources = sources;
                });
            });
            let _ = view.update(cx, |_, cx| cx.notify());
        }
    })
    .detach();
}

/// Refresh one calendar source's events, then reload the list (which picks
/// up the new `last_fetched_at`/`last_error`).
pub fn calendar_refresh(cx: &mut App, view: Entity<SettingsView>, source_id: String) {
    let Some(pool) = AppServices::global(cx).pool() else {
        return;
    };
    cx.global_mut::<SettingsCache>()
        .calendar_busy
        .insert(source_id.clone(), true);
    let _ = view.update(cx, |_, cx| cx.notify());

    let io = Io::global(cx).clone();
    cx.spawn(async move |cx| {
        let source_id_for_pool = source_id.clone();
        let sources = io
            .spawn(async move {
                let _ =
                    parley_core::calendar::refresh_source(&pool, &source_id_for_pool).await;
                CalendarRepository::list_sources(&pool).await
            })
            .await;

        if let Ok(Ok(sources)) = sources {
            let _ = cx.update(|cx| {
                cx.update_global::<SettingsCache, _>(|cache, _| {
                    cache.calendar_sources = sources;
                    cache.calendar_busy.insert(source_id.clone(), false);
                });
            });
            let _ = view.update(cx, |_, cx| cx.notify());
        }
    })
    .detach();
}

/// Fetch the live model list for `provider` (Settings → Summary's model
/// picker) and cache it in `provider_models`. Mirrors
/// `ModelSettingsModal`'s `fetchOllamaModels`/`loadOpenAIModels`/etc: keys
/// (openai/claude/groq) are read from SQLite server-side rather than kept in
/// the in-memory cache, since `SettingsCache` deliberately never holds
/// plaintext API keys once saved (see its doc comment).
pub fn fetch_provider_models(cx: &mut App, view: Entity<SettingsView>, provider: String) {
    {
        let cache = cx.global_mut::<SettingsCache>();
        cache.provider_models_loading.insert(provider.clone(), true);
        cache.provider_models_error.remove(&provider);
    }
    let _ = view.update(cx, |_, cx| cx.notify());

    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();
    let ollama_endpoint = SettingsCache::global(cx).summary.ollama_endpoint.clone();

    let provider_for_task = provider.clone();
    cx.spawn(async move |cx| {
        let result: Result<Vec<String>, String> = io
            .spawn(async move {
                match provider_for_task.as_str() {
                    "ollama" => {
                        let endpoint = if ollama_endpoint.is_empty() { None } else { Some(ollama_endpoint) };
                        parley_core::ollama::get_ollama_models(endpoint)
                            .await
                            .map(|models| models.into_iter().map(|m| m.name).collect())
                    }
                    "openrouter" => parley_core::openrouter::get_openrouter_models()
                        .await
                        .map(|models| models.into_iter().map(|m| m.id).collect()),
                    "openai" | "claude" | "groq" => {
                        let key = match &pool {
                            Some(pool) => SettingsRepository::get_api_key(pool, &provider_for_task)
                                .await
                                .ok()
                                .flatten(),
                            None => None,
                        };
                        match provider_for_task.as_str() {
                            "openai" => parley_core::openai::openai::get_openai_models(key)
                                .await
                                .map(|models| models.into_iter().map(|m| m.id).collect()),
                            "claude" => parley_core::anthropic::anthropic::get_anthropic_models(key)
                                .await
                                .map(|models| models.into_iter().map(|m| m.id).collect()),
                            "groq" => parley_core::groq::groq::get_groq_models(key)
                                .await
                                .map(|models| models.into_iter().map(|m| m.id).collect()),
                            _ => unreachable!(),
                        }
                    }
                    _ => Ok(Vec::new()),
                }
            })
            .await
            .unwrap_or_else(|_| Err("model list fetch task panicked".to_string()));

        let _ = cx.update(|cx| {
            cx.update_global::<SettingsCache, _>(|cache, _| {
                cache.provider_models_loading.insert(provider.clone(), false);
                match result {
                    Ok(models) => {
                        cache.provider_models.insert(provider.clone(), models);
                        cache.provider_models_error.remove(&provider);
                    }
                    Err(e) => {
                        log::warn!("settings: failed to fetch {} models: {}", provider, e);
                        cache.provider_models_error.insert(provider, e);
                    }
                }
            });
        });
        let _ = view.update(cx, |_, cx| cx.notify());
    })
    .detach();
}

/// Refresh the Ollama-installed model list (Settings → Summary, Ollama
/// model manager) against the configured endpoint. Mirrors
/// `fetchOllamaModels`.
pub fn refresh_ollama_models(view: Entity<SettingsView>, cx: &mut App) {
    cx.global_mut::<SettingsCache>().ollama_installed_loading = true;
    let _ = view.update(cx, |_, cx| cx.notify());

    let services = AppServices::global(cx);
    let io = services.io.clone();
    let ollama_endpoint = SettingsCache::global(cx).summary.ollama_endpoint.clone();

    cx.spawn(async move |cx| {
        let endpoint = if ollama_endpoint.is_empty() { None } else { Some(ollama_endpoint) };
        let result = io.spawn(parley_core::ollama::get_ollama_models(endpoint)).await;

        let _ = cx.update(|cx| {
            cx.update_global::<SettingsCache, _>(|cache, _| {
                cache.ollama_installed_loading = false;
                match result {
                    Ok(Ok(models)) => {
                        cache.ollama_installed = models;
                        cache.ollama_error = None;
                    }
                    Ok(Err(e)) => {
                        cache.ollama_installed = Vec::new();
                        cache.ollama_error = Some(e);
                    }
                    Err(_) => {
                        cache.ollama_error = Some("model list refresh task panicked".to_string());
                    }
                }
            });
        });
        let _ = view.update(cx, |_, cx| cx.notify());
    })
    .detach();
}

/// Pull an Ollama model by name (Settings → Summary, Ollama model manager's
/// "Download" button). Progress arrives via the `ollama-model-download-*`
/// core events, mirrored into `ollama_pull_progress` by `SettingsView`.
pub fn pull_ollama_model(cx: &mut App, view: &Entity<SettingsView>, model_name: String) {
    let services = AppServices::global(cx);
    let sink = services.sink.clone();
    let io = services.io.clone();
    let ollama_endpoint = SettingsCache::global(cx).summary.ollama_endpoint.clone();

    {
        let cache = cx.global_mut::<SettingsCache>();
        cache.ollama_pull_progress.insert(model_name.clone(), 0);
        cache.ollama_error = None;
    }
    let _ = view.update(cx, |_, cx| cx.notify());

    io.spawn(async move {
        let endpoint = if ollama_endpoint.is_empty() { None } else { Some(ollama_endpoint) };
        if let Err(e) = parley_core::ollama::pull_ollama_model_with_progress(sink, model_name.clone(), endpoint)
            .await
        {
            log::warn!("settings: failed to pull Ollama model '{}': {}", model_name, e);
        }
    });
}

/// Delete an installed Ollama model, then refresh the installed list.
pub fn delete_ollama_model(cx: &mut App, view: Entity<SettingsView>, model_name: String) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let ollama_endpoint = SettingsCache::global(cx).summary.ollama_endpoint.clone();

    cx.spawn(async move |cx| {
        let endpoint = if ollama_endpoint.is_empty() { None } else { Some(ollama_endpoint) };
        let result = io
            .spawn(parley_core::ollama::delete_ollama_model(model_name.clone(), endpoint))
            .await;

        if let Ok(Err(e)) = &result {
            log::warn!("settings: failed to delete Ollama model '{}': {}", model_name, e);
        }
        let _ = cx.update(|cx| {
            refresh_ollama_models(view.clone(), cx);
        });
    })
    .detach();
}

/// List the built-in AI models with live status (Settings → Summary,
/// "Built-in AI" section). Mirrors `BuiltInModelManager.fetchModels`.
pub fn refresh_builtin_models(view: Entity<SettingsView>, cx: &mut App) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let manager_slot = services.builtin_manager_slot();

    cx.spawn(async move |cx| {
        let result = io
            .spawn(async move {
                let manager = parley_core::summary::summary_engine::service::ensure_manager(&manager_slot)
                    .await?;
                Ok::<_, String>(manager.list_models().await)
            })
            .await;

        if let Ok(Ok(models)) = result {
            let _ = cx.update(|cx| {
                cx.update_global::<SettingsCache, _>(|cache, _| {
                    cache.builtin_models = models;
                });
            });
            let _ = view.update(cx, |_, cx| cx.notify());
        } else if let Ok(Err(e)) = result {
            log::warn!("settings: failed to list built-in AI models: {}", e);
        }
    })
    .detach();
}

/// Download a built-in AI model. Progress arrives via
/// `builtin-ai-download-progress`, mirrored by `SettingsView`, which also
/// calls `refresh_builtin_models` once the download settles.
pub fn download_builtin_model(cx: &mut App, view: &Entity<SettingsView>, model_name: String) {
    let services = AppServices::global(cx);
    let sink = services.sink.clone();
    let io = services.io.clone();
    let manager_slot = services.builtin_manager_slot();

    cx.global_mut::<SettingsCache>()
        .builtin_download_progress
        .insert(model_name.clone(), 0);
    let _ = view.update(cx, |_, cx| cx.notify());

    io.spawn(async move {
        match parley_core::summary::summary_engine::service::ensure_manager(&manager_slot).await {
            Ok(manager) => {
                if let Err(e) = parley_core::summary::summary_engine::service::download_builtin_ai_model(
                    manager,
                    model_name.clone(),
                    sink,
                )
                .await
                {
                    log::warn!("settings: failed to download built-in AI model '{}': {}", model_name, e);
                }
            }
            Err(e) => log::warn!("settings: built-in AI model manager unavailable: {}", e),
        }
    });
}

/// Cancel an in-progress built-in AI model download.
pub fn cancel_builtin_download(cx: &mut App, view: Entity<SettingsView>, model_name: String) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let manager_slot = services.builtin_manager_slot();

    cx.global_mut::<SettingsCache>()
        .builtin_download_progress
        .remove(&model_name);
    let _ = view.update(cx, |_, cx| cx.notify());

    io.spawn(async move {
        if let Ok(manager) = parley_core::summary::summary_engine::service::ensure_manager(&manager_slot).await {
            if let Err(e) = manager.cancel_download(&model_name).await {
                log::warn!("settings: failed to cancel built-in AI download '{}': {}", model_name, e);
            }
        }
    });
}

/// Delete a downloaded (or corrupted) built-in AI model, then refresh the
/// list.
pub fn delete_builtin_model(view: Entity<SettingsView>, cx: &mut App, model_name: String) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let manager_slot = services.builtin_manager_slot();

    cx.spawn(async move |cx| {
        let result = io
            .spawn(async move {
                let manager = parley_core::summary::summary_engine::service::ensure_manager(&manager_slot)
                    .await?;
                manager.delete_model(&model_name).await.map_err(|e| e.to_string())
            })
            .await;

        if let Ok(Err(e)) = result {
            log::warn!("settings: failed to delete built-in AI model: {}", e);
        }
        let _ = cx.update(|cx| {
            refresh_builtin_models(view.clone(), cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_str_reads_a_present_string_field() {
        let cache = SettingsCache {
            ui_config: serde_json::json!({"primaryLanguage": "fr"}),
            ..Default::default()
        };
        assert_eq!(cache.ui_str("primaryLanguage"), "fr");
    }

    #[test]
    fn ui_str_defaults_to_empty_when_missing() {
        let cache = SettingsCache::default();
        assert_eq!(cache.ui_str("primaryLanguage"), "");
    }

    #[test]
    fn ui_bool_reads_a_present_bool_field() {
        let cache = SettingsCache {
            ui_config: serde_json::json!({"isAutoSummary": true}),
            ..Default::default()
        };
        assert!(cache.ui_bool("isAutoSummary", false));
    }

    #[test]
    fn ui_bool_falls_back_to_the_given_default_when_missing() {
        let cache = SettingsCache::default();
        assert!(!cache.ui_bool("isAutoSummary", false));
        assert!(cache.ui_bool("isAutoSummary", true));
    }

    #[test]
    fn ui_config_fields_other_than_the_one_written_are_preserved() {
        // Regression guard for `save_ui_field`'s "merge into cached Value,
        // full-replace on save" approach: React's `providerModelMap` (or
        // any other field it stores in the same blob) must round-trip
        // untouched when this app only edits its own fields.
        let mut cache = SettingsCache {
            ui_config: serde_json::json!({"providerModelMap": {"openai": "gpt-4o"}}),
            ..Default::default()
        };
        if let Some(obj) = cache.ui_config.as_object_mut() {
            obj.insert("primaryLanguage".to_string(), serde_json::Value::String("en".to_string()));
        }
        assert_eq!(cache.ui_str("primaryLanguage"), "en");
        assert_eq!(
            cache.ui_config.get("providerModelMap").and_then(|v| v.get("openai")),
            Some(&serde_json::Value::String("gpt-4o".to_string()))
        );
    }

    #[test]
    fn notification_settings_default_matches_the_tauri_shell() {
        // Mirrors `frontend/src-tauri/src/notifications/settings.rs`'s
        // `NotificationSettings::default()`/`NotificationPreferences::default()`
        // exactly — a divergence here would mean a fresh `app_settings` row
        // written by this app doesn't match what the Tauri app expects.
        let defaults = NotificationSettings::default();
        assert!(defaults.recording_notifications);
        assert!(defaults.time_based_reminders);
        assert!(defaults.meeting_reminders);
        assert!(defaults.respect_do_not_disturb);
        assert!(defaults.notification_sound);
        assert!(!defaults.system_permission_granted);
        assert!(!defaults.consent_given);
        assert!(!defaults.manual_dnd_mode);
        assert!(!defaults.notification_preferences.show_recording_started);
        assert!(!defaults.notification_preferences.show_recording_stopped);
        assert!(defaults.notification_preferences.show_recording_paused);
        assert!(defaults.notification_preferences.show_recording_resumed);
        assert!(defaults.notification_preferences.show_transcription_complete);
        assert!(defaults.notification_preferences.show_meeting_reminders);
        assert!(defaults.notification_preferences.show_system_errors);
        assert_eq!(defaults.notification_preferences.meeting_reminder_minutes, vec![15, 5]);
    }

    #[test]
    fn feature_toggles_default_to_off() {
        let defaults = FeatureToggles::default();
        assert!(!defaults.live_action_items);
    }

    #[test]
    fn notification_settings_round_trips_through_json() {
        let settings = NotificationSettings::default();
        let json = serde_json::to_string(&settings).expect("serialize");
        let back: NotificationSettings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.recording_notifications, settings.recording_notifications);
        assert_eq!(
            back.notification_preferences.meeting_reminder_minutes,
            settings.notification_preferences.meeting_reminder_minutes
        );
    }
}
