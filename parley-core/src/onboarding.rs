use anyhow::{anyhow, Result};
use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::database::repositories::setting::{SettingsRepository, KEY_ONBOARDING_STATUS};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OnboardingStatus {
    pub version: String,
    pub completed: bool,
    pub current_step: u8,
    pub model_status: ModelStatus,
    pub last_updated: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ModelStatus {
    /// "downloaded" | "not_downloaded" | "downloading". Kept as `parakeet`
    /// in the on-disk field name for backward compatibility with existing
    /// onboarding-status.json files written before Parakeet was removed.
    /// Now reflects the local Whisper model state.
    #[serde(rename = "parakeet", alias = "transcription")]
    pub transcription: String,
    pub summary: String, // Summary model (gemma3:1b or gemma3:4b)
}

impl Default for OnboardingStatus {
    fn default() -> Self {
        Self {
            version: "1.0".to_string(),
            completed: false,
            current_step: 1,
            model_status: ModelStatus {
                transcription: "not_downloaded".to_string(),
                summary: "not_downloaded".to_string(),
            },
            last_updated: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// One-time, read-only import of onboarding status from the legacy
/// tauri-plugin-store JSON file (`onboarding-status.json`), written before
/// this moved to SQLite. Read directly off disk (same `$APPDATA` location
/// and flat `{"status": ...}` shape the store plugin used, with its default
/// serializer) so this module no longer depends on the plugin at all. The
/// file is left in place afterward.
fn import_legacy_onboarding_status() -> Option<OnboardingStatus> {
    let path = crate::paths::app_data_dir().ok()?.join("onboarding-status.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    let value = root.get("status")?;
    match serde_json::from_value::<OnboardingStatus>(value.clone()) {
        Ok(status) => Some(status),
        Err(e) => {
            warn!("Failed to deserialize legacy onboarding status: {}", e);
            None
        }
    }
}

/// Read onboarding status straight from the database, distinguishing "no DB
/// pool yet" / "pool ready but nothing saved" (both `None`) from an actual
/// saved status (imports the legacy JSON file into SQLite on first read, if
/// present).
pub async fn fetch_onboarding_status(
    pool: Option<sqlx::SqlitePool>,
) -> Result<Option<OnboardingStatus>> {
    let pool = match pool {
        Some(pool) => pool,
        None => return Ok(None),
    };

    match SettingsRepository::get_setting::<OnboardingStatus>(&pool, KEY_ONBOARDING_STATUS).await {
        Ok(Some(status)) => {
            info!(
                "Loaded onboarding status from database - Step: {}, Completed: {}",
                status.current_step, status.completed
            );
            Ok(Some(status))
        }
        Ok(None) => {
            if let Some(imported) = import_legacy_onboarding_status() {
                info!("Importing legacy onboarding status from onboarding-status.json");
                if let Err(e) =
                    SettingsRepository::set_setting(&pool, KEY_ONBOARDING_STATUS, &imported).await
                {
                    warn!("Failed to persist imported onboarding status: {}", e);
                }
                Ok(Some(imported))
            } else {
                info!("No stored onboarding status found, using defaults");
                Ok(None)
            }
        }
        Err(e) => Err(anyhow!("Failed to load onboarding status: {}", e)),
    }
}

/// Load onboarding status, falling back to defaults if nothing has been
/// saved yet (or the database isn't ready yet during early startup).
pub async fn load_onboarding_status(pool: Option<sqlx::SqlitePool>) -> Result<OnboardingStatus> {
    Ok(fetch_onboarding_status(pool).await?.unwrap_or_default())
}

/// Save onboarding status to the database
pub async fn save_onboarding_status(
    pool: Option<sqlx::SqlitePool>,
    status: &OnboardingStatus,
) -> Result<()> {
    info!(
        "Saving onboarding status: step={}, completed={}",
        status.current_step, status.completed
    );

    // Update last_updated timestamp
    let mut status = status.clone();
    status.last_updated = chrono::Utc::now().to_rfc3339();

    let pool =
        pool.ok_or_else(|| anyhow!("Database not yet initialized — try again shortly"))?;
    SettingsRepository::set_setting(&pool, KEY_ONBOARDING_STATUS, &status)
        .await
        .map_err(|e| anyhow!("Failed to save onboarding status: {}", e))?;

    info!("Successfully persisted onboarding status to database");
    Ok(())
}

/// Reset onboarding status (delete from database)
pub async fn reset_onboarding_status(pool: Option<sqlx::SqlitePool>) -> Result<()> {
    info!("Resetting onboarding status");

    let pool =
        pool.ok_or_else(|| anyhow!("Database not yet initialized — try again shortly"))?;
    SettingsRepository::delete_setting(&pool, KEY_ONBOARDING_STATUS)
        .await
        .map_err(|e| anyhow!("Failed to reset onboarding status: {}", e))?;

    info!("Successfully reset onboarding status");
    Ok(())
}
