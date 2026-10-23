//! "Notifications" settings page: enable/disable and which events.
//!
//! The row also holds `time_based_reminders`, `meeting_reminders` and
//! `respect_do_not_disturb` from the retired Tauri shell. They have no
//! toggle here: the first was never implemented, the second duplicated the
//! "Meeting reminders" event toggle, and the desktop's notification server
//! already holds popups back during its own Do Not Disturb.
//!
//! Reads/writes the `app_settings` row under `KEY_NOTIFICATION_SETTINGS`
//! through `state.rs`'s `NotificationSettings`.
//! `parley-gpui/src/notifications.rs` (tray/DBus notifications) reads a
//! subset of this exact row, so the JSON field names must stay identical.

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
                    "Allow the recording started, stopped, paused and resumed notifications chosen below.",
                    |s| s.recording_notifications,
                    |s, v| s.recording_notifications = v,
                ),
                toggle(
                    &view,
                    "Notification sound",
                    "Ask the desktop to play its message sound with each notification.",
                    |s| s.notification_sound,
                    |s, v| s.notification_sound = v,
                ),
                toggle(
                    &view,
                    "Manual Do Not Disturb",
                    "Silence all of Parley's notifications.",
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
