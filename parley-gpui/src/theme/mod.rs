//! Parley's own light and dark themes (`parley.json`): cool slate surfaces
//! and the logo's blue (#1A78E8) as the accent, in place of gpui-kit's
//! neutral shadcn defaults. Installed once at startup; the Appearance
//! setting still picks light / dark / system on top of it.

use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::App;

const THEMES: &str = include_str!("parley.json");
const DARK: &str = "Parley Dark";
const LIGHT: &str = "Parley Light";

/// Register the Parley themes and make them the light and dark themes.
/// Call right after `gpui_kit::init`.
pub fn install(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    if let Err(e) = registry.load_themes_from_str(THEMES) {
        log::error!("Failed to load the Parley theme, keeping the defaults: {e}");
        return;
    }
    let (Some(dark), Some(light)) = (
        registry.themes().get(DARK).cloned(),
        registry.themes().get(LIGHT).cloned(),
    ) else {
        log::error!("Parley theme file is missing \"{DARK}\" or \"{LIGHT}\"");
        return;
    };
    let theme = Theme::global_mut(cx);
    theme.dark_theme = dark;
    theme.light_theme = light;
    let mode = theme.mode;
    Theme::change(mode, None, cx);
}

#[cfg(test)]
mod tests {
    #[test]
    fn theme_file_parses_with_both_modes() {
        let value: serde_json::Value = serde_json::from_str(super::THEMES).unwrap();
        let names: Vec<&str> = value["themes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec![super::DARK, super::LIGHT]);
    }
}
