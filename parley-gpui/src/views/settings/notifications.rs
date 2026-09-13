//! "Notifications" settings page: enable/disable and which events.
//!
//! Reads/writes the same `app_settings` row (`KEY_NOTIFICATION_SETTINGS`)
//! as the React app's notification settings and
//! `frontend/src-tauri/src/notifications/settings.rs`'s
//! `NotificationSettings`/`NotificationPreferences` — see `state.rs`'s
//! mirrored struct doc comment for why it's a local copy rather than a
//! shared type. `parley-gpui/src/notifications.rs` (tray/DBus
//! notifications) already reads a subset of this exact row, so this page
//! must keep the JSON field names identical.

use gpui_kit::component::{
    button::Button,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    IconName,
};
use gpui_kit::*;

use super::state::{NotificationPreferences, NotificationSettings, SettingsCache};
use super::SettingsView;
use crate::notifications;

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let _ = cx;
    let view = view.clone();

    SettingPage::new("Notifications")
        .icon(IconName::Bell)
        .group(
            SettingGroup::new().title("General").items(vec![
                toggle(
                    &view,
                    "Recording notifications",
                    "Notify when a recording starts, stops, pauses, or resumes.",
                    |s| s.recording_notifications,
                    |s, v| s.recording_notifications = v,
                ),
                toggle(
                    &view,
                    "Time-based reminders",
                    "Remind about an in-progress meeting after a set duration.",
                    |s| s.time_based_reminders,
                    |s, v| s.time_based_reminders = v,
                ),
                toggle(
                    &view,
                    "Meeting reminders",
                    "Remind before a calendar-linked meeting starts.",
                    |s| s.meeting_reminders,
                    |s, v| s.meeting_reminders = v,
                ),
                toggle(
                    &view,
                    "Respect Do Not Disturb",
                    "Suppress notifications while the system DND mode is on.",
                    |s| s.respect_do_not_disturb,
                    |s, v| s.respect_do_not_disturb = v,
                ),
                toggle(
                    &view,
                    "Notification sound",
                    "Play a sound with each notification.",
                    |s| s.notification_sound,
                    |s, v| s.notification_sound = v,
                ),
                toggle(
                    &view,
                    "Manual Do Not Disturb",
                    "Silence all notifications regardless of the system DND state.",
                    |s| s.manual_dnd_mode,
                    |s, v| s.manual_dnd_mode = v,
                ),
            ]),
        )
        .group(
            SettingGroup::new().title("Events").items(vec![
                pref_toggle(
                    &view,
                    "Recording started",
                    |p| p.show_recording_started,
                    |p, v| p.show_recording_started = v,
                ),
                pref_toggle(
                    &view,
                    "Recording stopped",
                    |p| p.show_recording_stopped,
                    |p, v| p.show_recording_stopped = v,
                ),
                pref_toggle(
                    &view,
                    "Recording paused",
                    |p| p.show_recording_paused,
                    |p, v| p.show_recording_paused = v,
                ),
                pref_toggle(
                    &view,
                    "Recording resumed",
                    |p| p.show_recording_resumed,
                    |p, v| p.show_recording_resumed = v,
                ),
                pref_toggle(
                    &view,
                    "Transcription complete",
                    |p| p.show_transcription_complete,
                    |p, v| p.show_transcription_complete = v,
                ),
                pref_toggle(
                    &view,
                    "Meeting reminders",
                    |p| p.show_meeting_reminders,
                    |p, v| p.show_meeting_reminders = v,
                ),
                pref_toggle(
                    &view,
                    "System errors",
                    |p| p.show_system_errors,
                    |p, v| p.show_system_errors = v,
                ),
            ]),
        )
        .group(
            SettingGroup::new().title("Test").items(vec![SettingItem::render(
                |_options, _window, _cx| {
                    Button::new("send-test-notification")
                        .outline()
                        .label("Send test notification")
                        .on_click(|_, _, cx| notifications::send_test_notification(cx))
                        .into_any_element()
                },
            )]),
        )
}

fn toggle(
    view: &Entity<SettingsView>,
    title: &'static str,
    description: &'static str,
    get: fn(&NotificationSettings) -> bool,
    set: fn(&mut NotificationSettings, bool),
) -> SettingItem {
    let view = view.clone();
    SettingItem::new(
        title,
        SettingField::switch(
            move |cx: &App| get(&SettingsCache::global(cx).notifications),
            move |val: bool, cx: &mut App| {
                let mut settings = SettingsCache::global(cx).notifications.clone();
                set(&mut settings, val);
                super::state::save_notifications(cx, &view, settings);
            },
        ),
    )
    .description(description)
}

fn pref_toggle(
    view: &Entity<SettingsView>,
    title: &'static str,
    get: fn(&NotificationPreferences) -> bool,
    set: fn(&mut NotificationPreferences, bool),
) -> SettingItem {
    let view = view.clone();
    SettingItem::new(
        title,
        SettingField::switch(
            move |cx: &App| get(&SettingsCache::global(cx).notifications.notification_preferences),
            move |val: bool, cx: &mut App| {
                let mut settings = SettingsCache::global(cx).notifications.clone();
                set(&mut settings.notification_preferences, val);
                super::state::save_notifications(cx, &view, settings);
            },
        ),
    )
}
