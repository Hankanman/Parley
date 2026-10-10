//! The Recording page: device pickers, start/pause/resume/stop controls,
//! live level meters, and the live transcript. Behaviour mirrors the React
//! recording home (`frontend/src/app/page.tsx` and friends) — see the
//! module doc on `super` for the mapping.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Local;
use gpui_kit::component::{
    ActiveTheme, Disableable as _, IconName, IndexPath, StyledExt as _, WindowExt, h_flex, v_flex,
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
    label::Label,
    message_scroller::{MessageScroller, MessageScrollerState},
    notification::{Notification, NotificationType},
    select::{Select, SelectEvent, SelectItem, SelectState},
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::Deserialize;

use parley_core::audio::devices;
use parley_core::audio::recording_phase::{RecordingPhase, RecordingSnapshot};
use parley_core::audio::recording_preferences::{self, RecordingPreferences};
use parley_core::audio::recording_service::{self, RecordingArgs, StartRequest};
use parley_core::audio::simple_level_monitor;
use parley_core::audio::transcription::TranscriptUpdate;
use parley_core::calendar::repository::CalendarRepository;
use parley_core::database::repositories::setting::{
    SettingsRepository, KEY_BETA_FEATURES,
};
use parley_core::summary::live_action_items;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;
use crate::shell::{navigate, Route};

use super::logic::{self, Levels, RowChange, TranscriptRow};

const DEFAULT_DEVICE_ID: &str = "default";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One selectable entry in the mic/system device dropdowns.
#[derive(Debug, Clone, PartialEq)]
struct DeviceOption {
    id: SharedString,
    label: SharedString,
}

impl SelectItem for DeviceOption {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

/// Wire shape of the `transcript-partial` event
/// (`parley_core::audio::transcription::partial_worker`'s private
/// `PartialUpdate`) — mirrored here since it isn't a public type.
#[derive(Debug, Clone, Deserialize)]
struct PartialUpdatePayload {
    /// "mic" | "system"
    source: String,
    text: String,
    #[allow(dead_code)]
    utterance_id: u64,
}

#[derive(Debug, Deserialize)]
struct RecordingStoppedPayload {
    meeting_id: Option<String>,
    folder_path: Option<String>,
}

/// Wire shape of a `live-action-items` event's `items` entries (the
/// private `LiveItem` in `parley_core::summary::live_action_items`,
/// mirrored the same way `PartialUpdatePayload` mirrors `partial_worker`'s).
#[derive(Debug, Clone, Deserialize)]
struct LiveActionItemPayload {
    text: String,
    #[allow(dead_code)]
    assignee: Option<String>,
    #[allow(dead_code)]
    due_hint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LiveActionItemsEvent {
    items: Vec<LiveActionItemPayload>,
}

/// Only the one toggle this view needs ("Live action items", in Settings →
/// Recording), read straight from its row — stored under the historical
/// `KEY_BETA_FEATURES` key, default off. Deliberately not reusing
/// `views::settings::state::FeatureToggles`: that module is private to the
/// settings page, the same pattern `notifications.rs`'s `SettingsMini` uses
/// for the notification-settings row.
#[derive(Debug, Deserialize, Default)]
struct FeatureTogglesMini {
    #[serde(default)]
    live_action_items: bool,
}

/// Case/whitespace-insensitive dedupe key, mirroring
/// `useLiveActionItems.ts`'s `key()`.
fn action_item_key(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', ' '])
        .to_string()
}

fn is_active_phase(phase: RecordingPhase) -> bool {
    matches!(phase, RecordingPhase::Recording | RecordingPhase::Paused)
}

/// Tracks the server-reported active recording duration so the header can
/// tick locally between `recording-state` events instead of only updating
/// on phase transitions. Re-based on every snapshot.
#[derive(Debug, Clone, Copy, Default)]
struct ElapsedClock {
    base_active_secs: f64,
    base_host_ms: u64,
    running: bool,
}

impl ElapsedClock {
    fn rebase(&mut self, snapshot: &RecordingSnapshot) {
        self.base_active_secs = snapshot.active_duration_secs.unwrap_or(0.0);
        self.base_host_ms = now_ms();
        self.running = snapshot.phase == RecordingPhase::Recording;
    }

    fn displayed_secs(&self) -> f64 {
        if self.running {
            self.base_active_secs + (now_ms().saturating_sub(self.base_host_ms)) as f64 / 1000.0
        } else {
            self.base_active_secs
        }
    }
}

pub struct RecordingView {
    meeting_name: Entity<InputState>,
    mic_select: Entity<SelectState<Vec<DeviceOption>>>,
    system_select: Entity<SelectState<Vec<DeviceOption>>>,
    snapshot: RecordingSnapshot,
    elapsed: ElapsedClock,
    rows: Vec<TranscriptRow>,
    last_scroller_len: usize,
    scroller: Entity<MessageScrollerState>,
    /// In-progress preview text keyed by source ("mic" | "system"), from
    /// `transcript-partial` — replaced by the final row once it lands.
    partials: HashMap<String, String>,
    levels: Levels,
    refining: bool,
    pending_toasts: Vec<(NotificationType, String)>,
    /// Provisional action items from the beta live extractor — see
    /// `restart_live_action_items`/`stop_live_action_items`.
    live_action_items: Vec<String>,
    /// Calendar event matched at the moment recording started, stashed so
    /// `stop`'s `recording-stopped` payload (which carries the fresh
    /// `meeting_id`) can link the two. Mirrors
    /// `frontend/src/lib/recordingCalendarLink.ts`'s sessionStorage handoff.
    pending_calendar_event_id: Option<String>,
}

impl RecordingView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let default_title = logic::default_meeting_title(Local::now());
        let meeting_name =
            cx.new(|cx| InputState::new(window, cx).default_value(default_title));

        let mic_select = cx.new(|cx| {
            SelectState::new(
                vec![DeviceOption {
                    id: DEFAULT_DEVICE_ID.into(),
                    label: "Default Microphone".into(),
                }],
                Some(IndexPath::default()),
                window,
                cx,
            )
        });
        let system_select = cx.new(|cx| {
            SelectState::new(
                vec![DeviceOption {
                    id: DEFAULT_DEVICE_ID.into(),
                    label: "Default System Audio".into(),
                }],
                Some(IndexPath::default()),
                window,
                cx,
            )
        });

        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));

        let mut this = Self {
            meeting_name,
            mic_select,
            system_select,
            snapshot: idle_snapshot(),
            elapsed: ElapsedClock::default(),
            rows: Vec::new(),
            last_scroller_len: 0,
            scroller,
            partials: HashMap::new(),
            levels: Levels::default(),
            refining: false,
            pending_toasts: Vec::new(),
            live_action_items: Vec::new(),
            pending_calendar_event_id: None,
        };

        this.sync_initial_state(window, cx);
        this.load_devices(window, cx);
        this.subscribe_to_core_events(cx);
        this.subscribe_to_selects(cx);
        this.restart_level_monitor(cx);
        this.start_elapsed_ticker(cx);

        this
    }

    // ------------------------------------------------------------------
    // Startup
    // ------------------------------------------------------------------

    /// Read the current `RecordingSnapshot` (via `get_recording_state`) so
    /// the view is right if a recording is already running when it mounts.
    fn sync_initial_state(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let io = AppServices::global(cx).io.clone();
        cx.spawn(async move |this, cx| {
            let handle = io.spawn(async { recording_service::get_recording_state().await });
            let Ok(value) = handle.await else { return };
            let Ok(snapshot) = serde_json::from_value::<RecordingSnapshot>(value) else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.snapshot = snapshot.clone();
                this.elapsed.rebase(&snapshot);
                cx.notify();
            });
        })
        .detach();
    }

    fn load_devices(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let io = AppServices::global(cx).io.clone();
        let pool = AppServices::global(cx).pool();
        cx.spawn(async move |this, cx| {
            let handle = io.spawn(async move {
                let devices = devices::list_audio_devices().await.unwrap_or_default();
                let prefs = recording_preferences::load_recording_preferences(pool)
                    .await
                    .unwrap_or_else(|_| RecordingPreferences::default());
                (devices, prefs)
            });
            let Ok((devices, prefs)) = handle.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.apply_device_list(devices, &prefs, window, cx);
            });
        })
        .detach();
    }

    fn apply_device_list(
        &mut self,
        devices: Vec<parley_core::audio::pw::PwDevice>,
        prefs: &RecordingPreferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use parley_core::audio::pw::PwDeviceKind;

        let mut mic_items = vec![DeviceOption {
            id: DEFAULT_DEVICE_ID.into(),
            label: "Default Microphone".into(),
        }];
        let mut system_items = vec![DeviceOption {
            id: DEFAULT_DEVICE_ID.into(),
            label: "Default System Audio".into(),
        }];
        for device in devices {
            let option = DeviceOption {
                id: device.id.clone().into(),
                label: device.label.into(),
            };
            match device.kind {
                PwDeviceKind::Microphone => mic_items.push(option),
                PwDeviceKind::System => system_items.push(option),
            }
        }

        self.mic_select.update(cx, |select, cx| {
            select.set_items(mic_items, window, cx);
            if let Some(id) = &prefs.preferred_mic_device {
                select.set_selected_value(&id.clone().into(), window, cx);
            }
        });
        self.system_select.update(cx, |select, cx| {
            select.set_items(system_items, window, cx);
            if let Some(id) = &prefs.preferred_system_device {
                select.set_selected_value(&id.clone().into(), window, cx);
            }
        });
        self.restart_level_monitor(cx);
    }

    fn subscribe_to_selects(&mut self, cx: &mut Context<Self>) {
        cx.subscribe(&self.mic_select, |this, _, _event: &SelectEvent<Vec<DeviceOption>>, cx| {
            this.on_device_selection_changed(cx);
        })
        .detach();
        cx.subscribe(&self.system_select, |this, _, _event: &SelectEvent<Vec<DeviceOption>>, cx| {
            this.on_device_selection_changed(cx);
        })
        .detach();
    }

    fn on_device_selection_changed(&mut self, cx: &mut Context<Self>) {
        self.restart_level_monitor(cx);

        let mic_id = self.mic_selection(cx);
        let system_id = self.system_selection(cx);
        let pool = AppServices::global(cx).pool();
        let io = AppServices::global(cx).io.clone();
        cx.spawn(async move |_this, cx| {
            let handle = io.spawn(async move {
                let mut prefs = recording_preferences::load_recording_preferences(pool.clone())
                    .await
                    .unwrap_or_default();
                prefs.preferred_mic_device = (mic_id != DEFAULT_DEVICE_ID).then_some(mic_id);
                prefs.preferred_system_device =
                    (system_id != DEFAULT_DEVICE_ID).then_some(system_id);
                let _ = recording_preferences::save_recording_preferences(pool, &prefs).await;
            });
            let _ = handle.await;
            let _ = cx.update(|_| {});
        })
        .detach();
    }

    fn mic_selection(&self, cx: &App) -> String {
        self.mic_select
            .read(cx)
            .selected_value()
            .map(|v| v.to_string())
            .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string())
    }

    fn system_selection(&self, cx: &App) -> String {
        self.system_select
            .read(cx)
            .selected_value()
            .map(|v| v.to_string())
            .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string())
    }

    fn restart_level_monitor(&mut self, cx: &mut Context<Self>) {
        let services = AppServices::global(cx);
        let sink = services.sink.clone();
        let io = services.io.clone();
        let mic = self.mic_selection(cx);
        let system = self.system_selection(cx);
        io.spawn(async move {
            let _ = simple_level_monitor::start_monitoring(sink, Some(mic), Some(system)).await;
        });
    }

    fn start_elapsed_ticker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let result = this.update(cx, |this, cx| {
                    if this.elapsed.running {
                        cx.notify();
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // Core event handling
    // ------------------------------------------------------------------

    fn subscribe_to_core_events(&mut self, cx: &mut Context<Self>) {
        let core_events = AppServices::global(cx).core_events.clone();
        cx.subscribe(&core_events, |this, _, event: &CoreEvent, cx| {
            this.handle_core_event(event, cx);
        })
        .detach();
    }

    fn handle_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        match event.name.as_str() {
            "recording-state" => {
                if let Some(snapshot) = event.decode::<RecordingSnapshot>() {
                    self.apply_snapshot(snapshot, cx);
                }
            }
            "transcript-update" => {
                if let Some(update) = event.decode::<TranscriptUpdate>() {
                    self.apply_transcript_update(update, cx);
                }
            }
            "transcript-partial" => {
                if let Some(update) = event.decode::<PartialUpdatePayload>() {
                    if update.text.is_empty() {
                        self.partials.remove(&update.source);
                    } else {
                        self.partials.insert(update.source, update.text);
                    }
                    cx.notify();
                }
            }
            "live-action-items" => {
                if let Some(payload) = event.decode::<LiveActionItemsEvent>() {
                    let mut seen: std::collections::HashSet<String> = self
                        .live_action_items
                        .iter()
                        .map(|t| action_item_key(t))
                        .collect();
                    let mut added = false;
                    for item in payload.items {
                        let key = action_item_key(&item.text);
                        if seen.contains(&key) {
                            continue;
                        }
                        seen.insert(key);
                        self.live_action_items.push(item.text);
                        added = true;
                    }
                    if added {
                        cx.notify();
                    }
                }
            }
            "audio-levels" => {
                if let Some(update) =
                    event.decode::<parley_core::audio::simple_level_monitor::AudioLevelUpdate>()
                {
                    self.levels = logic::levels_from_update(&update.levels);
                    cx.notify();
                }
            }
            "recording-error" => {
                if let Some(message) = event.decode::<String>() {
                    self.toast(NotificationType::Error, message);
                }
            }
            "transcription-error" => {
                let message = event
                    .payload
                    .get("userMessage")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Transcription error".to_string());
                self.toast(NotificationType::Error, message);
            }
            "transcription-warning" => {
                if let Some(message) = event.decode::<String>() {
                    self.toast(NotificationType::Warning, message);
                }
            }
            "chunk-drop-warning" => {
                if let Some(message) = event.decode::<String>() {
                    self.toast(NotificationType::Warning, message);
                }
            }
            "recording-stopped" => {
                if let Some(payload) = event.decode::<RecordingStoppedPayload>() {
                    if let Some(meeting_id) = payload.meeting_id {
                        // Same background refine the Tauri frontend kicks off
                        // after its post-stop save (speaker refinement, then
                        // auto re-transcription if a better model exists).
                        if let Some(folder_path) = payload.folder_path {
                            let services = AppServices::global(cx);
                            services.io.spawn(recording_service::post_meeting_refine(
                                services.recording_context(),
                                meeting_id.clone(),
                                folder_path,
                            ));
                        }

                        // Link to the calendar event matched at start time
                        // (if any), now that a `meeting_id` exists. Mirrors
                        // `useRecordingStop.ts`'s `consumePendingCalendarEventId`
                        // + `linkMeetingToCalendarEvent` call — failure is
                        // non-fatal, matching React's try/catch.
                        if let Some(event_id) = self.pending_calendar_event_id.take() {
                            if let Some(pool) = AppServices::global(cx).pool() {
                                let meeting_id = meeting_id.clone();
                                AppServices::global(cx).io.spawn(async move {
                                    if let Err(e) = parley_core::calendar::service::link_meeting_with_snapshot(
                                        &pool,
                                        &meeting_id,
                                        Some(&event_id),
                                    )
                                    .await
                                    {
                                        log::warn!(
                                            "failed to link meeting {} to calendar event {}: {}",
                                            meeting_id,
                                            event_id,
                                            e
                                        );
                                    }
                                });
                            }
                        }

                        navigate(Route::Meeting(meeting_id), cx);
                    }
                }
            }
            "meeting-refining" => {
                self.refining = true;
                cx.notify();
            }
            "meeting-refined" | "meeting-refine-failed" => {
                self.refining = false;
                cx.notify();
            }
            _ => {}
        }
    }

    fn toast(&mut self, kind: NotificationType, message: String) {
        self.pending_toasts.push((kind, message));
    }

    fn apply_snapshot(&mut self, snapshot: RecordingSnapshot, cx: &mut Context<Self>) {
        let entering_starting = snapshot.phase == RecordingPhase::Starting
            && self.snapshot.phase != RecordingPhase::Starting;
        if entering_starting {
            self.clear_transcript(cx);
        }

        let was_active = is_active_phase(self.snapshot.phase);
        let now_active = is_active_phase(snapshot.phase);
        if !was_active && now_active {
            self.live_action_items.clear();
            self.restart_live_action_items(cx);
        } else if was_active && !now_active {
            live_action_items::stop();
        }

        self.elapsed.rebase(&snapshot);
        self.snapshot = snapshot;
        cx.notify();
    }

    /// Start the live action-item extractor: only when "Live action items"
    /// is on in Settings → Recording, and best-effort (a missing model
    /// config just means no live items).
    fn restart_live_action_items(&mut self, cx: &mut Context<Self>) {
        let services = AppServices::global(cx);
        let io = services.io.clone();
        let sink = services.sink.clone();
        let Some(pool) = services.pool() else { return };
        io.spawn(async move {
            let toggles = SettingsRepository::get_setting::<FeatureTogglesMini>(&pool, KEY_BETA_FEATURES)
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            if !toggles.live_action_items {
                return;
            }
            let Ok(Some(config)) = SettingsRepository::get_model_config(&pool).await else {
                return;
            };
            live_action_items::start(sink, pool, config.provider, config.model);
        });
    }

    fn clear_transcript(&mut self, cx: &mut Context<Self>) {
        let old_len = self.last_scroller_len;
        self.rows.clear();
        self.partials.clear();
        self.last_scroller_len = 0;
        self.scroller.update(cx, |s, cx| {
            s.splice(0..old_len, 0, cx);
        });
    }

    fn apply_transcript_update(&mut self, update: TranscriptUpdate, cx: &mut Context<Self>) {
        // The final segment supersedes any partial preview for this source.
        self.partials.remove(&update.source);

        let speaker = update.speaker.clone().unwrap_or_else(|| {
            if update.source == "mic" {
                "Me".to_string()
            } else {
                "Speaker".to_string()
            }
        });
        let row = TranscriptRow {
            sequence_id: update.sequence_id,
            audio_start_time: update.audio_start_time,
            speaker,
            text: update.text,
            source: update.source,
        };

        let old_len = self.rows.len();
        let change = logic::upsert_row(&mut self.rows, row);
        let new_len = self.rows.len();
        self.last_scroller_len = new_len;

        self.scroller.update(cx, |s, cx| match change {
            RowChange::Appended => {
                s.append(1, cx);
            }
            RowChange::InsertedAt(idx) => {
                s.splice(idx..idx, 1, cx);
            }
            RowChange::ReplacedAt(idx) => {
                s.splice(idx..idx + 1, 1, cx);
            }
            RowChange::Repositioned {
                removed_at,
                inserted_at,
            } => {
                s.splice(removed_at..removed_at + 1, 0, cx);
                s.splice(inserted_at..inserted_at, 1, cx);
            }
        });
        let _ = old_len;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Recording controls
    // ------------------------------------------------------------------

    fn on_start_click(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let services = AppServices::global(cx);
        let io = services.io.clone();
        let ctx = services.recording_context();
        let pool = services.pool();
        let typed_name = self.meeting_name.read(cx).value().to_string();
        let mic = self.mic_selection(cx);
        let system = self.system_selection(cx);

        cx.spawn(async move |this, cx| {
            // Find the calendar event happening right now (if any), mirroring
            // `recordingCalendarLink.ts`'s `prepareRecordingMetadata`: its
            // summary becomes the meeting title, and its id is stashed so
            // the `recording-stopped` handler can link the two once a
            // `meeting_id` exists.
            // On `Io`, like every other DB call: sqlx needs a tokio context,
            // and this closure runs on GPUI's executor.
            let matched_event = match pool {
                Some(pool) => io
                    .spawn(async move { CalendarRepository::find_event_for_now(&pool).await })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten(),
                None => None,
            };

            let (meeting_name, event_id) = match &matched_event {
                Some(event) => {
                    let summary = event.summary.clone().unwrap_or_default();
                    let title = if summary.trim().is_empty() { typed_name } else { summary };
                    (title, Some(event.id.clone()))
                }
                None => (typed_name, None),
            };

            let _ = this.update_in(cx, |this, window, cx| {
                this.pending_calendar_event_id = event_id;
                if let Some(event) = &matched_event {
                    this.meeting_name.update(cx, |state, cx| {
                        state.set_value(meeting_name.clone(), window, cx);
                    });
                    this.toast(
                        NotificationType::Info,
                        format!(
                            "Linked to \"{}\" from your calendar",
                            event.summary.clone().unwrap_or_else(|| meeting_name.clone())
                        ),
                    );
                }
            });

            let req = StartRequest {
                mic_device_name: Some(mic),
                system_device_name: Some(system),
                meeting_name: Some(meeting_name),
            };
            let handle = io.spawn(async move {
                let hooks = recording_service::default_start_hooks(ctx.pool.clone());
                recording_service::start(ctx, hooks, req).await
            });
            let result = handle.await;
            if let Ok(Err(e)) = result {
                let _ = this.update(cx, |this, _cx| {
                    this.toast(NotificationType::Error, format!("Failed to start recording: {e}"));
                });
            }
        })
        .detach();
    }

    fn on_pause_click(&mut self, cx: &mut Context<Self>) {
        let services = AppServices::global(cx);
        let io = services.io.clone();
        let ctx = services.recording_context();
        cx.spawn(async move |this, cx| {
            let handle = io.spawn(async move { recording_service::pause_recording(&ctx).await });
            if let Ok(Err(e)) = handle.await {
                let _ = this.update(cx, |this, _cx| {
                    this.toast(NotificationType::Error, format!("Failed to pause: {e}"));
                });
            }
        })
        .detach();
    }

    fn on_resume_click(&mut self, cx: &mut Context<Self>) {
        let services = AppServices::global(cx);
        let io = services.io.clone();
        let ctx = services.recording_context();
        cx.spawn(async move |this, cx| {
            let handle = io.spawn(async move { recording_service::resume_recording(&ctx).await });
            if let Ok(Err(e)) = handle.await {
                let _ = this.update(cx, |this, _cx| {
                    this.toast(NotificationType::Error, format!("Failed to resume: {e}"));
                });
            }
        })
        .detach();
    }

    fn on_stop_click(&mut self, cx: &mut Context<Self>) {
        let services = AppServices::global(cx);
        let io = services.io.clone();
        let ctx = services.recording_context();
        let save_path = parley_core::paths::app_data_dir()
            .map(|dir| logic::stop_save_path(&dir, Local::now()))
            .unwrap_or_else(|_| "recording.wav".to_string());

        cx.spawn(async move |this, cx| {
            let handle =
                io.spawn(async move { recording_service::stop(ctx, RecordingArgs { save_path }).await });
            if let Ok(Err(e)) = handle.await {
                let _ = this.update(cx, |this, _cx| {
                    this.toast(NotificationType::Error, format!("Failed to stop recording: {e}"));
                });
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // Rendering helpers
    // ------------------------------------------------------------------

    fn level_bar(&self, cx: &App, label: &'static str, level: f32) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap_2()
            .child(div().w_20().text_xs().text_color(cx.theme().muted_foreground).child(label))
            .child(
                div()
                    .flex_1()
                    .h_2()
                    .rounded_full()
                    .bg(hsla(0., 0., 0.5, 0.15))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .w(relative(level.clamp(0.0, 1.0)))
                            .bg(hsla(0.38 - 0.1 * level.clamp(0.0, 1.0), 0.65, 0.5, 1.)),
                    ),
            )
    }
}

fn idle_snapshot() -> RecordingSnapshot {
    RecordingSnapshot {
        phase: RecordingPhase::Idle,
        started_at_ms: None,
        active_duration_secs: None,
        total_pause_secs: 0.0,
        meeting_name: None,
        folder_path: None,
        meeting_id: None,
        chunks_in_queue: 0,
        error: None,
        seq: 0,
    }
}

impl Render for RecordingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for (kind, message) in self.pending_toasts.drain(..) {
            window.push_notification(Notification::new().message(message).with_type(kind), cx);
        }

        let buttons = logic::button_state(self.snapshot.phase);
        let elapsed = logic::format_elapsed(self.elapsed.displayed_secs().max(0.0) as u64);
        let mic_level = self.levels.mic;
        let system_level = self.levels.system;
        let this_entity = cx.entity();
        let partials: Vec<(String, String)> = self
            .partials
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        v_flex()
            .size_full()
            .gap_3()
            .p_4()
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(div().flex_1().child(Input::new(&self.meeting_name)))
                            .child(div().w_48().child(Select::new(&self.mic_select).disabled(!buttons.can_start)))
                            .child(div().w_48().child(Select::new(&self.system_select).disabled(!buttons.can_start))),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("start-recording")
                                    .primary()
                                    .icon(IconName::Play)
                                    .label("Start")
                                    .disabled(!buttons.can_start)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.on_start_click(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("pause-recording")
                                    .icon(IconName::Pause)
                                    .label("Pause")
                                    .disabled(!buttons.can_pause)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.on_pause_click(cx);
                                    })),
                            )
                            .child(
                                Button::new("resume-recording")
                                    .icon(IconName::Play)
                                    .label("Resume")
                                    .disabled(!buttons.can_resume)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.on_resume_click(cx);
                                    })),
                            )
                            .child(
                                Button::new("stop-recording")
                                    .danger()
                                    .label("Stop")
                                    .disabled(!buttons.can_stop)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.on_stop_click(cx);
                                    })),
                            )
                            .child(div().flex_1())
                            .child(Label::new(elapsed).text_color(cx.theme().muted_foreground))
                            .when(self.refining, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .italic()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Refining meeting…"),
                                )
                            }),
                    )
                    .child(self.level_bar(cx, "Mic", mic_level))
                    .child(self.level_bar(cx, "System", system_level)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        MessageScroller::new(
                            "live-transcript",
                            self.scroller.clone(),
                            move |ix, _window, cx| {
                                let entity = this_entity.read(cx);
                                let row = &entity.rows[ix];
                                let theme = ActiveTheme::theme(cx);
                                let accent = if row.source == "mic" {
                                    theme.primary
                                } else {
                                    theme.muted_foreground
                                };
                                v_flex()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_baseline()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_semibold()
                                                    .text_color(accent)
                                                    .child(row.speaker.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(logic::format_elapsed(
                                                        row.audio_start_time.max(0.0) as u64,
                                                    )),
                                            ),
                                    )
                                    .child(div().text_sm().child(row.text.clone()))
                            },
                        )
                        .with_bottom_fade(cx.theme().background),
                    ),
            )
            .when(!partials.is_empty(), |parent| {
                parent.child(v_flex().gap_1().px_1().children(partials.into_iter().map(
                    |(source, text)| {
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_xs()
                                    .italic()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if source == "mic" { "You (live)" } else { "System (live)" }),
                            )
                            .child(div().text_sm().italic().text_color(cx.theme().muted_foreground).child(text))
                    },
                )))
            })
            .when(!self.live_action_items.is_empty(), |parent| {
                parent.child(
                    v_flex()
                        .gap_1()
                        .p_3()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .text_xs()
                                .font_semibold()
                                .text_color(cx.theme().muted_foreground)
                                .child("Action items (live, beta)"),
                        )
                        .children(self.live_action_items.iter().map(|text| {
                            div().text_sm().child(format!("• {}", text))
                        })),
                )
            })
    }
}
