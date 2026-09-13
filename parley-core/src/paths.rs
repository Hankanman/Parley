//! Application directories.
//!
//! User data lives in `dirs::data_dir()/<identifier>`
//! (`~/.local/share/io.github.hankanman.Parley`). The identifier is
//! reverse-DNS under the GitHub owner rather than a bare `parley`, which
//! KDE's unrelated Parley vocabulary trainer already uses.
//!
//! Before the rename the directory was `com.meetily.ai`. On startup
//! [`migrate_legacy_data_dir`] moves it to the new name (a same-filesystem
//! rename, so the multi-GB models directory isn't copied) and leaves a
//! symlink at the old path, so anything still configured with it — an MCP
//! client's `--db` flag, a user script — keeps working. If the move can't
//! happen, [`app_data_dir`] keeps using the old directory rather than
//! starting over with an empty one.

use std::path::{Path, PathBuf};

/// App identifier: the data-directory name, and the desktop-entry / Wayland
/// app id. `parley-mcp` (`parley_mcp::cli::APP_IDENTIFIER`) resolves the
/// database the same way.
pub const APP_IDENTIFIER: &str = "io.github.hankanman.Parley";

/// The data-directory name used before the Parley rename.
pub const LEGACY_APP_IDENTIFIER: &str = "com.meetily.ai";

/// Per-user application data directory (`~/.local/share/io.github.hankanman.Parley`),
/// or the pre-rename directory if it hasn't been migrated. See module docs.
///
/// Errors only if the platform has no data directory at all (no `$HOME`).
pub fn app_data_dir() -> Result<PathBuf, String> {
    dirs::data_dir()
        .map(|base| resolve_data_dir(&base))
        .ok_or_else(|| "Could not resolve the user data directory".to_string())
}

/// Which data directory to use under `base`: the current one if it exists,
/// else a not-yet-migrated legacy one, else the current one (fresh install).
pub fn resolve_data_dir(base: &Path) -> PathBuf {
    let current = base.join(APP_IDENTIFIER);
    let legacy = base.join(LEGACY_APP_IDENTIFIER);
    if !current.exists() && legacy.is_dir() {
        legacy
    } else {
        current
    }
}

/// Result of [`migrate_legacy_data_dir_in`].
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// No legacy directory, or the current one already exists.
    NotNeeded,
    /// The legacy directory was moved to the current name.
    Moved { from: PathBuf, to: PathBuf },
    /// The move failed; the legacy directory is still in use.
    Failed(String),
}

/// Move the pre-rename data directory to its new name, once. Call before
/// anything resolves a path under [`app_data_dir`].
pub fn migrate_legacy_data_dir() {
    let Some(base) = dirs::data_dir() else {
        return;
    };
    match migrate_legacy_data_dir_in(&base) {
        Migration::NotNeeded => {}
        Migration::Moved { from, to } => {
            log::info!("Moved app data from {} to {}", from.display(), to.display())
        }
        Migration::Failed(e) => log::warn!(
            "Couldn't move app data to its new location ({e}); still using {}",
            base.join(LEGACY_APP_IDENTIFIER).display()
        ),
    }
}

/// [`migrate_legacy_data_dir`] against an explicit base directory, so it can
/// be tested without touching the real data directory.
pub fn migrate_legacy_data_dir_in(base: &Path) -> Migration {
    let current = base.join(APP_IDENTIFIER);
    let legacy = base.join(LEGACY_APP_IDENTIFIER);

    // `symlink_metadata`: a legacy path that is already our compatibility
    // symlink means the move happened on an earlier run.
    let legacy_is_real_dir = std::fs::symlink_metadata(&legacy)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if !legacy_is_real_dir {
        return Migration::NotNeeded;
    }
    if current.exists() {
        log::warn!(
            "Both {} and {} exist; using the new one and leaving the old one untouched",
            current.display(),
            legacy.display()
        );
        return Migration::NotNeeded;
    }

    if let Err(e) = std::fs::rename(&legacy, &current) {
        return Migration::Failed(e.to_string());
    }
    // Best effort: the data is already safely at the new path.
    if let Err(e) = std::os::unix::fs::symlink(APP_IDENTIFIER, &legacy) {
        log::warn!("Couldn't leave a symlink at {}: {e}", legacy.display());
    }
    Migration::Moved {
        from: legacy,
        to: current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_uses_the_new_identifier() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(resolve_data_dir(base.path()), base.path().join(APP_IDENTIFIER));
        assert_eq!(migrate_legacy_data_dir_in(base.path()), Migration::NotNeeded);
        assert!(!base.path().join(APP_IDENTIFIER).exists());
    }

    #[test]
    fn unmigrated_legacy_dir_is_still_used() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join(LEGACY_APP_IDENTIFIER)).unwrap();
        assert_eq!(
            resolve_data_dir(base.path()),
            base.path().join(LEGACY_APP_IDENTIFIER)
        );
    }

    #[test]
    fn migration_moves_data_and_leaves_a_working_symlink() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join(LEGACY_APP_IDENTIFIER);
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("meeting_minutes.sqlite"), b"db").unwrap();

        let current = base.path().join(APP_IDENTIFIER);
        assert_eq!(
            migrate_legacy_data_dir_in(base.path()),
            Migration::Moved {
                from: legacy.clone(),
                to: current.clone()
            }
        );
        assert_eq!(std::fs::read(current.join("meeting_minutes.sqlite")).unwrap(), b"db");
        assert!(std::fs::symlink_metadata(&legacy).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(legacy.join("meeting_minutes.sqlite")).unwrap(), b"db");
        assert_eq!(resolve_data_dir(base.path()), current);

        // A second run sees the symlink and does nothing.
        assert_eq!(migrate_legacy_data_dir_in(base.path()), Migration::NotNeeded);
    }

    #[test]
    fn migration_never_merges_into_an_existing_new_dir() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join(LEGACY_APP_IDENTIFIER);
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("old"), b"old").unwrap();
        std::fs::create_dir(base.path().join(APP_IDENTIFIER)).unwrap();

        assert_eq!(migrate_legacy_data_dir_in(base.path()), Migration::NotNeeded);
        assert!(legacy.join("old").is_file());
        assert_eq!(resolve_data_dir(base.path()), base.path().join(APP_IDENTIFIER));
    }
}
