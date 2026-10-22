//! The Recording page: device pickers, start/pause/resume/stop controls,
//! live level meters, and the live transcript. See the module doc on
//! `super` for how it's split.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, Utc};
use gpui_kit::component::{
    ActiveTheme, Icon, Sizable as _, Disableable as _, IconName, IndexPath, StyledExt as _, WindowExt, h_flex, v_flex,
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
use parley_core::bookmarks::{BookmarksChanged, BOOKMARKS_CHANGED_EVENT};
use parley_core::audio::recording_phase::{RecordingPhase, RecordingSnapshot};
use parley_core::audio::recording_preferences::{self, RecordingPreferences};
use parley_core::audio::recording_service::{self, RecordingArgs, StartRequest};
use parley_core::audio::simple_level_monitor;
use parley_core::audio::transcription::TranscriptUpdate;
use parley_core::calendar::{autolink, service as calendar_service};
use parley_core::database::repositories::setting::{
    SettingsRepository, KEY_BETA_FEATURES,
};
use parley_core::summary::live_action_items;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;
use crate::shell::{navigate, Route};

use super::logic::{self, Levels, RowChange, TranscriptRow};

const DEFAULT_DEVICE_ID: &str = "default";

type Lucide = gpui_kit::assets::IconName;

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

/// Where the level meters read from. Device streams hold the mic open, so
/// the meters only run while the audio panel is open on a visible page.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MeterSource {
    Off,
    /// Raw capture streams on the selected devices (no recording running).
    Devices { mic: String, system: String },
    /// The running recording's pipeline: the mic after echo cancellation,
    /// noise suppression and gain — what's actually transcribed.
    Pipeline,
}

fn desired_meter_source(on_screen: bool, panel_open: bool, phase: RecordingPhase, mic: String, system: String) -> MeterSource {
    if !on_screen || !panel_open {
        return MeterSource::Off;
    }
    match phase {
        RecordingPhase::Idle | RecordingPhase::Error => MeterSource::Devices { mic, system },
        RecordingPhase::Recording | RecordingPhase::Paused => MeterSource::Pipeline,
        // Mid start/stop: leave the devices to the recording.
        RecordingPhase::Starting | RecordingPhase::Stopping | RecordingPhase::Finalising => MeterSource::Off,
    }
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
    /// Ballistics-smoothed linear levels actually drawn (see `logic::meter_step`).
    displayed_levels: Levels,
    /// Time of the last meter animation step; `None` while the meters are settled.
    meter_tick: Option<std::time::Instant>,
    /// This page is the one on screen (see `on_show` / `on_leave`).
    on_screen: bool,
    /// The "Audio" panel (device pickers + level meters) is expanded.
    /// Closed by default: open meters hold the microphone, which desktop
    /// "mic in use" indicators report as Parley listening.
    audio_panel_open: bool,
    /// What the meters are currently reading from — see `sync_level_monitor`.
    meter_source: MeterSource,
    /// Meter switches, applied in order by one worker (`spawn_meter_worker`).
    meter_tx: tokio::sync::mpsc::UnboundedSender<MeterSource>,
    refining: bool,
    pending_toasts: Vec<(NotificationType, String)>,
    /// Provisional action items from the beta live extractor — see
    /// `restart_live_action_items`/`stop_live_action_items`.
    live_action_items: Vec<String>,
    /// Calendar event auto-linked when recording started, as
    /// `(meeting_id it was linked to, event_id)`. The meeting is linked
    /// right after start (`None` here when that couldn't happen yet); the
    /// `recording-stopped` handler then rewrites the snapshot into the final
    /// folder, unless the user changed the link in the meantime.
    pending_calendar_link: Option<(Option<String>, String)>,
    /// Offsets (seconds) bookmarked during this recording, newest last —
    /// from the button, the tray or the shortcut (issue #87).
    session_bookmarks: Vec<f64>,
}

/// Apply meter switches strictly in order. Spawning each switch as its own
/// task would let a quick open/close race, and a "start" landing after its
/// "stop" leaves the mic open with the panel shut. Only the newest pending
/// switch matters, so a backlog collapses to it. Ends when the view drops
/// its sender.
fn spawn_meter_worker(cx: &App) -> tokio::sync::mpsc::UnboundedSender<MeterSource> {
    let services = AppServices::global(cx);
    let sink = services.sink.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MeterSource>();
    services.io.spawn(async move {
        while let Some(mut source) = rx.recv().await {
            while let Ok(newer) = rx.try_recv() {
                source = newer;
            }
            let _ = match source {
                MeterSource::Off => simple_level_monitor::stop_monitoring().await,
                MeterSource::Devices { mic, system } => {
                    simple_level_monitor::start_monitoring(sink.clone(), Some(mic), Some(system)).await
                }
                MeterSource::Pipeline => simple_level_monitor::start_pipeline_monitoring(sink.clone()).await,
            };
        }
    });
    tx
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
            displayed_levels: Levels::default(),
            meter_tick: None,
            on_screen: true,
            audio_panel_open: false,
            meter_source: MeterSource::Off,
            meter_tx: spawn_meter_worker(cx),
            refining: false,
            pending_toasts: Vec::new(),
            live_action_items: Vec::new(),
            pending_calendar_link: None,
            session_bookmarks: Vec::new(),
        };

        this.sync_initial_state(window, cx);
        this.load_devices(window, cx);
        this.subscribe_to_core_events(cx);
        this.subscribe_to_selects(cx);
        this.start_elapsed_ticker(cx);
        // Closing to tray removes the window and drops this view, but the
        // monitor session lives in core — close its streams with the view.
        cx.on_release(|this, _cx| {
            if this.meter_source != MeterSource::Off {
                let _ = this.meter_tx.send(MeterSource::Off);
            }
        })
        .detach();

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
        self.sync_level_monitor(cx);
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
        self.sync_level_monitor(cx);

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

    /// The page came on screen.
    pub fn on_show(&mut self, cx: &mut Context<Self>) {
        self.on_screen = true;
        self.sync_level_monitor(cx);
    }

    /// The page went off screen: close the level meter's capture streams.
    pub fn on_leave(&mut self, cx: &mut Context<Self>) {
        self.on_screen = false;
        self.sync_level_monitor(cx);
    }

    fn toggle_audio_panel(&mut self, cx: &mut Context<Self>) {
        self.audio_panel_open = !self.audio_panel_open;
        self.sync_level_monitor(cx);
        cx.notify();
    }

    /// Point the meters at whatever they should read now (page visibility,
    /// panel state, recording phase, selected devices), starting or
    /// stopping the core monitor only when that changes.
    fn sync_level_monitor(&mut self, cx: &mut Context<Self>) {
        let desired = desired_meter_source(
            self.on_screen,
            self.audio_panel_open,
            self.snapshot.phase,
            self.mic_selection(cx),
            self.system_selection(cx),
        );
        if desired == self.meter_source {
            return;
        }
        self.levels = Levels::default();
        self.displayed_levels = Levels::default();
        self.meter_tick = None;
        self.meter_source = desired.clone();
        let _ = self.meter_tx.send(desired);
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
                if self.meter_source == MeterSource::Off {
                    return;
                }
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
                        // Background refine after the post-stop save (speaker
                        // refinement, then auto re-transcription if a better
                        // model exists).
                        if let Some(folder_path) = payload.folder_path {
                            let services = AppServices::global(cx);
                            services.io.spawn(recording_service::post_meeting_refine(
                                services.recording_context(),
                                meeting_id.clone(),
                                folder_path,
                            ));
                        }

                        // Finish the calendar auto-link made at start: rewrite
                        // the snapshot into the final folder if the meeting is
                        // still linked to that event (a manual unlink/relink
                        // in the meantime wins), or link now if it couldn't be
                        // linked at start. Failure is non-fatal.
                        if let Some((linked_meeting, event_id)) = self.pending_calendar_link.take() {
                            if let Some(pool) = AppServices::global(cx).pool() {
                                let meeting_id = meeting_id.clone();
                                AppServices::global(cx).io.spawn(async move {
                                    let result = match linked_meeting {
                                        Some(linked) if linked == meeting_id => {
                                            calendar_service::refresh_link_snapshot(&pool, &meeting_id, &event_id).await
                                        }
                                        Some(_) => Ok(false),
                                        None => calendar_service::auto_link_meeting(&pool, &meeting_id, &event_id).await,
                                    };
                                    if let Err(e) = result {
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
            name if name == BOOKMARKS_CHANGED_EVENT => {
                if let Some(BookmarksChanged { meeting_id, added: Some(bookmark) }) =
                    event.decode::<BookmarksChanged>()
                {
                    if self.snapshot.meeting_id.as_deref().is_none_or(|id| id == meeting_id) {
                        self.session_bookmarks.push(bookmark.offset_seconds);
                        self.toast(
                            NotificationType::Info,
                            format!(
                                "Bookmarked {}",
                                logic::format_elapsed(bookmark.offset_seconds.max(0.0) as u64)
                            ),
                        );
                        cx.notify();
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
            self.session_bookmarks.clear();
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
        self.sync_level_monitor(cx);
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
        let call = crate::meeting_detect::active_call(cx);

        cx.spawn(async move |this, cx| {
            // Pick the calendar event this recording belongs to (if any —
            // blockers like "Lunch", all-day, cancelled and solo events are
            // skipped; see `calendar::autolink`): its summary becomes the
            // meeting title, and the meeting is linked to it once started.
            // On `Io`, like every other DB call: sqlx needs a tokio context,
            // and this closure runs on GPUI's executor.
            let matched_event = match pool.clone() {
                Some(pool) => io
                    .spawn(async move { autolink::find_event_for_recording(&pool, Utc::now(), call).await })
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
                this.pending_calendar_link = event_id.clone().map(|id| (None, id));
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
                    this.pending_calendar_link = None;
                    this.toast(NotificationType::Error, format!("Failed to start recording: {e}"));
                });
                return;
            }

            // Link right away, so the meeting page shows the event (and the
            // user can change it) while recording, and a crash keeps it.
            if let (Some(pool), Some(event_id)) = (pool, event_id) {
                let linked = io
                    .spawn({
                        let event_id = event_id.clone();
                        async move {
                            let meeting_id = recording_service::current_meeting_id()?;
                            match calendar_service::auto_link_meeting(&pool, &meeting_id, &event_id).await {
                                Ok(true) => Some(meeting_id),
                                Ok(false) => None,
                                Err(e) => {
                                    log::warn!("failed to link meeting {meeting_id} to calendar event {event_id}: {e}");
                                    None
                                }
                            }
                        }
                    })
                    .await
                    .ok()
                    .flatten();
                if let Some(meeting_id) = linked {
                    let _ = this.update(cx, |this, _cx| {
                        if let Some((linked_meeting, pending_event)) = &mut this.pending_calendar_link {
                            if *pending_event == event_id {
                                *linked_meeting = Some(meeting_id);
                            }
                        }
                    });
                }
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
        let now = std::time::Instant::now();
        let dt = self
            .meter_tick
            .map(|t| now.duration_since(t).as_secs_f32().min(0.1))
            .unwrap_or(1.0 / 60.0);
        let (mic_disp, mic_moving) =
            logic::meter_step(self.displayed_levels.mic, self.levels.mic, dt);
        let (sys_disp, sys_moving) =
            logic::meter_step(self.displayed_levels.system, self.levels.system, dt);
        self.displayed_levels = Levels {
            mic: mic_disp,
            system: sys_disp,
        };
        if mic_moving || sys_moving {
            self.meter_tick = Some(now);
            window.request_animation_frame();
        } else {
            self.meter_tick = None;
        }
        let mic_level = logic::level_to_meter_fraction(mic_disp);
        let system_level = logic::level_to_meter_fraction(sys_disp);
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
                            .child(
                                Button::new("toggle-audio-panel")
                                    .ghost()
                                    .icon(if self.audio_panel_open { Lucide::ChevronDown } else { Lucide::ChevronRight })
                                    .label("Audio")
                                    .tooltip("Choose devices and check levels")
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_audio_panel(cx))),
                            ),
                    )
                    .when(self.audio_panel_open, |this| {
                        let caption = match self.meter_source {
                            MeterSource::Pipeline => {
                                "Mic level is after echo cancellation and noise suppression — what's being transcribed."
                            }
                            _ => "Raw input. Echo cancellation and noise suppression apply once recording starts.",
                        };
                        this.child(
                            v_flex()
                                .gap_2()
                                .p_2()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().muted.opacity(0.4))
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(div().w_20().text_xs().text_color(cx.theme().muted_foreground).child("Devices"))
                                        .child(div().flex_1().child(Select::new(&self.mic_select).disabled(!buttons.can_start)))
                                        .child(div().flex_1().child(Select::new(&self.system_select).disabled(!buttons.can_start))),
                                )
                                .child(self.level_bar(cx, "Mic", mic_level))
                                .child(self.level_bar(cx, "System", system_level))
                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(caption)),
                        )
                    })
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
                            .child(
                                Button::new("bookmark-moment")
                                    .icon(Lucide::BookmarkPlus)
                                    .label("Bookmark")
                                    .tooltip(format!(
                                        "Bookmark this moment ({})",
                                        crate::bookmark::SHORTCUT_LABEL
                                    ))
                                    .disabled(!buttons.can_stop)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        crate::bookmark::bookmark_now(cx);
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
                    .when(!self.session_bookmarks.is_empty(), |this| {
                        let marks = self
                            .session_bookmarks
                            .iter()
                            .map(|s| logic::format_elapsed(s.max(0.0) as u64))
                            .collect::<Vec<_>>()
                            .join(" · ");
                        this.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(Icon::from(Lucide::Bookmark).xsmall())
                                .child(format!("Bookmarks: {marks}")),
                        )
                    })
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

#[cfg(test)]
mod tests {
    use super::{desired_meter_source, MeterSource};
    use parley_core::audio::recording_phase::RecordingPhase;

    fn source(on_screen: bool, panel_open: bool, phase: RecordingPhase) -> MeterSource {
        desired_meter_source(on_screen, panel_open, phase, "default".into(), "default".into())
    }

    #[test]
    fn meters_stay_off_until_the_panel_is_opened() {
        assert_eq!(source(true, false, RecordingPhase::Idle), MeterSource::Off);
        assert_eq!(source(true, false, RecordingPhase::Recording), MeterSource::Off);
    }

    #[test]
    fn meters_stay_off_while_the_page_is_hidden() {
        assert_eq!(source(false, true, RecordingPhase::Idle), MeterSource::Off);
        assert_eq!(source(false, true, RecordingPhase::Recording), MeterSource::Off);
    }

    #[test]
    fn idle_meters_read_the_selected_devices() {
        assert_eq!(
            source(true, true, RecordingPhase::Idle),
            MeterSource::Devices { mic: "default".into(), system: "default".into() }
        );
    }

    #[test]
    fn recording_meters_read_the_pipeline_not_the_devices() {
        assert_eq!(source(true, true, RecordingPhase::Recording), MeterSource::Pipeline);
        assert_eq!(source(true, true, RecordingPhase::Paused), MeterSource::Pipeline);
    }

    #[test]
    fn meters_stand_aside_while_a_recording_starts_or_stops() {
        for phase in [RecordingPhase::Starting, RecordingPhase::Stopping, RecordingPhase::Finalising] {
            assert_eq!(source(true, true, phase), MeterSource::Off);
        }
    }
}
