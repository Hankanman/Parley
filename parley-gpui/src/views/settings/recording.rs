//! "Recording" settings page: save folder, default devices, auto-save.
//! Reads/writes `recording_preferences` (`parley-core/src/audio/recording_preferences.rs`),
//! the same table the Tauri app's Recording settings tab uses.

use gpui_kit::component::{
    button::Button,
    h_flex,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    IconName,
};
use gpui_kit::*;

use super::state::SettingsCache;
use super::SettingsView;

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let view = view.clone();

    SettingPage::new("Recording")
        .icon(IconName::Folder)
        .default_open(true)
        .group(
            SettingGroup::new().title("Storage").item(
                SettingItem::render(|_options, _window, cx| {
                    let folder = SettingsCache::global(cx)
                        .recording
                        .save_folder
                        .to_string_lossy()
                        .to_string();
                    h_flex()
                        .w_full()
                        .justify_between()
                        .gap_3()
                        .child(folder.clone())
                        .child(Button::new("open-save-folder").outline().label("Open folder").on_click(
                            move |_, _, _cx| {
                                let _ = std::process::Command::new("xdg-open")
                                    .arg(&folder)
                                    .spawn();
                            },
                        ))
                        .into_any_element()
                })
                .keywords(["save folder", "recordings folder"]),
            ),
        )
        .group(
            SettingGroup::new().title("Devices").items(vec![
                SettingItem::new(
                    "Default microphone",
                    SettingField::dropdown(
                        device_options(cx, true),
                        |cx: &App| {
                            SharedString::from(
                                SettingsCache::global(cx)
                                    .recording
                                    .preferred_mic_device
                                    .clone()
                                    .unwrap_or_else(|| "default".to_string()),
                            )
                        },
                        {
                            let view = view.clone();
                            move |val: SharedString, cx: &mut App| {
                                set_recording(cx, &view, |prefs| {
                                    prefs.preferred_mic_device = Some(val.to_string());
                                });
                            }
                        },
                    ),
                )
                .description("Microphone used when starting a recording with default devices."),
                SettingItem::new(
                    "Default system audio",
                    SettingField::dropdown(
                        device_options(cx, false),
                        |cx: &App| {
                            SharedString::from(
                                SettingsCache::global(cx)
                                    .recording
                                    .preferred_system_device
                                    .clone()
                                    .unwrap_or_else(|| "default".to_string()),
                            )
                        },
                        {
                            let view = view.clone();
                            move |val: SharedString, cx: &mut App| {
                                set_recording(cx, &view, |prefs| {
                                    prefs.preferred_system_device = Some(val.to_string());
                                });
                            }
                        },
                    ),
                )
                .description("System audio source used when starting a recording with default devices."),
            ]),
        )
        .group(SettingGroup::new().title("General").items(vec![
            SettingItem::new(
                "Auto-save recordings",
                SettingField::switch(
                    |cx: &App| SettingsCache::global(cx).recording.auto_save,
                    {
                        let view = view.clone();
                        move |val: bool, cx: &mut App| {
                            set_recording(cx, &view, |prefs| prefs.auto_save = val);
                        }
                    },
                )
                .default_value(true),
            )
            .description("Automatically save the audio recording alongside its transcript."),
            SettingItem::new(
                "Live action items",
                SettingField::switch(
                    |cx: &App| SettingsCache::global(cx).features.live_action_items,
                    {
                        let view = view.clone();
                        move |val: bool, cx: &mut App| {
                            let mut features = SettingsCache::global(cx).features.clone();
                            features.live_action_items = val;
                            super::state::save_features(cx, &view, features);
                        }
                    },
                ),
            )
            .description("Pick out action items from the transcript while the meeting is still being recorded.")
            .keywords(["action items", "live"]),
        ]))
}

fn device_options(cx: &App, mic: bool) -> Vec<(SharedString, SharedString)> {
    let cache = SettingsCache::global(cx);
    let devices = if mic {
        &cache.mic_devices
    } else {
        &cache.system_devices
    };
    let mut options = vec![(SharedString::from("default"), SharedString::from("System default"))];
    options.extend(
        devices
            .iter()
            .map(|d| (SharedString::from(d.id.clone()), SharedString::from(d.label.clone()))),
    );
    options
}

/// Mutate the cached `RecordingPreferences`, persist the whole struct, and
/// redraw. `RecordingPreferences` is saved as one JSON blob (see
/// `recording_preferences::save_recording_preferences`), so every field
/// change round-trips the full struct rather than a single column.
fn set_recording(
    cx: &mut App,
    view: &Entity<SettingsView>,
    mutate: impl FnOnce(&mut parley_core::audio::recording_preferences::RecordingPreferences),
) {
    let updated = {
        let cache = cx.global_mut::<SettingsCache>();
        mutate(&mut cache.recording);
        cache.recording.clone()
    };
    super::state::save_recording(cx, updated);
    let _ = view.update(cx, |_, cx| cx.notify());
}
