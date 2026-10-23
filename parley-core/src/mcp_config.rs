//! Config surface for the standalone `parley-mcp` server.
//!
//! `parley-mcp` is a separate binary that an external AI client (Claude
//! Desktop, Claude Code) spawns by absolute path and which reads this app's
//! SQLite database directly — see the `parley-mcp` crate. It is not a
//! sidecar: the app never spawns it, it only helps the user register it.
//! This module backs the Settings → Integrations panel: it reports where the
//! database lives and does its best to locate the binary so the UI can render
//! a ready-to-paste client config.
//!
//! The AppImage carries a copy next to `parley`, but a path into the
//! AppImage's mount changes every launch, so clients can't use it directly.
//! [`install_bundled_binary`] copies it to `~/.local/bin` (as `install.sh`
//! does) to give it a stable path.
//!
//! Locating the binary is best-effort. A from-source build leaves it in the
//! workspace `target/` dir (see `build.sh`), which is knowable at compile time
//! via `CARGO_MANIFEST_DIR`, so a from-source build+run resolves it
//! automatically. The returned `binary_path` is only a suggestion for the
//! snippet.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Filename of the MCP server binary produced by `cargo build -p parley-mcp`.
const MCP_BIN_NAME: &str = "parley-mcp";

/// Env var an advanced user can set to pin the binary location explicitly.
const MCP_BIN_ENV: &str = "PARLEY_MCP_BIN";

/// What the Integrations panel needs to render MCP-client config.
///
/// Fields are `pub` so the GPUI Integrations settings page can read them
/// directly without round-tripping through JSON.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerInfo {
    /// Absolute path to the Parley SQLite database this app uses. The MCP
    /// server resolves the same default on its own, so a `--db` flag is only
    /// needed when this differs from that default.
    #[serde(rename = "dbPath")]
    pub db_path: String,
    /// Whether that database file exists yet (it won't until the app has run
    /// once and created it).
    #[serde(rename = "dbExists")]
    pub db_exists: bool,
    /// Whether `db_path` is the platform-default location the MCP server would
    /// pick with no `--db`/`$PARLEY_DB_PATH`. When true the snippet can omit
    /// the flag entirely.
    #[serde(rename = "dbIsDefault")]
    pub db_is_default: bool,
    /// Best guess at the `parley-mcp` binary's absolute path, or `null` if it
    /// couldn't be found. The UI treats this as a prefill the user can edit.
    #[serde(rename = "binaryPath")]
    pub binary_path: Option<String>,
    /// Whether `binary_path` was found and points at an existing file.
    #[serde(rename = "binaryFound")]
    pub binary_found: bool,
    /// The copy inside the running AppImage, when there is one. Not usable
    /// as a client command (see the module docs); [`install_bundled_binary`]
    /// copies it to [`install_path`].
    #[serde(rename = "bundledPath")]
    pub bundled_path: Option<String>,
    /// Whether the binary at [`install_path`] is byte-for-byte the bundled
    /// one, i.e. installing again would change nothing.
    #[serde(rename = "installedIsCurrent")]
    pub installed_is_current: bool,
}

/// The default DB location the MCP server picks with no flag/env, mirroring
/// `parley_mcp::cli::default_db_path` (`<data-dir>/io.github.hankanman.Parley/…`,
/// or the unmigrated legacy dir). Kept in sync by hand — both resolve the
/// same way as [`crate::paths::app_data_dir`].
fn default_db_path() -> Option<PathBuf> {
    crate::paths::app_data_dir()
        .ok()
        .map(|d| d.join("meeting_minutes.sqlite"))
}

/// Where [`install_bundled_binary`] puts the binary: `~/.local/bin`, the same
/// directory `install.sh` installs `parley` into by default.
pub fn install_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".local/bin").join(MCP_BIN_NAME))
}

/// The `parley-mcp` packaged next to the running executable inside an
/// AppImage (`$APPDIR`), if any.
fn bundled_binary() -> Option<PathBuf> {
    let appdir = std::env::var_os("APPDIR")?;
    let exe = std::env::current_exe().ok()?;
    if !exe.starts_with(&appdir) {
        return None;
    }
    let candidate = exe.parent()?.join(MCP_BIN_NAME);
    candidate.is_file().then_some(candidate)
}

/// Copy the AppImage's bundled `parley-mcp` to [`install_path`], replacing
/// any older copy, and return where it went. Written to a temporary file
/// and renamed into place, so a client mid-launch never sees half a binary.
pub fn install_bundled_binary() -> Result<PathBuf, String> {
    let source = bundled_binary().ok_or_else(|| "This build doesn't include parley-mcp".to_string())?;
    let target = install_path().ok_or_else(|| "Couldn't find your home folder".to_string())?;
    install_binary(&source, &target)?;
    Ok(target)
}

fn install_binary(source: &Path, target: &Path) -> Result<(), String> {
    let dir = target.parent().ok_or_else(|| "Invalid install path".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create {}: {}", dir.display(), e))?;
    let tmp = dir.join(format!(".{}.tmp", MCP_BIN_NAME));
    std::fs::copy(source, &tmp).map_err(|e| format!("Couldn't copy parley-mcp: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Couldn't make parley-mcp executable: {}", e))?;
    }
    std::fs::rename(&tmp, target).map_err(|e| format!("Couldn't install parley-mcp: {}", e))?;
    Ok(())
}

fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Search likely locations for the `parley-mcp` binary, in priority order.
/// Returns the first path that exists. Never returns the copy inside an
/// AppImage mount, whose path changes every launch.
fn locate_binary() -> Option<PathBuf> {
    // 1. Explicit override — an advanced user pointing us at a specific build.
    if let Ok(env_path) = std::env::var(MCP_BIN_ENV) {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.is_file() {
                return Some(path);
            }
        }
    }

    // 2. Installed by `install.sh` or the Integrations page.
    if let Some(candidate) = install_path() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // 3. Next to the AppImage file itself (`install.sh` with a custom
    //    `PARLEY_BIN_DIR` puts both there).
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        if let Some(dir) = Path::new(&appimage).parent() {
            let candidate = dir.join(MCP_BIN_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 4. Next to the running executable — covers the case where someone has
    //    copied the binary alongside the app. Skipped inside an AppImage.
    if bundled_binary().is_none() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join(MCP_BIN_NAME);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    // 5. The workspace target dir this app was built from. `CARGO_MANIFEST_DIR`
    //    is `…/parley-core` at compile time; the workspace `target/` lives
    //    one level up. This is what resolves a from-source build+run.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent();
    if let Some(root) = workspace_root {
        for profile in ["release", "debug"] {
            let candidate = root.join("target").join(profile).join(MCP_BIN_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 6. Anywhere on PATH.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(MCP_BIN_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Report the DB path plus a best-effort location for the `parley-mcp` binary
/// so the Integrations settings panel can render client-registration config.
pub fn get_mcp_server_info() -> Result<McpServerInfo, String> {
    let db_path = crate::paths::app_data_dir()?.join("meeting_minutes.sqlite");

    let db_is_default = default_db_path().map(|d| d == db_path).unwrap_or(false);

    let binary = locate_binary();
    let binary_found = binary.is_some();
    let bundled = bundled_binary();
    let installed_is_current = match (&bundled, install_path()) {
        (Some(bundled), Some(installed)) => same_contents(bundled, &installed),
        _ => false,
    };

    Ok(McpServerInfo {
        db_exists: db_path.is_file(),
        db_path: db_path.to_string_lossy().into_owned(),
        db_is_default,
        binary_path: binary.map(|p| p.to_string_lossy().into_owned()),
        binary_found,
        bundled_path: bundled.map(|p| p.to_string_lossy().into_owned()),
        installed_is_current,
    })
}

/// Open the folder containing the `parley-mcp` binary in the system file
/// manager, so the user can grab its path or confirm it's there.
pub fn reveal_mcp_binary(path: String) -> Result<(), String> {
    let path = PathBuf::from(&path);
    let dir = if path.is_file() {
        path.parent().map(Path::to_path_buf)
    } else {
        Some(path)
    }
    .ok_or_else(|| "No folder to open".to_string())?;

    std::process::Command::new("xdg-open")
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("Failed to open folder: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_binary_creates_the_dir_and_replaces_an_older_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundled");
        std::fs::write(&source, b"new build").unwrap();
        let target = tmp.path().join("home/.local/bin").join(MCP_BIN_NAME);

        install_binary(&source, &target).unwrap();
        assert!(same_contents(&source, &target));

        std::fs::write(&target, b"old build").unwrap();
        assert!(!same_contents(&source, &target));
        install_binary(&source, &target).unwrap();
        assert!(same_contents(&source, &target));
        assert!(!target.with_file_name(format!(".{}.tmp", MCP_BIN_NAME)).exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
}
