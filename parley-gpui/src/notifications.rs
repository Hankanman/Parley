//! Desktop notifications for the recording lifecycle, transcription
//! completion, system errors and meeting reminders, gated by the same
//! notification settings the Tauri app stores (`notification_settings` in
//! SQLite, via `SettingsRepository`/`KEY_NOTIFICATION_SETTINGS` —
//! Tauri-free, already in `parley-core`).
//!
//! The full `NotificationSettings`/`NotificationPreferences` shape lives in
//! `frontend/src-tauri/src/notifications/settings.rs`, which — being built
//! around an `AppHandle`-based `ConsentManager` — isn't something this
//! shell can reuse directly without pulling in Tauri. Deserializing only
//! the handful of fields this needs (`serde` ignores the rest) reads the
//! same row without moving that module into core. If no row exists yet
//! (nobody has ever opened the Tauri app's notification settings), this
//! defaults to on, per the phase 1 tray spec.
//!
//! Also owns the meeting-reminder background loop and the GitHub-releases
//! update check — both dead code paths in the Tauri app (see module docs
//! on [`start_meeting_reminder_loop`] and [`check_for_updates`]).

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
    Test,
}

#[derive(Debug, Deserialize)]
struct SettingsMini {
    #[serde(default = "default_true")]
    consent_given: bool,
    #[serde(default = "default_true")]
    system_permission_granted: bool,
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
        // Matches the Tauri shell's `NotificationPreferences::default()`.
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
            consent_given: true,
            system_permission_granted: true,
            manual_dnd_mode: false,
            notification_preferences: PreferencesMini::default(),
        }
    }
}

// ============================================================================
// Pure gating logic (unit-tested)
// ============================================================================

/// Whether `kind` should be shown given `settings` — consent, system
/// permission, manual DND, and the per-event-type toggle, mirroring
/// `NotificationManager::should_show_notification` (minus the system DND
/// check, which needs a live D-Bus query and stays in [`maybe_notify`]).
/// `Kind::Test` always shows, matching the Tauri manager.
fn should_notify(settings: &SettingsMini, kind: &Kind) -> bool {
    if matches!(kind, Kind::Test) {
        return true;
    }
    if !settings.consent_given || !settings.system_permission_granted || settings.manual_dnd_mode {
        return false;
    }
    let p = &settings.notification_preferences;
    match kind {
        Kind::Started => p.show_recording_started,
        Kind::Stopped => p.show_recording_stopped,
        Kind::Paused => p.show_recording_paused,
        Kind::Resumed => p.show_recording_resumed,
        Kind::TranscriptionComplete => p.show_transcription_complete,
        Kind::SystemError(_) => p.show_system_errors,
        Kind::MeetingReminder { .. } => p.show_meeting_reminders,
        Kind::Test => true,
    }
}

/// Title/body pair for a notification kind, mirroring
/// `frontend/src-tauri/src/notifications/types.rs`'s `Notification::*`
/// constructors.
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
        Kind::Test => (
            "Parley",
            "This is a test notification to verify the system is working correctly".to_string(),
        ),
    }
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
        show(title, body).await;
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

/// Show a system notification via D-Bus, off the calling task (notify-rust
/// makes a blocking D-Bus call).
async fn show(title: &'static str, body: String) {
    let result = tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new().summary(title).body(&body).show()
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
/// de-duplicated per event id for the process lifetime. Dead in the Tauri
/// app (`show_meeting_reminder` is a command nothing ever calls) — this is
/// the real implementation. Call once, after the main window opens.
pub fn start_meeting_reminder_loop(cx: &mut App) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let db = services.db.clone();

    io.spawn(async move {
        let mut already_notified: HashSet<String> = HashSet::new();
        loop {
            let pool = db.read().unwrap().as_ref().map(|db| db.pool().clone());
            if let Some(pool) = pool {
                reminder_tick(pool, &mut already_notified).await;
            }
            tokio::time::sleep(Duration::from_secs(45)).await;
        }
    });
}

/// Real body of the reminder tick, taking the pool directly (used by
/// [`start_meeting_reminder_loop`], separated out so it's callable/testable
/// without a live `App`/`Io`).
async fn reminder_tick(pool: sqlx::SqlitePool, already_notified: &mut HashSet<String>) {
    let settings = load_settings(Some(pool.clone())).await;
    if !settings.notification_preferences.show_meeting_reminders {
        return;
    }
    let reminder_minutes = &settings.notification_preferences.meeting_reminder_minutes;
    if reminder_minutes.is_empty() {
        return;
    }
    let max_minutes = *reminder_minutes.iter().max().unwrap_or(&0);

    let now = Utc::now();
    let horizon = now + chrono::Duration::minutes(max_minutes as i64 + 1);
    let events = match CalendarRepository::list_events_in_range(&pool, now, horizon).await {
        Ok(events) => events,
        Err(e) => {
            log::warn!("meeting reminder loop: failed to list upcoming events: {}", e);
            return;
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
    for (id, minutes, title) in due {
        already_notified.insert(id);
        let (event_title, body) = notification_text(&Kind::MeetingReminder { minutes, title });
        show(event_title, body).await;
    }
}

// ============================================================================
// Update check
// ============================================================================

const RELEASES_API: &str = "https://api.github.com/repos/Hankanman/Meetily-Local/releases/latest";

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
/// X.Y.Z is available", "You're up to date", or a friendly error. Dead in
/// the Tauri app (the "Check for updates" tray item dispatches a JS event
/// nothing listens to) — this is the real implementation. No auto-install.
pub fn check_for_updates(cx: &mut App) {
    let io = AppServices::global(cx).io.clone();
    io.spawn(async move {
        let current = env!("CARGO_PKG_VERSION");
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("parley-gpui-update-check")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("update check: failed to build http client: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string()).await;
                return;
            }
        };

        let response = match client.get(RELEASES_API).send().await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("update check: request failed: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string()).await;
                return;
            }
        };

        let release: ReleaseResponse = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("update check: bad response: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string()).await;
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
                )
                .await;
            }
            Ok(false) => {
                show("Parley", "You're up to date.".to_string()).await;
            }
            Err(e) => {
                log::warn!("update check: version compare failed: {}", e);
                show("Parley", "Couldn't check for updates right now.".to_string()).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_all_on() -> SettingsMini {
        SettingsMini {
            consent_given: true,
            system_permission_granted: true,
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
    fn no_consent_blocks_everything_except_test() {
        let mut settings = settings_all_on();
        settings.consent_given = false;
        assert!(!should_notify(&settings, &Kind::Started));
        assert!(should_notify(&settings, &Kind::Test));
    }

    #[test]
    fn no_system_permission_blocks_everything_except_test() {
        let mut settings = settings_all_on();
        settings.system_permission_granted = false;
        assert!(!should_notify(&settings, &Kind::Stopped));
        assert!(should_notify(&settings, &Kind::Test));
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
    fn defaults_match_the_tauri_shell_defaults() {
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
