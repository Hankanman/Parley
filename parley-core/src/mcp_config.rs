//! Config surface for the standalone `parley-mcp` server.
//!
//! `parley-mcp` is a separate binary that an external AI client (Claude
//! Desktop, Claude Code) spawns by absolute path and which reads this app's
//! SQLite database directly — see the `parley-mcp` crate. It is deliberately
//! *not* a Tauri sidecar and *not* bundled into the app (an absolute path into
//! an AppImage mount is not stable across launches), so the app cannot spawn
//! it; all it can do is help the user register it. This module backs the
//! Settings → Integrations panel: it reports where the database lives and does
//! its best to locate the binary so the UI can render a ready-to-paste client
//! config.
//!
//! Locating the binary is best-effort. The build leaves it in the workspace
//! `target/` dir (see `build.sh`), which is knowable at compile time via
//! `CARGO_MANIFEST_DIR`, so a from-source build+run resolves it automatically.
//! When that fails (e.g. a relocated binary), the UI lets the user point at it
//! by hand — the returned `binary_path` is only a suggestion for the snippet.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Filename of the MCP server binary produced by `cargo build -p parley-mcp`.
const MCP_BIN_NAME: &str = "parley-mcp";

/// Env var an advanced user can set to pin the binary location explicitly.
const MCP_BIN_ENV: &str = "PARLEY_MCP_BIN";

/// What the Integrations panel needs to render MCP-client config.
///
/// Fields are `pub` (rather than accessed only through `Serialize`/JSON) so
/// a non-Tauri shell — the GPUI Integrations settings page — can read them
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

/// Search likely locations for the `parley-mcp` binary, in priority order.
/// Returns the first path that exists.
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

    // 2. Next to the running executable — covers the case where someone has
    //    copied the binary alongside the app.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(MCP_BIN_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 3. The workspace target dir this app was built from. `CARGO_MANIFEST_DIR`
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

    // 4. Anywhere on PATH.
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
/// Core logic behind the `get_mcp_server_info` Tauri command in
/// `mcp_config_commands.rs`.
pub fn get_mcp_server_info() -> Result<McpServerInfo, String> {
    let db_path = crate::paths::app_data_dir()?.join("meeting_minutes.sqlite");

    let db_is_default = default_db_path()
        .map(|d| d == db_path)
        .unwrap_or(false);

    let binary = locate_binary();
    let binary_found = binary.is_some();

    Ok(McpServerInfo {
        db_exists: db_path.is_file(),
        db_path: db_path.to_string_lossy().into_owned(),
        db_is_default,
        binary_path: binary.map(|p| p.to_string_lossy().into_owned()),
        binary_found,
    })
}

/// Open the folder containing the `parley-mcp` binary in the system file
/// manager, so the user can grab its path or confirm it's there. Core logic
/// behind the `reveal_mcp_binary` Tauri command in `mcp_config_commands.rs`.
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
