//! Desktop notifications for the recording lifecycle, transcription
//! completion, system errors and meeting reminders, gated by the
//! notification settings stored in SQLite (`app_settings` under
//! `KEY_NOTIFICATION_SETTINGS`, via `SettingsRepository`).
//!
//! The full `NotificationSettings` shape lives in
//! `views/settings/state.rs`, which owns the settings page. Deserializing
//! only the handful of fields this needs (`serde` ignores the rest) reads
//! the same row without depending on the settings view. If no row exists
//! yet (notification settings never saved), this defaults to on, per the
//! phase 1 tray spec.
//!
//! The row's `consent_given` / `system_permission_granted` fields are
//! leftovers from the retired Tauri shell's permission prompt. Linux has no
//! such prompt and nothing sets them, so they are ignored here: gating on
//! them silenced every notification once the settings page saved its
//! (false) defaults.
//!
//! Also owns the meeting-reminder background loop and the GitHub-releases
//! update check (see [`start_meeting_reminder_loop`] and
//! [`check_for_updates`]).

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui_kit::App;
use serde::Deserialize;

use parley_core::calendar::repository::CalendarRepository;
use parley_core::database::repositories::setting::{
    SettingsRepository, KEY_NOTIFICATION_SETTINGS,
};

use crate::app_state::AppServices;

#[derive(Clone)]
pub enum Kind {
    Started,
    Stopped,
    Paused,
    Resumed,
    TranscriptionComplete,
    SystemError(String),
    MeetingReminder { minutes: u64, title: Option<String> },
    /// Auto-record (meeting auto-detection) started a recording. No
    /// per-kind toggle: the user opted in via Settings → Recording, and a
    /// recording starting unannounced would be a surprise.
    CallRecordingStarted { app_label: String, title: String },
    Test,
}

#[derive(Debug, Deserialize)]
struct SettingsMini {
    /// Master switch for the recording started/stopped/paused/resumed
    /// notifications (Settings → Notifications → "Recording notifications").
    #[serde(default = "default_true")]
    recording_notifications: bool,
    #[serde(default = "default_true")]
    notification_sound: bool,
    #[serde(default)]
    manual_dnd_mode: bool,
    #[serde(default)]
    notification_preferences: PreferencesMini,
}

#[derive(Debug, Deserialize)]
struct PreferencesMini {
    #[serde(default)]
    show_recording_started: bool,
    #[serde(default)]
    show_recording_stopped: bool,
    #[serde(default = "default_true")]
    show_recording_paused: bool,
    #[serde(default = "default_true")]
    show_recording_resumed: bool,
    #[serde(default = "default_true")]
    show_transcription_complete: bool,
    #[serde(default = "default_true")]
    show_meeting_reminders: bool,
    #[serde(default = "default_true")]
    show_system_errors: bool,
    #[serde(default = "default_reminder_minutes")]
    meeting_reminder_minutes: Vec<u64>,
}

impl Default for PreferencesMini {
    fn default() -> Self {
        // Matches `views::settings::state::NotificationPreferences::default()`.
        Self {
            show_recording_started: false,
            show_recording_stopped: false,
            show_recording_paused: true,
            show_recording_resumed: true,
            show_transcription_complete: true,
            show_meeting_reminders: true,
            show_system_errors: true,
            meeting_reminder_minutes: default_reminder_minutes(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_reminder_minutes() -> Vec<u64> {
    vec![15, 5]
}

impl Default for SettingsMini {
    fn default() -> Self {
        Self {
            recording_notifications: true,
            notification_sound: true,
            manual_dnd_mode: false,
            notification_preferences: PreferencesMini::default(),
        }
    }
}

// ============================================================================
// Pure gating logic (unit-tested)
// ============================================================================

/// Whether `kind` should be shown given `settings` — manual DND, the
/// recording-notifications master switch, and the per-event-type toggle.
/// The desktop's own Do Not Disturb is left to the notification server,
/// which already holds back popups while it is on. `Kind::Test` always
/// shows.
fn should_notify(settings: &SettingsMini, kind: &Kind) -> bool {
    if matches!(kind, Kind::Test) {
        return true;
    }
    if settings.manual_dnd_mode {
        return false;
    }
    let p = &settings.notification_preferences;
    let recording = settings.recording_notifications;
    match kind {
        Kind::Started => recording && p.show_recording_started,
        Kind::Stopped => recording && p.show_recording_stopped,
        Kind::Paused => recording && p.show_recording_paused,
        Kind::Resumed => recording && p.show_recording_resumed,
        Kind::TranscriptionComplete => p.show_transcription_complete,
        Kind::SystemError(_) => p.show_system_errors,
        Kind::MeetingReminder { .. } => p.show_meeting_reminders,
        Kind::CallRecordingStarted { .. } => true,
        Kind::Test => true,
    }
}

/// Title/body pair for a notification kind.
fn notification_text(kind: &Kind) -> (&'static str, String) {
    match kind {
        Kind::Started => (
            "Parley",
            "Recording has started. Please inform others in the meeting that you are recording."
                .to_string(),
        ),
        Kind::Stopped => ("Parley", "Recording has been stopped and saved".to_string()),
        Kind::Paused => ("Parley", "Recording has been paused".to_string()),
        Kind::Resumed => ("Parley", "Recording has been resumed".to_string()),
        Kind::TranscriptionComplete => {
            ("Parley", "Transcription has been completed".to_string())
        }
        Kind::SystemError(message) => ("Parley Error", message.clone()),
        Kind::MeetingReminder { minutes, title } => (
            "Parley",
            match title {
                Some(t) => format!("Meeting '{}' starts in {} minutes", t, minutes),
                None => format!("Meeting starts in {} minutes", minutes),
            },
        ),
        Kind::CallRecordingStarted { app_label, title } => (
            "Parley — recording",
            format!(
                "{app_label} detected, so Parley started recording \"{title}\". \
                 Please let others in the meeting know."
            ),
        ),
        Kind::Test => (
            "Parley",
            "This is a test notification to verify the system is working correctly".to_string(),
        ),
    }
}

// ============================================================================
// Meeting auto-detection prompt
// ============================================================================

/// What the user did with a "call detected — record?" notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPromptResponse {
    /// Clicked the "Record" action button.
    Record,
    /// Clicked the notification body (the `default` action).
    Open,
    /// Closed it, let it expire, or the server doesn't support actions.
    Dismissed,
}

/// How long the prompt stays up before the server expires it.
const CALL_PROMPT_TIMEOUT_MS: u32 = 60_000;

/// "Teams call detected — Record?" with a **Record** action button and a
/// clickable body, when the notification server advertises the `actions`
/// capability; otherwise a plain notification (the tray hint is then the
/// way to act on it). Gated by manual DND only — the per-feature opt-in is
/// the Recording page's auto-detect setting.
/// Resolves once the user acts on it (or it closes).
pub async fn prompt_call_detected(
    pool: Option<sqlx::SqlitePool>,
    app_label: String,
) -> CallPromptResponse {
    let settings = load_settings(pool).await;
    if settings.manual_dnd_mode {
        return CallPromptResponse::Dismissed;
    }
    let sound = settings.notification_sound;
    let result =
        tokio::task::spawn_blocking(move || show_call_prompt_blocking(&app_label, sound)).await;
    match result {
        Ok(response) => response,
        Err(e) => {
            log::warn!("call prompt notification task panicked: {}", e);
            CallPromptResponse::Dismissed
        }
    }
}

fn show_call_prompt_blocking(app_label: &str, sound: bool) -> CallPromptResponse {
    let supports_actions = notify_rust::get_capabilities()
        .map(|caps| caps.iter().any(|c| c == "actions"))
        .unwrap_or(false);

    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Parley")
        .summary(&format!("{app_label} detected — Record?"))
        .timeout(notify_rust::Timeout::Milliseconds(CALL_PROMPT_TIMEOUT_MS));
    apply_sound(&mut notification, sound);
    if supports_actions {
        notification
            .body("Record this call with Parley.")
            .action("default", "Open Parley")
            .action("record", "Record");
    } else {
        notification.body("Open Parley's tray menu to record this call.");
    }

    let handle = match notification.show() {
        Ok(handle) => handle,
        Err(e) => {
            log::warn!("failed to show call-detected notification: {}", e);
            return CallPromptResponse::Dismissed;
        }
    };
    if !supports_actions {
        return CallPromptResponse::Dismissed;
    }
    let mut response = CallPromptResponse::Dismissed;
    handle.wait_for_action(|action| {
        response = match action {
            "record" => CallPromptResponse::Record,
            "default" => CallPromptResponse::Open,
            _ => CallPromptResponse::Dismissed,
        };
    });
    response
}

/// Look up whether `kind` should be shown right now, and fire it
/// (best-effort, via `notify-rust`/D-Bus) if so. Runs entirely on the io
/// runtime; never blocks the GPUI thread.
pub fn maybe_notify(cx: &mut App, kind: Kind) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();

    io.spawn(async move {
        let settings = load_settings(pool).await;
        if !should_notify(&settings, &kind) {
            return;
        }
        let (title, body) = notification_text(&kind);
        show(title, body, settings.notification_sound).await;
    });
}

/// "Send test notification" button target (Notifications settings page).
/// Bypasses every gate except showing the notification itself — matches
/// `NotificationManager::show_test_notification`.
pub fn send_test_notification(cx: &mut App) {
    maybe_notify(cx, Kind::Test);
}

async fn load_settings(pool: Option<sqlx::SqlitePool>) -> SettingsMini {
    match pool {
        Some(pool) => {
            match SettingsRepository::get_setting::<SettingsMini>(&pool, KEY_NOTIFICATION_SETTINGS)
                .await
            {
                Ok(Some(settings)) => settings,
                Ok(None) => {
                    log::debug!("no stored notification settings; defaulting to on");
                    SettingsMini::default()
                }
                Err(e) => {
                    log::warn!("failed to read notification settings, defaulting to on: {}", e);
                    SettingsMini::default()
                }
            }
        }
        None => SettingsMini::default(),
    }
}

/// Ask the notification server for its standard "new message" sound, or for
/// silence ("Notification sound" in Settings → Notifications). Servers
/// without sound support ignore both hints.
fn apply_sound(notification: &mut notify_rust::Notification, sound: bool) {
    if sound {
        notification.sound_name("message-new-instant");
    } else {
        notification.hint(notify_rust::Hint::SuppressSound(true));
    }
}

/// Show a system notification via D-Bus, off the calling task (notify-rust
/// makes a blocking D-Bus call).
async fn show(title: &'static str, body: String, sound: bool) {
    let result = tokio::task::spawn_blocking(move || {
        let mut notification = notify_rust::Notification::new();
        notification.appname("Parley").summary(title).body(&body);
        apply_sound(&mut notification, sound);
        notification.show()
    })
    .await;
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => log::warn!("failed to show desktop notification: {}", e),
        Err(e) => log::warn!("notification task panicked: {}", e),
    }
}

// ============================================================================
// Meeting reminders
// ============================================================================

/// One upcoming calendar event, trimmed to what the reminder loop needs.
#[derive(Debug, Clone, PartialEq)]
struct UpcomingEvent {
    id: String,
    title: Option<String>,
    start_at: DateTime<Utc>,
}

/// Which of `events` are due a reminder right now, given the configured
/// `reminder_minutes` thresholds (e.g. `[15, 5]`) and the set of event ids
/// already reminded this session. An event fires **once** per session: the
/// largest configured threshold that the event has reached (i.e.
/// `minutes_until <= threshold`) is used, and the event id is then
/// considered done — it won't fire again for a smaller threshold later.
/// Pure, unit-tested; the caller is responsible for inserting the returned
/// ids into `already_notified` afterward.
fn due_reminders(
    events: &[UpcomingEvent],
    now: DateTime<Utc>,
    reminder_minutes: &[u64],
    already_notified: &HashSet<String>,
) -> Vec<(String, u64, Option<String>)> {
    if reminder_minutes.is_empty() {
        return Vec::new();
    }
    let mut thresholds: Vec<u64> = reminder_minutes.to_vec();
    thresholds.sort_unstable_by(|a, b| b.cmp(a)); // descending

    let mut due = Vec::new();
    for event in events {
        if already_notified.contains(&event.id) {
            continue;
        }
        let minutes_until = (event.start_at - now).num_seconds() as f64 / 60.0;
        if minutes_until < 0.0 {
            continue; // already started
        }
        if let Some(&threshold) = thresholds.iter().find(|&&t| minutes_until <= t as f64) {
            due.push((event.id.clone(), threshold, event.title.clone()));
        }
    }
    due
}

/// Background loop that reads upcoming calendar events every ~45s and fires
/// one reminder per event, `meeting_reminder_minutes` before its start,
/// de-duplicated per event id for the process lifetime. Call once, after
/// the main window opens.
///
/// Each reminder offers the event's pre-meeting brief (issue #84): an
/// "Open brief" action button when the notification server supports
/// actions, and clicking the notification itself opens it too. Either
/// raises Parley on the Brief page for that event.
pub fn start_meeting_reminder_loop(cx: &mut App) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let db = services.db.clone();

    cx.spawn(async move |cx| {
        let mut already_notified: HashSet<String> = HashSet::new();
        loop {
            let pool = db.read().unwrap().as_ref().map(|db| db.pool().clone());
            if let Some(pool) = pool {
                let tick = io
                    .spawn(async move {
                        let due = reminder_tick(pool, &mut already_notified).await;
                        (already_notified, due)
                    })
                    .await;
                let due = match tick {
                    Ok((notified, due)) => {
                        already_notified = notified;
                        due
                    }
                    Err(e) => {
                        log::warn!("meeting reminder tick panicked: {}", e);
                        already_notified = HashSet::new();
                        Vec::new()
                    }
                };
                for reminder in due {
                    let io = io.clone();
                    cx.spawn(async move |cx| {
                        let event_id = reminder.event_id.clone();
                        let response = io
                            .spawn(async move {
                                tokio::task::spawn_blocking(move || show_reminder_blocking(&reminder))
                                    .await
                                    .unwrap_or(ReminderResponse::Dismissed)
                            })
                            .await
                            .unwrap_or(ReminderResponse::Dismissed);
                        if response == ReminderResponse::OpenBrief {
                            let _ = cx.update(|cx| {
                                crate::tray::open_parley(cx);
                                crate::shell::navigate(crate::shell::Route::Brief(event_id), cx);
                            });
                        }
                    })
                    .detach();
                }
            }
            cx.background_executor().timer(Duration::from_secs(45)).await;
        }
    })
    .detach();
}

/// A reminder that's due now.
#[derive(Debug, Clone, PartialEq)]
struct DueReminder {
    event_id: String,
    minutes: u64,
    title: Option<String>,
    sound: bool,
}

/// What the user did with a meeting reminder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReminderResponse {
    /// "Open brief", or a click on the notification body.
    OpenBrief,
    Dismissed,
}

/// How long a reminder stays up (servers may ignore this).
const REMINDER_TIMEOUT_MS: u32 = 10 * 60 * 1000;

/// Show a reminder and, when the server supports actions, wait for the user
/// to act on it. Blocking (D-Bus): run on a blocking thread.
fn show_reminder_blocking(reminder: &DueReminder) -> ReminderResponse {
    let supports_actions = notify_rust::get_capabilities()
        .map(|caps| caps.iter().any(|c| c == "actions"))
        .unwrap_or(false);
    let (summary, body) = notification_text(&Kind::MeetingReminder {
        minutes: reminder.minutes,
        title: reminder.title.clone(),
    });

    let mut notification = notify_rust::Notification::new();
    notification.appname("Parley").summary(summary);
    apply_sound(&mut notification, reminder.sound);
    if supports_actions {
        notification
            .body(&format!("{body}. Open the brief to catch up on past meetings."))
            .timeout(notify_rust::Timeout::Milliseconds(REMINDER_TIMEOUT_MS))
            .action("default", "Open brief")
            .action("brief", "Open brief");
    } else {
        notification.body(&body);
    }
    let handle = match notification.show() {
        Ok(handle) => handle,
        Err(e) => {
            log::warn!("failed to show meeting reminder: {}", e);
            return ReminderResponse::Dismissed;
        }
    };
    if !supports_actions {
        return ReminderResponse::Dismissed;
    }
    let mut response = ReminderResponse::Dismissed;
    handle.wait_for_action(|action| {
        response = reminder_response(action);
    });
    response
}

fn reminder_response(action: &str) -> ReminderResponse {
    match action {
        "brief" | "default" => ReminderResponse::OpenBrief,
        _ => ReminderResponse::Dismissed,
    }
}

/// Real body of the reminder tick, taking the pool directly (used by
/// [`start_meeting_reminder_loop`], separated out so it's callable/testable
/// without a live `App`/`Io`). Returns the reminders to show now; their
/// event ids are added to `already_notified`.
async fn reminder_tick(pool: sqlx::SqlitePool, already_notified: &mut HashSet<String>) -> Vec<DueReminder> {
    let settings = load_settings(Some(pool.clone())).await;
    if settings.manual_dnd_mode || !settings.notification_preferences.show_meeting_reminders {
        return Vec::new();
    }
    let reminder_minutes = &settings.notification_preferences.meeting_reminder_minutes;
    if reminder_minutes.is_empty() {
        return Vec::new();
    }
    let max_minutes = *reminder_minutes.iter().max().unwrap_or(&0);

    let now = Utc::now();
    let horizon = now + chrono::Duration::minutes(max_minutes as i64 + 1);
    let events = match CalendarRepository::list_active_events_in_range(&pool, now, horizon).await {
        Ok(events) => events,
        Err(e) => {
            log::warn!("meeting reminder loop: failed to list upcoming events: {}", e);
            return Vec::new();
        }
    };
    let upcoming: Vec<UpcomingEvent> = events
        .into_iter()
        .filter(|e| !e.is_all_day && e.start_at > now)
        .map(|e| UpcomingEvent {
            id: e.id,
            title: e.summary,
            start_at: e.start_at,
        })
        .collect();

    let due = due_reminders(&upcoming, now, reminder_minutes, already_notified);
    due.into_iter()
        .map(|(event_id, minutes, title)| {
            already_notified.insert(event_id.clone());
            DueReminder { event_id, minutes, title, sound: settings.notification_sound }
        })
        .collect()
}

// ============================================================================
// Update check
// ============================================================================

const RELEASES_API: &str = "https://api.github.com/repos/Hankanman/Parley/releases/latest";

#[derive(Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    html_url: String,
}

/// Compare `latest_tag` (a GitHub release tag, e.g. `"v1.2.3"`) against
/// `current` (`env!("CARGO_PKG_VERSION")`, unprefixed) using semver
/// ordering. Pure, unit-tested; a malformed version on either side is a
/// friendly `Err` rather than a panic.
fn is_newer(current: &str, latest_tag: &str) -> Result<bool, String> {
    let latest = latest_tag.strip_prefix('v').unwrap_or(latest_tag);
    let current_v = semver::Version::parse(current)
        .map_err(|e| format!("couldn't parse the current version ({current}): {e}"))?;
    let latest_v = semver::Version::parse(latest)
        .map_err(|e| format!("couldn't parse the latest release version ({latest}): {e}"))?;
    Ok(latest_v > current_v)
}

/// Check GitHub for the latest release and show a notification: "Parley
/// X.Y.Z is available", "You're up to date", or a friendly error. No
/// auto-install.
pub fn check_for_updates(cx: &mut App) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();
    io.spawn(async move {
        let sound = load_settings(pool).await.notification_sound;
        let current = env!("CARGO_PKG_VERSION");
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("parley-gpui-update-check")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("update check: failed to build http client: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string(), sound).await;
                return;
            }
        };

        let response = match client.get(RELEASES_API).send().await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("update check: request failed: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string(), sound).await;
                return;
            }
        };

        let release: ReleaseResponse = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("update check: bad response: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string(), sound).await;
                return;
            }
        };

        match is_newer(current, &release.tag_name) {
            Ok(true) => {
                show(
                    "Parley",
                    format!(
                        "Parley {} is available — {}",
                        release.tag_name.trim_start_matches('v'),
                        release.html_url
                    ),
                    sound,
                )
                .await;
            }
            Ok(false) => {
                show("Parley", "You're up to date.".to_string(), sound).await;
            }
            Err(e) => {
                log::warn!("update check: version compare failed: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string(), sound).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_all_on() -> SettingsMini {
        SettingsMini {
            recording_notifications: true,
            notification_sound: true,
            manual_dnd_mode: false,
            notification_preferences: PreferencesMini {
                show_recording_started: true,
                show_recording_stopped: true,
                show_recording_paused: true,
                show_recording_resumed: true,
                show_transcription_complete: true,
                show_meeting_reminders: true,
                show_system_errors: true,
                meeting_reminder_minutes: vec![15, 5],
            },
        }
    }

    #[test]
    fn legacy_consent_fields_do_not_silence_notifications() {
        // Rows saved by the settings page carry `consent_given: false`
        // and `system_permission_granted: false`; they must not gate.
        let settings: SettingsMini = serde_json::from_str(
            r#"{"consent_given":false,"system_permission_granted":false,"manual_dnd_mode":false}"#,
        )
        .unwrap();
        assert!(should_notify(&settings, &Kind::Paused));
        assert!(should_notify(&settings, &Kind::MeetingReminder { minutes: 5, title: None }));
    }

    #[test]
    fn recording_master_switch_gates_only_recording_events() {
        let mut settings = settings_all_on();
        settings.recording_notifications = false;
        assert!(!should_notify(&settings, &Kind::Started));
        assert!(!should_notify(&settings, &Kind::Stopped));
        assert!(!should_notify(&settings, &Kind::Paused));
        assert!(!should_notify(&settings, &Kind::Resumed));
        assert!(should_notify(&settings, &Kind::TranscriptionComplete));
        assert!(should_notify(&settings, &Kind::SystemError("boom".into())));
    }

    #[test]
    fn manual_dnd_blocks_everything_except_test() {
        let mut settings = settings_all_on();
        settings.manual_dnd_mode = true;
        assert!(!should_notify(&settings, &Kind::SystemError("boom".into())));
        assert!(should_notify(&settings, &Kind::Test));
    }

    #[test]
    fn per_kind_toggle_gates_that_kind_only() {
        let mut settings = settings_all_on();
        settings.notification_preferences.show_recording_paused = false;
        assert!(!should_notify(&settings, &Kind::Paused));
        assert!(should_notify(&settings, &Kind::Resumed));
        assert!(should_notify(&settings, &Kind::Started));
    }

    #[test]
    fn auto_record_notice_ignores_per_kind_toggles_but_respects_dnd() {
        let kind = Kind::CallRecordingStarted {
            app_label: "Microsoft Teams call".into(),
            title: "Weekly sync".into(),
        };
        let mut settings = settings_all_on();
        settings.notification_preferences.show_recording_started = false;
        assert!(should_notify(&settings, &kind));
        settings.manual_dnd_mode = true;
        assert!(!should_notify(&settings, &kind));
    }

    #[test]
    fn defaults_match_the_settings_page_defaults() {
        let settings = SettingsMini::default();
        let p = &settings.notification_preferences;
        assert!(!p.show_recording_started);
        assert!(!p.show_recording_stopped);
        assert!(p.show_recording_paused);
        assert!(p.show_recording_resumed);
        assert!(p.show_transcription_complete);
        assert!(p.show_meeting_reminders);
        assert!(p.show_system_errors);
        assert_eq!(p.meeting_reminder_minutes, vec![15, 5]);
    }

    fn event(id: &str, minutes_from_now: i64, title: Option<&str>, now: DateTime<Utc>) -> UpcomingEvent {
        UpcomingEvent {
            id: id.to_string(),
            title: title.map(|s| s.to_string()),
            start_at: now + chrono::Duration::minutes(minutes_from_now),
        }
    }

    #[test]
    fn due_reminders_fires_the_largest_reached_threshold() {
        let now = Utc::now();
        let events = vec![event("e1", 12, Some("Standup"), now)];
        let due = due_reminders(&events, now, &[15, 5], &HashSet::new());
        assert_eq!(due, vec![("e1".to_string(), 15, Some("Standup".to_string()))]);
    }

    #[test]
    fn due_reminders_skips_events_that_already_started() {
        let now = Utc::now();
        let events = vec![event("e1", -1, None, now)];
        let due = due_reminders(&events, now, &[15, 5], &HashSet::new());
        assert!(due.is_empty());
    }

    #[test]
    fn due_reminders_skips_events_outside_every_threshold() {
        let now = Utc::now();
        let events = vec![event("e1", 30, None, now)];
        let due = due_reminders(&events, now, &[15, 5], &HashSet::new());
        assert!(due.is_empty());
    }

    #[test]
    fn due_reminders_dedupes_per_event_id() {
        let now = Utc::now();
        let events = vec![event("e1", 3, None, now)];
        let mut already = HashSet::new();
        already.insert("e1".to_string());
        let due = due_reminders(&events, now, &[15, 5], &already);
        assert!(due.is_empty());
    }

    #[test]
    fn reminder_actions_open_the_brief() {
        assert_eq!(reminder_response("brief"), ReminderResponse::OpenBrief);
        assert_eq!(reminder_response("default"), ReminderResponse::OpenBrief);
        assert_eq!(reminder_response("__closed"), ReminderResponse::Dismissed);
    }

    #[test]
    fn is_newer_detects_a_newer_release() {
        assert_eq!(is_newer("1.2.3", "v1.3.0"), Ok(true));
    }

    #[test]
    fn is_newer_false_when_up_to_date() {
        assert_eq!(is_newer("1.2.3", "v1.2.3"), Ok(false));
    }

    #[test]
    fn is_newer_false_when_current_is_ahead_of_a_stale_release() {
        assert_eq!(is_newer("2.0.0", "v1.9.9"), Ok(false));
    }

    #[test]
    fn is_newer_handles_an_unprefixed_tag() {
        assert_eq!(is_newer("1.0.0", "1.0.1"), Ok(true));
    }

    #[test]
    fn is_newer_errors_on_malformed_versions() {
        assert!(is_newer("1.0.0", "not-a-version").is_err());
    }
}
