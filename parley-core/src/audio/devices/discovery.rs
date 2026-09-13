use anyhow::Result;
use log::info;

use crate::audio::pw::{self, PwDevice};

/// List all available audio devices from the PipeWire registry.
///
/// Returns `{id, label, kind}` entries — `id` is the stable
/// `node.name`, `label` the human-readable `node.description`.
pub async fn list_audio_devices() -> Result<Vec<PwDevice>> {
    tokio::task::spawn_blocking(pw::enumerate_devices).await?
}

/// Verify audio capture is available by connecting to PipeWire.
///
/// Linux has no macOS-style permission dialog outside sandboxed
/// (Flatpak) environments; a successful registry roundtrip means audio
/// is reachable.
pub fn trigger_audio_permission() -> Result<bool> {
    match pw::enumerate_devices() {
        Ok(devices) => {
            info!(
                "[trigger_audio_permission] PipeWire reachable ({} audio nodes)",
                devices.len()
            );
            Ok(true)
        }
        Err(e) => {
            info!("[trigger_audio_permission] PipeWire not reachable: {}", e);
            Ok(false)
        }
    }
}
