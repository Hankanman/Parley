pub mod download;

use std::sync::Mutex as StdMutex;

pub fn format_timestamp(seconds: f64) -> String {
    let total_seconds = seconds as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let secs = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, secs)
}

// Global transcription language preference (default to "auto-translate" for
// automatic translation to English). Read by the transcription workers;
// written by the shell's `set_language_preference` Tauri command via
// `set_language_preference_internal`.
static LANGUAGE_PREFERENCE: std::sync::LazyLock<StdMutex<String>> =
    std::sync::LazyLock::new(|| StdMutex::new("auto-translate".to_string()));

/// Internal helper to get the current language preference (for use within
/// Rust code, not a Tauri command).
pub fn get_language_preference_internal() -> Option<String> {
    LANGUAGE_PREFERENCE.lock().ok().map(|lang| lang.clone())
}

/// Internal helper to set the current language preference. The shell's
/// `set_language_preference` command delegates to this.
pub fn set_language_preference_internal(language: String) {
    if let Ok(mut lang_pref) = LANGUAGE_PREFERENCE.lock() {
        *lang_pref = language;
    }
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
