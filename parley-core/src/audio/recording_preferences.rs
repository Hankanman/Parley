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

/// Name of the default recordings folder before the Parley rename.
const LEGACY_RECORDINGS_FOLDER_NAME: &str = "meetily-recordings";

/// Move `~/Documents/meetily-recordings` to `~/Documents/parley-recordings`,
/// once, and point everything stored in the database at the new location.
/// Call once the database is open; logs rather than fails.
pub async fn migrate_legacy_recordings_folder(pool: &sqlx::SqlitePool) {
    let Some(documents) = dirs::document_dir() else {
        return;
    };
    match migrate_legacy_recordings_folder_in(pool, &documents).await {
        Ok(Some((from, to))) => info!(
            "Moved recordings from {} to {}",
            from.display(),
            to.display()
        ),
        Ok(None) => {}
        Err(e) => warn!("Recordings folder migration didn't complete: {e}"),
    }
}

/// [`migrate_legacy_recordings_folder`] against an explicit documents dir,
/// for tests. Returns the move it made, if any.
///
/// Meeting rows store absolute paths (`meetings.folder_path` / `audio_path`)
/// and the recording preferences store `save_folder`, so the folder move is
/// followed by rewriting those — a transaction that only touches paths
/// under exactly the legacy folder. A symlink is left at the old path, so
/// even if the rewrite fails (and is retried on the next launch) every
/// stored path still resolves.
pub async fn migrate_legacy_recordings_folder_in(
    pool: &sqlx::SqlitePool,
    documents: &std::path::Path,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let legacy = documents.join(LEGACY_RECORDINGS_FOLDER_NAME);
    let current = documents.join("parley-recordings");

    let legacy_meta = std::fs::symlink_metadata(&legacy).ok();
    let legacy_is_real_dir = legacy_meta.as_ref().is_some_and(|m| m.is_dir());
    let legacy_is_symlink = legacy_meta.as_ref().is_some_and(|m| m.file_type().is_symlink());

    let moved = if legacy_is_real_dir {
        if current.exists() {
            // Both exist: stored paths into the legacy folder still work, so
            // leave everything alone rather than guess how to merge.
            warn!(
                "Both {} and {} exist; leaving recordings where they are",
                legacy.display(),
                current.display()
            );
            return Ok(None);
        }
        std::fs::rename(&legacy, &current)?;
        if let Err(e) = std::os::unix::fs::symlink("parley-recordings", &legacy) {
            warn!("Couldn't leave a symlink at {}: {e}", legacy.display());
        }
        true
    } else if legacy_is_symlink && current.is_dir() {
        // Moved on an earlier launch; make sure the rewrite below finished.
        false
    } else {
        return Ok(None);
    };

    let legacy_str = legacy
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 recordings path {}", legacy.display()))?;
    let current_str = current
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 recordings path {}", current.display()))?;
    rewrite_recording_paths(pool, legacy_str, current_str).await?;

    Ok(moved.then_some((legacy, current)))
}

/// Replace the `from` folder prefix with `to` in every stored recording path.
/// Matches the folder itself or paths strictly inside it, never a sibling
/// that merely shares the prefix (`meetily-recordings-old`).
async fn rewrite_recording_paths(pool: &sqlx::SqlitePool, from: &str, to: &str) -> Result<()> {
    // SQLite's substr/length count characters, not bytes.
    let from_len = from.chars().count() as i64;
    let mut tx = pool.begin().await?;

    for column in ["folder_path", "audio_path"] {
        let sql = format!(
            "UPDATE meetings SET {column} = ?1 || substr({column}, ?2 + 1) \
             WHERE substr({column}, 1, ?2) = ?3 \
               AND (length({column}) = ?2 OR substr({column}, ?2 + 1, 1) = '/')"
        );
        sqlx::query(&sql)
            .bind(to)
            .bind(from_len)
            .bind(from)
            .execute(&mut *tx)
            .await?;
    }

    // Edit the stored JSON as a value so fields this build doesn't know
    // about survive the round trip.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = ?1")
            .bind(KEY_RECORDING_PREFERENCES)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(json) = stored {
        let mut value: serde_json::Value = serde_json::from_str(&json)?;
        let rewritten = value
            .get("save_folder")
            .and_then(|v| v.as_str())
            .and_then(|folder| {
                let rest = folder.strip_prefix(from)?;
                (rest.is_empty() || rest.starts_with('/')).then(|| format!("{to}{rest}"))
            });
        if let Some(folder) = rewritten {
            value["save_folder"] = serde_json::Value::String(folder);
            sqlx::query("UPDATE app_settings SET value = ?1 WHERE key = ?2")
                .bind(serde_json::to_string(&value)?)
                .bind(KEY_RECORDING_PREFERENCES)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(())
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
mod legacy_recordings_folder_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn insert_meeting(pool: &SqlitePool, id: &str, folder: &str, audio: Option<&str>) {
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path, audio_path) \
             VALUES (?1, 'm', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', ?2, ?3)",
        )
        .bind(id)
        .bind(folder)
        .bind(audio)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn paths_of(pool: &SqlitePool, id: &str) -> (String, Option<String>) {
        sqlx::query_as("SELECT folder_path, audio_path FROM meetings WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn moves_the_folder_and_rewrites_stored_paths() {
        let pool = test_pool().await;
        let docs = tempfile::tempdir().unwrap();
        let legacy = docs.path().join("meetily-recordings");
        std::fs::create_dir_all(legacy.join("Standup")).unwrap();
        std::fs::write(legacy.join("Standup/audio.mp4"), b"audio").unwrap();
        let (legacy_s, docs_s) = (legacy.to_str().unwrap(), docs.path().to_str().unwrap());

        insert_meeting(
            &pool,
            "moved",
            &format!("{legacy_s}/Standup"),
            Some(&format!("{legacy_s}/Standup/audio.mp4")),
        )
        .await;
        // A sibling sharing the prefix, and a folder elsewhere: both untouched.
        insert_meeting(&pool, "sibling", &format!("{legacy_s}-old/x"), None).await;
        insert_meeting(&pool, "elsewhere", "/mnt/archive/y", None).await;
        let prefs = serde_json::json!({
            "save_folder": legacy_s, "auto_save": true, "file_format": "mp4",
            "some_future_field": 42
        });
        SettingsRepository::set_setting(&pool, KEY_RECORDING_PREFERENCES, &prefs)
            .await
            .unwrap();

        let current = docs.path().join("parley-recordings");
        let moved = migrate_legacy_recordings_folder_in(&pool, docs.path())
            .await
            .unwrap();
        assert_eq!(moved, Some((legacy.clone(), current.clone())));

        assert_eq!(std::fs::read(current.join("Standup/audio.mp4")).unwrap(), b"audio");
        assert!(std::fs::symlink_metadata(&legacy).unwrap().file_type().is_symlink());
        assert_eq!(
            paths_of(&pool, "moved").await,
            (
                format!("{docs_s}/parley-recordings/Standup"),
                Some(format!("{docs_s}/parley-recordings/Standup/audio.mp4"))
            )
        );
        assert_eq!(paths_of(&pool, "sibling").await.0, format!("{legacy_s}-old/x"));
        assert_eq!(paths_of(&pool, "elsewhere").await.0, "/mnt/archive/y");

        let stored: serde_json::Value =
            SettingsRepository::get_setting(&pool, KEY_RECORDING_PREFERENCES)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(stored["save_folder"], current.to_str().unwrap());
        assert_eq!(stored["some_future_field"], 42);

        // Second launch: nothing left to do, nothing changes.
        assert_eq!(
            migrate_legacy_recordings_folder_in(&pool, docs.path()).await.unwrap(),
            None
        );
        assert_eq!(
            paths_of(&pool, "moved").await.0,
            format!("{docs_s}/parley-recordings/Standup")
        );
    }

    #[tokio::test]
    async fn leaves_everything_alone_when_both_folders_exist() {
        let pool = test_pool().await;
        let docs = tempfile::tempdir().unwrap();
        let legacy = docs.path().join("meetily-recordings");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::create_dir(docs.path().join("parley-recordings")).unwrap();
        let folder = format!("{}/a", legacy.to_str().unwrap());
        insert_meeting(&pool, "m", &folder, None).await;

        assert_eq!(
            migrate_legacy_recordings_folder_in(&pool, docs.path()).await.unwrap(),
            None
        );
        assert!(legacy.is_dir() && !std::fs::symlink_metadata(&legacy).unwrap().file_type().is_symlink());
        assert_eq!(paths_of(&pool, "m").await.0, folder);
    }

    #[tokio::test]
    async fn no_legacy_folder_is_a_no_op() {
        let pool = test_pool().await;
        let docs = tempfile::tempdir().unwrap();
        assert_eq!(
            migrate_legacy_recordings_folder_in(&pool, docs.path()).await.unwrap(),
            None
        );
        assert!(!docs.path().join("parley-recordings").exists());
    }
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
