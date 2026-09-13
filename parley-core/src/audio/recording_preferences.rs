use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use crate::database::repositories::setting::{SettingsRepository, KEY_RECORDING_PREFERENCES};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordingPreferences {
    pub save_folder: PathBuf,
    pub auto_save: bool,
    pub file_format: String,
    #[serde(default)]
    pub preferred_mic_device: Option<String>,
    #[serde(default)]
    pub preferred_system_device: Option<String>,
    /// Show the "inform participants" toast when a recording starts.
    /// Previously its own tauri-plugin-store file (`preferences.json`,
    /// written by the frontend directly); folded in here so all recording
    /// settings live in one place.
    #[serde(default = "default_true")]
    pub show_recording_notification: bool,
    /// After a recording stops and its audio is finalized, automatically
    /// re-run it through a higher-accuracy Whisper model in the background
    /// and upgrade the stored transcript (see
    /// `audio::retranscription::spawn_auto_refine`). Best-effort and
    /// non-blocking; defaults to on since it never touches the live
    /// transcript unless the refine pass fully succeeds.
    #[serde(default = "default_true")]
    pub auto_refine: bool,
    /// Show streaming preview text while someone is still speaking (decoded
    /// from the in-progress utterance and replaced by the final transcript
    /// when the segment completes). Best-effort overlay; the committed
    /// transcript is unaffected when off. Defaults on.
    #[serde(default = "default_true")]
    pub streaming_partials: bool,
    /// Unload the Whisper model from memory after each recording stops
    /// instead of keeping it resident (see #47). Off by default: keeping
    /// the model loaded avoids paying the (multi-second, on some hardware
    /// much longer) reload cost at the start of the next recording. Turn
    /// this on to free the model's memory/VRAM between recordings instead,
    /// at the cost of a reload next time.
    #[serde(default)]
    pub unload_model_after_recording: bool,
    /// Opt-in to the accurate offline diarization pass on Import (pyannote
    /// segmentation + global clustering over the whole file, fed into the
    /// diarizer as clustering hints — see `speaker_diarization::offline` and
    /// `audio::import`). Attempted only when it's likely to matter (an
    /// explicit speaker count, or a long file) and only if this is on;
    /// still lazily downloads its ~5.7MB segmentation model on first use
    /// rather than at app startup. Defaults on: it's strictly a labelling
    /// improvement over the online clusterer, and falls back silently if
    /// the model can't be fetched.
    #[serde(default = "default_true")]
    pub offline_diarization_on_import: bool,
}

fn default_true() -> bool {
    true
}

impl Default for RecordingPreferences {
    fn default() -> Self {
        Self {
            save_folder: get_default_recordings_folder(),
            auto_save: true,
            file_format: "mp4".to_string(),
            preferred_mic_device: None,
            preferred_system_device: None,
            show_recording_notification: true,
            auto_refine: true,
            streaming_partials: true,
            unload_model_after_recording: false,
            offline_diarization_on_import: true,
        }
    }
}

/// Whether the Whisper model should be unloaded once a recording finishes,
/// per issue #47. Pure decision function (no I/O) so it's unit-testable
/// without a `RecordingPreferences` round-trip: defaults to `false` (keep
/// the model resident across recordings) unless the user has opted into
/// `unload_model_after_recording`.
pub fn should_unload_after_stop(preferences: &RecordingPreferences) -> bool {
    preferences.unload_model_after_recording
}

/// Get the default recordings folder (~/Documents/parley-recordings)
pub fn get_default_recordings_folder() -> PathBuf {
    dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("parley-recordings")
}

/// Ensure the recordings directory exists
pub fn ensure_recordings_directory(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
        info!("Created recordings directory: {:?}", path);
    }
    Ok(())
}

/// Generate a unique filename for a recording
pub fn generate_recording_filename(format: &str) -> String {
    let now = chrono::Utc::now();
    let timestamp = now.format("%Y%m%d_%H%M%S");
    format!("recording_{}.{}", timestamp, format)
}

/// One-time, read-only import of recording preferences from the legacy
/// tauri-plugin-store JSON file (`recording_preferences.json`), written
/// before this moved to SQLite. Read directly off disk (same `$APPDATA`
/// location and flat `{"preferences": ...}` shape the store plugin used,
/// with its default serializer) so this module no longer depends on the
/// plugin at all. The file is left in place afterward.
fn import_legacy_recording_preferences() -> Option<RecordingPreferences> {
    let path = crate::paths::app_data_dir().ok()?.join("recording_preferences.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    let value = root.get("preferences")?;
    match serde_json::from_value::<RecordingPreferences>(value.clone()) {
        Ok(prefs) => Some(prefs),
        Err(e) => {
            warn!("Failed to deserialize legacy recording preferences: {}", e);
            None
        }
    }
}

/// One-time, read-only import of the "show recording notification" toggle
/// from the legacy tauri-plugin-store JSON file (`preferences.json`) — a
/// *different* file from `recording_preferences.json`, written directly by
/// the frontend's JS-side `Store` API (`RecordingSettings.tsx` /
/// `recordingNotification.tsx`). Folded into `RecordingPreferences` on
/// import so it lives in the same SQLite row going forward. The file is
/// left in place afterward.
fn import_legacy_show_recording_notification() -> Option<bool> {
    let path = crate::paths::app_data_dir().ok()?.join("preferences.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    root.get("show_recording_notification")?.as_bool()
}

/// Load recording preferences from the database. `pool` is `None` when
/// `AppState` hasn't been managed yet (e.g. a first-launch cold start) — the
/// caller resolves it once (typically via `AppState`'s Tauri-managed pool)
/// and passes it in, so this module itself never depends on Tauri.
pub async fn load_recording_preferences(
    pool: Option<sqlx::SqlitePool>,
) -> Result<RecordingPreferences> {
    let pool = match pool {
        Some(pool) => pool,
        None => {
            info!("Database not yet initialized, using default recording preferences");
            return Ok(RecordingPreferences::default());
        }
    };

    let prefs = match SettingsRepository::get_setting::<RecordingPreferences>(
        &pool,
        KEY_RECORDING_PREFERENCES,
    )
    .await
    {
        Ok(Some(p)) => {
            info!("Loaded recording preferences from database");
            p
        }
        Ok(None) => {
            let base_imported = import_legacy_recording_preferences();
            let notification_imported = import_legacy_show_recording_notification();

            if base_imported.is_none() && notification_imported.is_none() {
                info!("No stored preferences found, using defaults");
                RecordingPreferences::default()
            } else {
                let mut imported = base_imported.unwrap_or_default();
                if let Some(show_notification) = notification_imported {
                    imported.show_recording_notification = show_notification;
                }
                info!("Importing legacy recording preferences (recording_preferences.json / preferences.json)");
                if let Err(e) =
                    SettingsRepository::set_setting(&pool, KEY_RECORDING_PREFERENCES, &imported)
                        .await
                {
                    warn!("Failed to persist imported recording preferences: {}", e);
                }
                imported
            }
        }
        Err(e) => {
            warn!(
                "Failed to load recording preferences: {}, using defaults",
                e
            );
            RecordingPreferences::default()
        }
    };

    info!("Loaded recording preferences: save_folder={:?}, auto_save={}, format={}, mic={:?}, system={:?}",
          prefs.save_folder, prefs.auto_save, prefs.file_format,
          prefs.preferred_mic_device, prefs.preferred_system_device);
    Ok(prefs)
}

/// Save recording preferences to the database. `pool` is resolved by the
/// caller (typically via `AppState`'s Tauri-managed pool), so this module
/// itself never depends on Tauri.
pub async fn save_recording_preferences(
    pool: Option<sqlx::SqlitePool>,
    preferences: &RecordingPreferences,
) -> Result<()> {
    info!("Saving recording preferences: save_folder={:?}, auto_save={}, format={}, mic={:?}, system={:?}",
          preferences.save_folder, preferences.auto_save, preferences.file_format,
          preferences.preferred_mic_device, preferences.preferred_system_device);

    let pool = pool.ok_or_else(|| anyhow!("Database not yet initialized — try again shortly"))?;
    SettingsRepository::set_setting(&pool, KEY_RECORDING_PREFERENCES, preferences)
        .await
        .map_err(|e| anyhow!("Failed to save recording preferences: {}", e))?;

    info!("Successfully persisted recording preferences to database");

    // Ensure the directory exists
    ensure_recordings_directory(&preferences.save_folder)?;

    Ok(())
}

#[cfg(test)]
mod unload_after_stop_tests {
    use super::{should_unload_after_stop, RecordingPreferences};

    #[test]
    fn defaults_to_keeping_model_loaded() {
        let prefs = RecordingPreferences::default();
        assert!(!prefs.unload_model_after_recording);
        assert!(!should_unload_after_stop(&prefs));
    }

    #[test]
    fn honors_opt_in_flag() {
        let mut prefs = RecordingPreferences::default();
        prefs.unload_model_after_recording = true;
        assert!(should_unload_after_stop(&prefs));
    }

    #[test]
    fn deserializes_missing_field_as_false() {
        // Old preferences JSON predating this field should not suddenly
        // start unloading the model.
        let json = serde_json::json!({
            "save_folder": "/tmp/x",
            "auto_save": true,
            "file_format": "mp4",
        });
        let prefs: RecordingPreferences = serde_json::from_value(json).unwrap();
        assert!(!prefs.unload_model_after_recording);
    }

    #[test]
    fn offline_diarization_on_import_defaults_to_true() {
        // Old preferences JSON predating this field should get the opt-in
        // default rather than silently disabling the feature.
        let json = serde_json::json!({
            "save_folder": "/tmp/x",
            "auto_save": true,
            "file_format": "mp4",
        });
        let prefs: RecordingPreferences = serde_json::from_value(json).unwrap();
        assert!(prefs.offline_diarization_on_import);
        assert!(RecordingPreferences::default().offline_diarization_on_import);
    }
}
