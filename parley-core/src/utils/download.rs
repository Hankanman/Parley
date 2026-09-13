//! Shared streaming HTTP downloader for model weights.
//!
//! `whisper_engine::whisper_engine::WhisperEngine::download_model` and
//! `summary::summary_engine::model_manager::ModelManager::download_model_detailed`
//! used to carry two nearly-identical copies of this loop (progress
//! reporting, cooperative cancellation, truncated-download detection) that
//! had drifted apart in one meaningful way: only the model manager supported
//! HTTP `Range` resume. This module gives both call sites the same core
//! loop and lets resume, chunk timeouts, etc. be opted into per call.
//!
//! This is intentionally silent about Tauri — `download_file` never emits
//! an event. Callers own their own event names/payload shapes and translate
//! `on_progress` calls / the returned `Result` into whatever their frontend
//! expects.

use anyhow::{anyhow, Result};
use reqwest::Client;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::RwLock;
use tokio::time::timeout;

/// Marker error indicating a download was cancelled via
/// [`DownloadGuard::request_cancel`]. Detect it with `Error::downcast_ref`
/// rather than string-matching `to_string()` — callers format their own
/// user-facing message from it, since whisper and the built-in-AI model
/// manager use different conventions for surfacing "the user cancelled" to
/// the frontend (see `whisper_engine::commands::whisper_download_model` vs
/// `summary_engine::commands::builtin_ai_download_model`).
#[derive(Debug)]
pub struct DownloadCancelled;

impl std::fmt::Display for DownloadCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Download cancelled by user")
    }
}

impl std::error::Error for DownloadCancelled {}

/// Returns true if `err` is (or wraps) a [`DownloadCancelled`] marker.
pub fn is_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DownloadCancelled>().is_some()
}

/// Tracks in-flight downloads (to reject concurrent duplicate requests) and
/// cooperative cancellation requests, keyed by an opaque resource name
/// (a model name in both current callers). Shared so whisper_engine and
/// summary_engine::model_manager use identical check-and-insert semantics
/// instead of each hand-rolling a `HashSet` + `Option<String>` pair.
pub struct DownloadGuard {
    active: RwLock<HashSet<String>>,
    cancel_flag: RwLock<Option<String>>,
}

impl Default for DownloadGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadGuard {
    pub fn new() -> Self {
        Self {
            active: RwLock::new(HashSet::new()),
            cancel_flag: RwLock::new(None),
        }
    }

    /// Atomically checks that `name` isn't already downloading and marks it
    /// active, under a single write lock — two concurrent callers for the
    /// same name can't both pass the check and race to write the same file.
    /// Also clears any stale cancellation flag left over from a previous
    /// run for `name`.
    pub async fn begin(&self, name: &str) -> Result<()> {
        {
            let mut active = self.active.write().await;
            if !active.insert(name.to_string()) {
                return Err(anyhow!("Download already in progress for {}", name));
            }
        }
        *self.cancel_flag.write().await = None;
        Ok(())
    }

    /// Marks `name` as no longer downloading. Safe to call even if `name`
    /// isn't currently tracked (e.g. a second cleanup after cancellation).
    pub async fn finish(&self, name: &str) {
        self.active.write().await.remove(name);
    }

    /// Requests cancellation of an in-flight download for `name`. The
    /// running [`download_file`] loop notices the flag before its next
    /// chunk and unwinds with [`DownloadCancelled`]; this also immediately
    /// marks `name` as no longer active so a retry isn't rejected while the
    /// old loop is still winding down.
    pub async fn request_cancel(&self, name: &str) {
        *self.cancel_flag.write().await = Some(name.to_string());
        self.active.write().await.remove(name);
    }

    /// True if cancellation has been requested for `name`.
    pub async fn is_cancel_requested(&self, name: &str) -> bool {
        self.cancel_flag.read().await.as_deref() == Some(name)
    }

    /// True if a download for `name` is currently tracked as active.
    pub async fn is_active(&self, name: &str) -> bool {
        self.active.read().await.contains(name)
    }
}

/// Configuration for a single [`download_file`] call.
pub struct DownloadRequest<'a> {
    pub client: &'a Client,
    pub url: &'a str,
    pub dest_path: &'a Path,
    /// If true and a partial file already exists at `dest_path`, resume via
    /// an HTTP `Range` request instead of restarting from scratch. Falls
    /// back to a fresh full download if the server ignores the `Range`
    /// header (anything other than `206 Partial Content`).
    pub resume: bool,
    /// Optional per-chunk stall timeout — if no data arrives within this
    /// window the download fails. `None` disables the per-chunk timeout
    /// (only the underlying `Client`'s own timeout, if any, then applies).
    pub chunk_timeout: Option<Duration>,
}

/// Outcome of a successful [`download_file`] call.
pub struct DownloadOutcome {
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub resumed: bool,
}

/// Stream `req.url` to `req.dest_path`, reporting progress and honoring
/// cooperative cancellation via `guard`/`name`.
///
/// `on_progress(downloaded_bytes, total_bytes, speed_mbps)` fires on the
/// first byte, roughly every 500ms while a chunk lands, and once more on
/// completion. `total_bytes` is 0 if the server didn't send a
/// `Content-Length`.
///
/// On success, `dest_path` contains exactly `total_bytes` bytes — verified
/// after the stream ends, so a connection that closes early without the
/// HTTP client itself surfacing an error doesn't get silently treated as
/// complete. That verification failure (and any error while streaming
/// chunks) removes the partial file. Cancellation is the one exception: the
/// partial file is left on disk so a later call with `resume: true` can
/// pick up where it left off; callers that want a clean slate on
/// cancellation should remove the file themselves.
pub async fn download_file(
    req: DownloadRequest<'_>,
    guard: &DownloadGuard,
    name: &str,
    mut on_progress: impl FnMut(u64, u64, f64) + Send,
) -> Result<DownloadOutcome> {
    let DownloadRequest {
        client,
        url,
        dest_path,
        resume,
        chunk_timeout,
    } = req;

    let existing_size: u64 = if resume && dest_path.exists() {
        fs::metadata(dest_path).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    let mut request = client.get(url);
    if existing_size > 0 {
        log::info!(
            "Resuming download from byte {} ({:.1} MB)",
            existing_size,
            existing_size as f64 / (1024.0 * 1024.0)
        );
        request = request.header("Range", format!("bytes={}-", existing_size));
    }

    let response = request
        .send()
        .await
        .map_err(|e| anyhow!("Failed to start download: {}", e))?;

    let (total_size, resuming) = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        // Server honored the Range request - total size is what we already
        // have plus whatever it's sending now.
        let remaining = response.content_length().unwrap_or(0);
        log::info!(
            "Server supports resume, {} MB remaining",
            remaining / (1024 * 1024)
        );
        (existing_size + remaining, true)
    } else if response.status().is_success() {
        if existing_size > 0 {
            log::warn!("Server doesn't support resume, starting fresh download");
        }
        (response.content_length().unwrap_or(0), false)
    } else {
        return Err(anyhow!(
            "Download failed with status: {}",
            response.status()
        ));
    };

    log::info!(
        "Downloading {} -> {} ({:.1} MB expected)",
        url,
        dest_path.display(),
        total_size as f64 / (1024.0 * 1024.0)
    );

    let file = if resuming {
        OpenOptions::new()
            .write(true)
            .append(true)
            .open(dest_path)
            .await
            .map_err(|e| anyhow!("Failed to open file for append: {}", e))?
    } else {
        fs::File::create(dest_path)
            .await
            .map_err(|e| anyhow!("Failed to create file: {}", e))?
    };

    // 8MB buffer to cut down on disk I/O syscalls during the hot loop.
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
    let mut downloaded: u64 = if resuming { existing_size } else { 0 };

    // Report the starting position immediately so the UI doesn't sit blank
    // while the first chunk is in flight.
    on_progress(downloaded, total_size, 0.0);

    let mut last_reported_percent = percent_of(downloaded, total_size);
    let mut last_report_time = std::time::Instant::now();
    let mut bytes_since_last_report: u64 = 0;

    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();

    loop {
        if guard.is_cancel_requested(name).await {
            log::info!("Download cancelled for {}", name);
            let _ = writer.flush().await;
            return Err(anyhow::Error::new(DownloadCancelled));
        }

        let next = match chunk_timeout {
            Some(t) => match timeout(t, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    log::warn!(
                        "Download timeout for {}: no data received for {} seconds",
                        name,
                        t.as_secs()
                    );
                    let _ = writer.flush().await;
                    return Err(anyhow!(
                        "Download timeout - No data received for {} seconds",
                        t.as_secs()
                    ));
                }
            },
            None => stream.next().await,
        };

        let chunk = match next {
            None => break,
            Some(Ok(chunk)) => chunk,
            Some(Err(e)) => {
                log::error!("Download error for {}: {:?}", name, e);
                let _ = writer.flush().await;

                let category = if e.is_timeout() {
                    "Connection timeout - Check your internet"
                } else if e.is_connect() {
                    "Connection failed - Check your internet"
                } else if e.is_body() {
                    "Stream interrupted - Network unstable"
                } else {
                    "Download error"
                };
                return Err(anyhow!("{}: {}", category, e));
            }
        };

        let chunk_len = chunk.len() as u64;
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| anyhow!("Failed to write chunk to file: {}", e))?;

        downloaded += chunk_len;
        bytes_since_last_report += chunk_len;

        let progress_percent = percent_of(downloaded, total_size);
        let elapsed_since_report = last_report_time.elapsed();
        let is_complete = total_size > 0 && downloaded >= total_size;
        let should_report = progress_percent > last_reported_percent
            || is_complete
            || elapsed_since_report.as_millis() >= 500;

        if should_report {
            let speed_mbps = if elapsed_since_report.as_secs_f64() > 0.0 {
                (bytes_since_last_report as f64 / (1024.0 * 1024.0))
                    / elapsed_since_report.as_secs_f64()
            } else {
                0.0
            };

            log::info!(
                "Download progress ({}): {:.1} MB / {:.1} MB ({:.1} MB/s)",
                name,
                downloaded as f64 / (1024.0 * 1024.0),
                total_size as f64 / (1024.0 * 1024.0),
                speed_mbps
            );

            on_progress(downloaded, total_size, speed_mbps);

            last_reported_percent = progress_percent;
            last_report_time = std::time::Instant::now();
            bytes_since_last_report = 0;
        }
    }

    writer
        .flush()
        .await
        .map_err(|e| anyhow!("Failed to flush file: {}", e))?;
    drop(writer);

    // Final size verification: a stream that ends early without the HTTP
    // client itself surfacing an error would otherwise get trusted as a
    // complete file.
    if total_size > 0 && downloaded != total_size {
        log::warn!(
            "Download for {} ended early: got {} of {} bytes",
            name,
            downloaded,
            total_size
        );
        if let Err(e) = fs::remove_file(dest_path).await {
            log::warn!("Failed to clean up truncated download file: {}", e);
        }
        return Err(anyhow!(
            "Download incomplete for {}: got {} of {} bytes",
            name,
            downloaded,
            total_size
        ));
    }

    // Always report a final 100%-equivalent sample, even if the last chunk
    // didn't cross the report threshold.
    on_progress(downloaded, total_size, 0.0);

    log::info!("Download completed for {}: {} bytes", name, downloaded);

    Ok(DownloadOutcome {
        total_bytes: total_size,
        downloaded_bytes: downloaded,
        resumed: resuming,
    })
}

fn percent_of(downloaded: u64, total: u64) -> u8 {
    if total > 0 {
        ((downloaded as f64 / total as f64) * 100.0).min(100.0) as u8
    } else {
        0
    }
}
