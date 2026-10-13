pub mod download;

use std::sync::Mutex as StdMutex;

pub fn format_timestamp(seconds: f64) -> String {
    let total_seconds = seconds as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let secs = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, secs)
}

// Live transcription language, read by the transcription workers. Values:
// "auto" (detect), "auto-translate" (detect, translate to English) or an ISO
// code. Set from the Settings "Transcription language" field (`ui_config`
// `primaryLanguage`) at startup and whenever it changes — see
// `apply_language_preference`. Defaults to detection until then.
static LANGUAGE_PREFERENCE: std::sync::LazyLock<StdMutex<String>> =
    std::sync::LazyLock::new(|| StdMutex::new("auto".to_string()));

/// Get the current transcription language preference.
pub fn get_language_preference_internal() -> Option<String> {
    LANGUAGE_PREFERENCE.lock().ok().map(|lang| lang.clone())
}

/// Set the current transcription language preference.
pub fn set_language_preference_internal(language: String) {
    if let Ok(mut lang_pref) = LANGUAGE_PREFERENCE.lock() {
        *lang_pref = language;
    }
}

/// Map the Settings field to a Whisper language preference: blank means
/// detect; "auto"/"auto-translate" pass through; a 2–3 letter code is used
/// as-is (lower-cased). Anything else would make whisper.cpp reject every
/// segment ("unknown language"), so it falls back to detection.
pub fn language_preference_from_setting(primary_language: Option<&str>) -> String {
    let value = primary_language.unwrap_or("").trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "auto" => "auto".to_string(),
        "auto-translate" => value,
        code if (2..=3).contains(&code.len()) && code.chars().all(|c| c.is_ascii_lowercase()) => {
            value
        }
        _ => {
            log::warn!(
                "Ignoring transcription language {:?}; using auto-detect",
                value
            );
            "auto".to_string()
        }
    }
}

/// Apply a `primaryLanguage` value to live transcription.
pub fn apply_language_preference(primary_language: Option<&str>) {
    let language = language_preference_from_setting(primary_language);
    log::info!("Live transcription language: {}", language);
    set_language_preference_internal(language);
}

/// Read `ui_config.primaryLanguage` from the DB and apply it (startup).
pub async fn load_language_preference(pool: &sqlx::SqlitePool) {
    use crate::database::repositories::setting::{SettingsRepository, KEY_UI_CONFIG};
    let ui_config = SettingsRepository::get_setting::<serde_json::Value>(pool, KEY_UI_CONFIG)
        .await
        .ok()
        .flatten();
    let primary = ui_config
        .as_ref()
        .and_then(|v| v.get("primaryLanguage"))
        .and_then(|v| v.as_str());
    apply_language_preference(primary);
}

/// Plain HTTP-streaming download to a destination path. Used for built-in
/// models that don't have their own dedicated downloader command (also
/// reused by `speaker_diarization::service::ensure_pyannote_segmentation_model`-style
/// lazy, on-demand fetches). Cleans up partial files on error.
pub async fn download_file_to(url: &str, dest: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::anyhow;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let response = reqwest::get(url)
        .await
        .map_err(|e| anyhow!("HTTP error fetching {}: {}", url, e))?;
    if !response.status().is_success() {
        return Err(anyhow!("HTTP {} fetching {}", response.status(), url));
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| anyhow!("Cannot create {}: {}", dest.display(), e))?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("Download stream error: {}", e))?;
        if let Err(e) = file.write_all(&chunk).await {
            let _ = std::fs::remove_file(dest);
            return Err(anyhow!("Write error: {}", e));
        }
    }
    if let Err(e) = file.flush().await {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!("Flush error: {}", e));
    }
    Ok(())
}

#[cfg(test)]
mod language_preference_tests {
    use super::language_preference_from_setting as map;

    #[test]
    fn maps_settings_field_to_whisper_preference() {
        assert_eq!(map(None), "auto");
        assert_eq!(map(Some("  ")), "auto");
        assert_eq!(map(Some("en")), "en");
        assert_eq!(map(Some(" FR ")), "fr");
        assert_eq!(map(Some("yue")), "yue");
        assert_eq!(map(Some("auto-translate")), "auto-translate");
        assert_eq!(map(Some("English")), "auto");
        assert_eq!(map(Some("en-GB")), "auto");
    }
}
