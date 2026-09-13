//! Pure helpers for the Import Audio dialog: extension filtering, dropped-path
//! selection, formatting. Mirrors `frontend/src/components/ImportAudio/` and
//! `frontend/src/hooks/useImportAudio.ts` (duration/size formatting,
//! finding an audio file among dropped paths).

use std::path::{Path, PathBuf};

use parley_core::audio::constants::AUDIO_EXTENSIONS;

/// Whitelisted languages offered in the dialog (a trimmed mirror of
/// `frontend/src/constants/languages.ts` — full ISO 639-1 coverage isn't
/// needed for a first pass; `"auto"` matches the React default).
pub const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Auto Detect"),
    ("en", "English"),
    ("zh", "Chinese"),
    ("de", "German"),
    ("es", "Spanish"),
    ("ru", "Russian"),
    ("ko", "Korean"),
    ("fr", "French"),
    ("ja", "Japanese"),
    ("pt", "Portuguese"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("ar", "Arabic"),
    ("hi", "Hindi"),
];

/// True if `path`'s extension is one of the supported audio formats
/// (case-insensitive), mirroring `isAudioExtension` /
/// `validate_audio_file`'s own check in `parley-core`.
pub fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Given the paths from an external drag-drop, pick the first audio file —
/// mirrors `FileDropBridge.tsx`'s `paths.find(isAudioExtension)`.
pub fn pick_dropped_audio_file(paths: &[PathBuf]) -> Option<&PathBuf> {
    paths.iter().find(|p| is_audio_path(p))
}

/// `HH:MM:SS` (or `M:SS` under an hour) — mirrors `formatDuration` in
/// `ImportAudioDialog.tsx`.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// Human file size — mirrors `formatFileSize` in `ImportAudioDialog.tsx`.
pub fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes_f < MB {
        format!("{:.1} KB", bytes_f / KB)
    } else if bytes_f < GB {
        format!("{:.1} MB", bytes_f / MB)
    } else {
        format!("{:.1} GB", bytes_f / GB)
    }
}

/// Clamp a `0..=100` percentage into the range gpui-kit's `Progress::value`
/// expects (it clamps too, but this keeps the intent explicit and testable).
pub fn progress_percent(progress_percentage: u32) -> f32 {
    progress_percentage.min(100) as f32
}

/// Display name for a language code, falling back to the code itself for one
/// not in [`LANGUAGES`] (still forwarded to the backend as-is).
pub fn language_label(code: &str) -> String {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| code.to_string())
}

/// Speaker-count dropdown label — 0 means "auto-estimate".
pub fn speaker_count_label(count: i32) -> String {
    match count {
        0 => "Auto-detect".to_string(),
        1 => "1 speaker".to_string(),
        n => format!("{n} speakers"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_extensions_case_insensitive() {
        assert!(is_audio_path(Path::new("/tmp/meeting.WAV")));
        assert!(is_audio_path(Path::new("/tmp/meeting.mp3")));
        assert!(!is_audio_path(Path::new("/tmp/notes.txt")));
        assert!(!is_audio_path(Path::new("/tmp/no-extension")));
    }

    #[test]
    fn picks_first_audio_file_among_dropped_paths() {
        let paths = vec![
            PathBuf::from("/tmp/readme.txt"),
            PathBuf::from("/tmp/clip.mp4"),
            PathBuf::from("/tmp/other.wav"),
        ];
        assert_eq!(
            pick_dropped_audio_file(&paths),
            Some(&PathBuf::from("/tmp/clip.mp4"))
        );
    }

    #[test]
    fn no_audio_file_among_dropped_paths() {
        let paths = vec![PathBuf::from("/tmp/readme.txt")];
        assert_eq!(pick_dropped_audio_file(&paths), None);
    }

    #[test]
    fn formats_duration_under_an_hour() {
        assert_eq!(format_duration(65.0), "1:05");
        assert_eq!(format_duration(5.0), "0:05");
    }

    #[test]
    fn formats_duration_over_an_hour() {
        assert_eq!(format_duration(3725.0), "1:02:05");
    }

    #[test]
    fn formats_negative_duration_as_zero() {
        assert_eq!(format_duration(-5.0), "0:00");
    }

    #[test]
    fn formats_file_sizes() {
        assert_eq!(format_file_size(500), "500 B");
        assert_eq!(format_file_size(2048), "2.0 KB");
        assert_eq!(format_file_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_file_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn clamps_progress_percent() {
        assert_eq!(progress_percent(0), 0.0);
        assert_eq!(progress_percent(50), 50.0);
        assert_eq!(progress_percent(150), 100.0);
    }

    #[test]
    fn language_label_falls_back_to_code() {
        assert_eq!(language_label("en"), "English");
        assert_eq!(language_label("xx-unknown"), "xx-unknown");
    }

    #[test]
    fn speaker_count_labels() {
        assert_eq!(speaker_count_label(0), "Auto-detect");
        assert_eq!(speaker_count_label(1), "1 speaker");
        assert_eq!(speaker_count_label(3), "3 speakers");
    }
}
