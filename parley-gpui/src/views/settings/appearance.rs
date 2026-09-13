//! "Appearance" settings page: light/dark/system theme.
//!
//! `Theme::change` is gpui-kit's own in-memory switch. The chosen mode is
//! now also persisted under `KEY_THEME_PREFERENCE` (new — the React app's
//! theme lives in browser `localStorage`, outside SQLite, so this key has
//! no legacy predecessor) via `state::save_theme_pref`, so it survives a
//! restart. `init_theme` applies the saved value when the main window
//! opens and follows OS light/dark changes while set to "system".

use gpui_kit::component::{
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    IconName, Theme, ThemeMode,
};
use gpui_kit::*;

use parley_core::database::repositories::setting::{SettingsRepository, KEY_THEME_PREFERENCE};

use super::state::SettingsCache;
use crate::app_state::AppServices;
use crate::runtime::Io;

pub fn page(_view: &Entity<super::SettingsView>, _cx: &mut Context<super::SettingsView>) -> SettingPage {
    SettingPage::new("Appearance")
        .icon(IconName::Palette)
        .group(SettingGroup::new().title("Theme").items(vec![
            SettingItem::new(
                "Theme",
                SettingField::dropdown(
                    vec![
                        (SharedString::from("system"), SharedString::from("Follow system")),
                        (SharedString::from("light"), SharedString::from("Light")),
                        (SharedString::from("dark"), SharedString::from("Dark")),
                    ],
                    |cx: &App| {
                        let pref = SettingsCache::global(cx).theme_pref.clone();
                        SharedString::from(if pref.is_empty() { "system".to_string() } else { pref })
                    },
                    |val: SharedString, cx: &mut App| {
                        let mode_str = val.to_string();
                        apply_theme_mode(&mode_str, cx);
                        super::state::save_theme_pref(cx, mode_str);
                    },
                ),
            )
            .description("Applied immediately and remembered for next launch."),
        ]))
}

/// The preference currently in effect ("light" / "dark" / "system"), so an
/// OS appearance change only re-themes the app while following the system.
struct ActiveThemePref(String);

impl Global for ActiveThemePref {}

/// Switch the theme to `mode` ("light" / "dark" / "system").
fn apply_theme_mode(mode: &str, cx: &mut App) {
    cx.set_global(ActiveThemePref(mode.to_string()));
    match mode {
        "light" => Theme::change(ThemeMode::Light, None, cx),
        "dark" => Theme::change(ThemeMode::Dark, None, cx),
        _ => Theme::sync_system_appearance(None, cx),
    }
}

/// Startup theme setup for the main window: apply the preference saved under
/// `KEY_THEME_PREFERENCE` (read straight from the DB — `SettingsCache` loads
/// asynchronously and may not be ready yet; no DB or no saved value means
/// "system"), and keep following OS light/dark changes while the preference
/// is "system".
pub fn init_theme(window: &mut Window, cx: &mut App) {
    window
        .observe_window_appearance(|window, cx| {
            let following_system = cx
                .try_global::<ActiveThemePref>()
                .is_none_or(|pref| pref.0 == "system");
            if following_system {
                Theme::sync_system_appearance(Some(window), cx);
            }
        })
        .detach();

    Theme::sync_system_appearance(Some(window), cx);
    let Some(pool) = AppServices::global(cx).pool() else {
        return;
    };
    let io = Io::global(cx);
    cx.spawn(async move |cx| {
        let saved = io
            .spawn(async move {
                SettingsRepository::get_setting::<String>(&pool, KEY_THEME_PREFERENCE).await
            })
            .await;
        let pref = match saved {
            Ok(Ok(Some(pref))) if !pref.is_empty() => pref,
            _ => "system".to_string(),
        };
        cx.update(|cx| apply_theme_mode(&pref, cx));
    })
    .detach();
}
