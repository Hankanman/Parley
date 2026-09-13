//! Locating (and, on demand, installing) the ffmpeg binary used for
//! encoding recordings, extracting clips, and decoding imported audio.
//!
//! [`find_ffmpeg_path`] is intentionally cheap and synchronous — it only
//! ever looks at the filesystem / `PATH`, never the network — so it is
//! safe to call from any context, sync or async, hot path or not.
//! Installing ffmpeg (a network download + archive unpack) is a
//! separate, explicit, async operation: [`ensure_ffmpeg_installed`].
//! This keeps a `find_ffmpeg_path()` call from ever blocking a caller
//! (e.g. an async Tauri command) on a synchronous download, and keeps a
//! transient network failure from poisoning the process for its
//! lifetime — only a *successful* resolution is ever cached, so
//! installing ffmpeg later (via [`ensure_ffmpeg_installed`] or by hand)
//! is picked up without an app restart.

use ffmpeg_sidecar::download::{
    check_latest_version, download_ffmpeg_package, ffmpeg_download_url, unpack_ffmpeg,
};
use log::{debug, warn};
use serde::Serialize;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use which::which;

const EXECUTABLE_NAME: &str = "ffmpeg";

/// Overrides the search entirely when set, pointing directly at an
/// ffmpeg binary. Mainly useful for development/testing.
const FFMPEG_PATH_ENV_VAR: &str = "PARLEY_FFMPEG_PATH";

/// Caches only a *successful* resolution — never `None` and never a
/// failed install — so a later successful install is observed by the
/// next call instead of being masked for the process lifetime.
static FFMPEG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Resolve a usable ffmpeg binary, if one is already available.
///
/// Cheap and non-blocking: only checks (in order) an explicit override
/// env var, the bundled sidecar binary next to the app executable, a
/// copy previously installed by [`ensure_ffmpeg_installed`] under the
/// app data dir, and finally `PATH`. Never downloads anything and never
/// panics.
pub fn find_ffmpeg_path() -> Option<PathBuf> {
    find_ffmpeg_path_with_source().map(|(path, _source)| path)
}

/// Like [`find_ffmpeg_path`], but also reports which candidate matched.
///
/// `source` is one of `"env"` (the `PARLEY_FFMPEG_PATH` override),
/// `"bundled"` (shipped next to the app executable), `"app-data"`
/// (previously installed by [`ensure_ffmpeg_installed`]), or `"path"`
/// (found on `PATH`). Note: when the result comes from the cache, the
/// source reflects whichever candidate matched the first time this was
/// resolved, not necessarily the one that would match a fresh scan.
pub fn find_ffmpeg_path_with_source() -> Option<(PathBuf, &'static str)> {
    if let Some(cached) = FFMPEG_PATH.get() {
        return Some((cached.clone(), cached_source(cached)));
    }

    let (found, source) = find_ffmpeg_path_uncached()?;
    // Best-effort: if another thread already cached a (necessarily
    // equally valid) result, keep that one rather than erroring.
    let _ = FFMPEG_PATH.set(found.clone());
    Some((found, source))
}

/// Best-effort re-derivation of which candidate a cached path came from,
/// used only when reporting status for an already-cached path (the
/// original source isn't stored alongside the cached `PathBuf`).
fn cached_source(path: &Path) -> &'static str {
    if let Ok(env_path) = std::env::var(FFMPEG_PATH_ENV_VAR) {
        if Path::new(&env_path) == path {
            return "env";
        }
    }
    if path.starts_with(ffmpeg_install_dir()) {
        return "app-data";
    }
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_folder) = exe_path.parent() {
            if path.parent() == Some(exe_folder) {
                return "bundled";
            }
        }
    }
    "path"
}

/// True when `path` exists, is a regular file, and (on unix) has at
/// least one executable bit set. Guards against accepting a
/// partially-written/corrupt download or a non-executable stray file
/// named `ffmpeg`.
fn is_executable_file(path: &Path) -> bool {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if !metadata.is_file() {
        return false;
    }
    metadata.permissions().mode() & 0o111 != 0
}

fn find_ffmpeg_path_uncached() -> Option<(PathBuf, &'static str)> {
    debug!("Starting search for ffmpeg executable");

    // ============================================================
    // PRIORITY 1: Explicit override
    // ============================================================
    if let Ok(path) = std::env::var(FFMPEG_PATH_ENV_VAR) {
        let path = PathBuf::from(path);
        if is_executable_file(&path) {
            debug!(
                "Using ffmpeg override from {}: {:?}",
                FFMPEG_PATH_ENV_VAR, path
            );
            return Some((path, "env"));
        }
        warn!(
            "{} is set but is not an executable file: {:?}",
            FFMPEG_PATH_ENV_VAR, path
        );
    }

    // ============================================================
    // PRIORITY 2: Bundled binary next to the app executable (production)
    // ============================================================
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_folder) = exe_path.parent() {
            let bundled = exe_folder.join(EXECUTABLE_NAME);
            if is_executable_file(&bundled) {
                debug!("Found bundled ffmpeg: {:?}", bundled);
                return Some((bundled, "bundled"));
            }
        }
    }

    // ============================================================
    // PRIORITY 3: Previously installed under the app data dir
    // ============================================================
    let installed = ffmpeg_install_dir().join(EXECUTABLE_NAME);
    if is_executable_file(&installed) {
        debug!("Found installed ffmpeg: {:?}", installed);
        return Some((installed, "app-data"));
    }

    // ============================================================
    // PRIORITY 4: PATH
    // ============================================================
    if let Ok(path) = which(EXECUTABLE_NAME) {
        if is_executable_file(&path) {
            debug!("Found ffmpeg in PATH: {:?}", path);
            return Some((path, "path"));
        }
    }

    debug!("ffmpeg not found");
    None
}

/// Directory ffmpeg is installed into by [`ensure_ffmpeg_installed`].
///
/// Uses the same app-data root as the rest of the app's user data
/// (`~/.local/share/io.github.hankanman.Parley/`, see [`crate::paths`])
/// rather than ffmpeg-sidecar's default of "next to the executable" — that
/// default is read-only inside an AppImage's squashfs mount, so a download
/// would succeed but unpacking into it would fail.
fn ffmpeg_install_dir() -> PathBuf {
    crate::paths::app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join(crate::paths::APP_IDENTIFIER))
        .join("ffmpeg")
}

/// Ensure a working ffmpeg binary is available, downloading and
/// unpacking one into the app data dir if necessary.
///
/// Idempotent and safe to call repeatedly (a no-op once ffmpeg is
/// found), and safe to call from an async context: all network and
/// filesystem work runs under [`tokio::task::spawn_blocking`], so it
/// never blocks the async runtime. Never panics; all failures are
/// returned as `Err`.
pub async fn ensure_ffmpeg_installed() -> anyhow::Result<PathBuf> {
    if let Some(path) = find_ffmpeg_path() {
        return Ok(path);
    }

    let installed = tokio::task::spawn_blocking(install_ffmpeg_blocking)
        .await
        .map_err(|e| anyhow::anyhow!("ffmpeg install task panicked: {e}"))??;

    // Cache the freshly installed binary so `find_ffmpeg_path()` picks
    // it up immediately, without needing to re-scan the filesystem.
    let _ = FFMPEG_PATH.set(installed.clone());
    Ok(installed)
}

/// Blocking body of [`ensure_ffmpeg_installed`] — must only ever run
/// inside `spawn_blocking`.
fn install_ffmpeg_blocking() -> anyhow::Result<PathBuf> {
    let destination = ffmpeg_install_dir();
    std::fs::create_dir_all(&destination).map_err(|e| {
        anyhow::anyhow!(
            "failed to create ffmpeg install directory {:?}: {e}",
            destination
        )
    })?;

    debug!("ffmpeg not found locally; downloading...");
    match check_latest_version() {
        Ok(version) => debug!("latest ffmpeg version: {}", version),
        Err(e) => debug!("skipping ffmpeg version check due to error: {e}"),
    }

    let download_url = ffmpeg_download_url()
        .map_err(|e| anyhow::anyhow!("failed to resolve ffmpeg download url: {e}"))?;

    debug!("downloading ffmpeg from: {:?}", download_url);
    let archive_path = download_ffmpeg_package(download_url, &destination)
        .map_err(|e| anyhow::anyhow!("failed to download ffmpeg: {e}"))?;
    debug!("downloaded ffmpeg package: {:?}", archive_path);

    debug!("extracting ffmpeg...");
    unpack_ffmpeg(&archive_path, &destination)
        .map_err(|e| anyhow::anyhow!("failed to unpack ffmpeg: {e}"))?;

    let installed = destination.join(EXECUTABLE_NAME);
    if !installed.is_file() {
        return Err(anyhow::anyhow!(
            "ffmpeg install completed but binary not found at {:?}",
            installed
        ));
    }

    // `unpack_ffmpeg` should already leave the binary executable on
    // unix, but some archive layouts don't preserve the bit — be
    // defensive rather than caching an unusable path.
    if !is_executable_file(&installed) {
        let mut perms = std::fs::metadata(&installed)
            .map_err(|e| anyhow::anyhow!("failed to stat installed ffmpeg: {e}"))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&installed, perms)
            .map_err(|e| anyhow::anyhow!("failed to make installed ffmpeg executable: {e}"))?;
    }

    debug!("ffmpeg installed at {:?}", installed);
    Ok(installed)
}

/// Status payload for the `ffmpeg_status` Tauri command — cheap and
/// synchronous, like [`find_ffmpeg_path`]; never downloads anything.
#[derive(Debug, Clone, Serialize)]
pub struct FfmpegStatus {
    pub installed: bool,
    pub path: Option<String>,
    /// One of `"env"`, `"bundled"`, `"app-data"`, `"path"`; `None` when
    /// `installed` is `false`.
    pub source: Option<String>,
}

/// Report whether ffmpeg is currently available and, if so, where it was
/// found (core logic behind the `ffmpeg_status` Tauri command in
/// `ffmpeg_commands.rs`). Never downloads anything.
pub fn ffmpeg_status() -> FfmpegStatus {
    match find_ffmpeg_path_with_source() {
        Some((path, source)) => FfmpegStatus {
            installed: true,
            path: Some(path.to_string_lossy().to_string()),
            source: Some(source.to_string()),
        },
        None => FfmpegStatus {
            installed: false,
            path: None,
            source: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_executable_file_rejects_missing_path() {
        assert!(!is_executable_file(Path::new(
            "/nonexistent/path/to/ffmpeg-that-does-not-exist"
        )));
    }

    #[test]
    fn is_executable_file_rejects_non_executable() {
        let dir = std::env::temp_dir().join(format!(
            "parley-ffmpeg-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ffmpeg");
        std::fs::write(&path, b"not really ffmpeg").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();

        assert!(!is_executable_file(&path));

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        assert!(is_executable_file(&path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffmpeg_install_dir_is_under_the_app_data_dir() {
        let dir = ffmpeg_install_dir();
        assert_eq!(dir, crate::paths::app_data_dir().unwrap().join("ffmpeg"));
    }
}
