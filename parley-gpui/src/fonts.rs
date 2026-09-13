//! Match the desktop's monospace font.
//!
//! gpui-kit's Linux default is "DejaVu Sans Mono"; when that isn't installed
//! it probes for a stand-in and logs a warning. Rather than let it guess, ask
//! the desktop what monospace font the user actually chose, so code blocks
//! and transcripts match the rest of their system.
//!
//! Only applied when the family is really installed: GPUI panics on the first
//! line laid out in a family it can't find, so a stale setting must never
//! reach the theme.

use gpui_kit::App;
use gpui_kit::component::Theme;

/// Apply the desktop's configured monospace family to the theme, if we can
/// find one that's installed. No-op otherwise, leaving gpui-kit's own
/// fallback probe in charge.
pub fn apply_system_mono_font(cx: &mut App) {
    let Some(family) = system_mono_family() else {
        return;
    };

    // GPUI panics laying out text in a family it can't load, so only take the
    // desktop's answer if the font system actually knows it.
    let installed = cx
        .text_system()
        .all_font_names()
        .iter()
        .any(|name| name == &family);
    if !installed {
        log::debug!("System monospace font {family:?} isn't installed; keeping the default");
        return;
    }

    log::info!("Using the desktop's monospace font: {family}");
    cx.global_mut::<Theme>().mono_font_family = family.into();
}

/// The desktop's monospace family: the GNOME/GTK setting where present,
/// otherwise fontconfig's `monospace` alias. `None` if neither answers.
fn system_mono_family() -> Option<String> {
    gsettings_mono_family().or_else(fontconfig_mono_family)
}

/// `org.gnome.desktop.interface monospace-font-name` — a Pango description
/// like `'Adwaita Mono 11'`, i.e. quoted, with a trailing size to strip.
fn gsettings_mono_family() -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "monospace-font-name"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?;
    strip_pango_size(value.trim().trim_matches('\''))
}

/// `fc-match monospace family` — the portable answer for non-GNOME desktops.
fn fontconfig_mono_family() -> Option<String> {
    let out = std::process::Command::new("fc-match")
        .args(["monospace", "family"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?;
    // fc-match can return a comma-separated alias list ("Noto Sans Mono,Noto Sans Mono Regular").
    let family = value.trim().split(',').next()?.trim().to_string();
    (!family.is_empty()).then_some(family)
}

/// Drop a Pango trailing size ("Adwaita Mono 11" -> "Adwaita Mono"), keeping
/// families that merely end in a digit ("M+ 1m", "Go Mono 2").
fn strip_pango_size(desc: &str) -> Option<String> {
    let desc = desc.trim();
    if desc.is_empty() {
        return None;
    }
    let family = match desc.rsplit_once(' ') {
        // Only a bare integer is a size; "M+ 1m" keeps its last word.
        Some((head, tail)) if !head.is_empty() && tail.parse::<u32>().is_ok() => head.trim(),
        _ => desc,
    };
    (!family.is_empty()).then(|| family.to_string())
}

#[cfg(test)]
mod tests {
    use super::strip_pango_size;

    #[test]
    fn strips_a_trailing_point_size() {
        // Quotes are stripped by the caller, so this only sees bare
        // descriptions.
        assert_eq!(strip_pango_size("Adwaita Mono 11").as_deref(), Some("Adwaita Mono"));
        assert_eq!(
            strip_pango_size("Source Code Pro 10").as_deref(),
            Some("Source Code Pro")
        );
    }

    #[test]
    fn keeps_families_without_a_size() {
        assert_eq!(strip_pango_size("Hack").as_deref(), Some("Hack"));
        assert_eq!(strip_pango_size("JetBrains Mono").as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn keeps_a_family_whose_last_word_only_contains_a_digit() {
        assert_eq!(strip_pango_size("M+ 1m").as_deref(), Some("M+ 1m"));
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(strip_pango_size("   "), None);
    }
}
