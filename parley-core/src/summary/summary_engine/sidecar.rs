// Sidecar process lifecycle management for llama-helper
// Handles spawning, health checking, keep-alive, and graceful shutdown

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use llama_protocol::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, RwLock};

use super::models;

// ============================================================================
// Sidecar State Management
// ============================================================================

/// Sidecar process manager with keep-alive and health monitoring.
///
/// Lives behind an `Arc` (see `client.rs`); the background health/idle
/// loops hold a clone of that `Arc` plus the spawn generation they belong
/// to, and exit as soon as a newer spawn supersedes them — so restarting
/// the sidecar can never accumulate loops.
pub struct SidecarManager {
    /// Child process handle
    child_process: Mutex<Option<Child>>,

    /// Stdin writer for sending requests
    stdin_writer: Mutex<Option<ChildStdin>>,

    /// Stdout reader for receiving responses
    stdout_reader: Mutex<Option<BufReader<ChildStdout>>>,

    /// Last activity timestamp
    last_activity: RwLock<Instant>,

    /// Health status
    is_healthy: AtomicBool,

    /// Shutdown flag
    should_shutdown: AtomicBool,

    /// Active request count (for graceful shutdown). Arc'd because each
    /// in-flight `RequestGuard` holds its own handle to it.
    active_request_count: Arc<AtomicUsize>,

    /// Serializes the full write-request + read-response round trip so
    /// concurrent callers (including the background health-check ping)
    /// can never interleave their stdin writes/stdout reads and end up
    /// reading back a response meant for a different request.
    request_lock: Mutex<()>,

    /// Bumped by every `spawn()`. Background loops capture the value at
    /// their spawn and exit when it changes, so a respawn retires the
    /// previous spawn's loops instead of stacking new ones on top.
    spawn_generation: AtomicU64,

    /// Path to llama-helper binary
    helper_binary_path: PathBuf,

    /// Current model path (if loaded)
    current_model_path: RwLock<Option<PathBuf>>,

    /// Idle timeout in seconds (configurable via env var)
    idle_timeout_secs: u64,
}

/// RAII guard for tracking active requests
/// Decrements the active request count when dropped
struct RequestGuard {
    counter: Arc<AtomicUsize>,
}

impl RequestGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl SidecarManager {
    /// Create a new sidecar manager
    pub fn new(_app_data_dir: PathBuf) -> Result<Self> {
        let helper_binary_path = Self::resolve_helper_binary()?;

        // Get idle timeout from env var or use default
        let idle_timeout_secs = std::env::var("LLAMA_IDLE_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(models::DEFAULT_IDLE_TIMEOUT_SECS);

        log::info!(
            "SidecarManager initialized with idle timeout: {}s",
            idle_timeout_secs
        );
        log::info!("Helper binary path: {}", helper_binary_path.display());

        Ok(Self {
            child_process: Mutex::new(None),
            stdin_writer: Mutex::new(None),
            stdout_reader: Mutex::new(None),
            last_activity: RwLock::new(Instant::now()),
            is_healthy: AtomicBool::new(false),
            should_shutdown: AtomicBool::new(false),
            active_request_count: Arc::new(AtomicUsize::new(0)),
            request_lock: Mutex::new(()),
            spawn_generation: AtomicU64::new(0),
            helper_binary_path,
            current_model_path: RwLock::new(None),
            idle_timeout_secs,
        })
    }

    /// Resolve the path to llama-helper binary
    fn resolve_helper_binary() -> Result<PathBuf> {
        // 1. Check environment variable (dev mode or manual override)
        if let Ok(env_path) = std::env::var("PARLEY_LLAMA_HELPER") {
            if !env_path.is_empty() {
                let path = PathBuf::from(env_path);
                if path.exists() {
                    log::info!(
                        "Using llama-helper from PARLEY_LLAMA_HELPER: {}",
                        path.display()
                    );
                    return Ok(path);
                }
            }
        }

        // In production, Tauri bundles the binary with target triple suffix
        // 2. Check relative to current executable (most reliable for AppImage/bundled apps)
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                log::info!(
                    "Searching for llama-helper relative to executable: {}",
                    exe_dir.display()
                );

                // Get the target triple (same logic as before)
                let target_triple = std::env::var("TARGET").unwrap_or_else(|_| {
                    #[cfg(target_arch = "x86_64")]
                    {
                        "x86_64-unknown-linux-gnu".to_string()
                    }
                    #[cfg(target_arch = "aarch64")]
                    {
                        "aarch64-unknown-linux-gnu".to_string()
                    }
                    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                    {
                        "unknown".to_string()
                    }
                });

                let binary_name = format!("llama-helper-{}", target_triple);

                // Try exact match in exe dir
                let bundled = exe_dir.join(&binary_name);
                if bundled.exists() {
                    log::info!(
                        "Found exact match next to executable: {}",
                        bundled.display()
                    );
                    return Ok(bundled);
                }

                // Fuzzy match in exe dir
                log::info!("Attempting fuzzy match in exe dir: {}", exe_dir.display());
                if let Ok(entries) = std::fs::read_dir(exe_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if name.starts_with("llama-helper") && !name.ends_with(".d") {
                                log::info!(
                                    "Found fuzzy match next to executable: {}",
                                    path.display()
                                );
                                return Ok(path);
                            }
                        }
                    }
                }
            }
        }

        // 3. Check bundled resources (RESOURCE_DIR) - Fallback
        if let Ok(resource_dir) = std::env::var("RESOURCE_DIR") {
            log::info!(
                "Searching for llama-helper in RESOURCE_DIR: {}",
                resource_dir
            );
            let resource_path = PathBuf::from(&resource_dir);
            // Get the target triple again (or we could have shared it, but code duplication is safer for this tool usage)
            let target_triple = std::env::var("TARGET").unwrap_or_else(|_| {
                #[cfg(target_arch = "x86_64")]
                {
                    "x86_64-unknown-linux-gnu".to_string()
                }
                #[cfg(target_arch = "aarch64")]
                {
                    "aarch64-unknown-linux-gnu".to_string()
                }
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                {
                    "unknown".to_string()
                }
            });

            let binary_name = format!("llama-helper-{}", target_triple);

            let bundled = resource_path.join(&binary_name);
            if bundled.exists() {
                log::info!("Found exact match in RESOURCE_DIR: {}", bundled.display());
                return Ok(bundled);
            }

            // Fuzzy match in RESOURCE_DIR
            if let Ok(entries) = std::fs::read_dir(&resource_path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("llama-helper") && !name.ends_with(".d") {
                            log::info!("Found fuzzy match in RESOURCE_DIR: {}", path.display());
                            return Ok(path);
                        }
                    }
                }
            }
        } else {
            log::warn!("RESOURCE_DIR environment variable not set");
        }

        // 3. Fallback for dev: the workspace target dir (no target triple in
        //    dev builds). Walk up from the running crate's manifest dir, since
        //    the app crate may sit one level below the workspace root
        //    (parley-gpui/) or two (frontend/src-tauri/).
        if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
            for dir in PathBuf::from(&manifest_dir).ancestors() {
                for profile in ["release", "debug"] {
                    let candidate = dir.join("target").join(profile).join("llama-helper");
                    if candidate.exists() {
                        log::info!("Using dev llama-helper: {}", candidate.display());
                        return Ok(candidate);
                    }
                }
            }
        }

        Err(anyhow!(
            "llama-helper binary not found. Build with 'cd llama-helper && cargo build --release' or set PARLEY_LLAMA_HELPER env var."
        ))
    }

    /// Ensure sidecar is running, spawn if needed
    pub async fn ensure_running(this: &Arc<Self>, model_path: PathBuf) -> Result<()> {
        // Check if already running with correct model
        {
            let current_model = this.current_model_path.read().await;
            if current_model.as_ref() == Some(&model_path) && this.is_healthy() {
                log::debug!("Sidecar already running with correct model");
                this.update_activity().await;
                return Ok(());
            }
        }

        // Need to spawn or restart
        Self::spawn(this, model_path).await
    }

    /// Spawn the sidecar process
    async fn spawn(this: &Arc<Self>, model_path: PathBuf) -> Result<()> {
        // Shutdown existing process if running
        this.shutdown().await?;

        log::info!("Spawning llama-helper sidecar");
        log::info!("Model path: {}", model_path.display());

        let mut command = tokio::process::Command::new("nice");
        command.arg("-n").arg("10").arg(&this.helper_binary_path);

        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()); // Log stderr to main process

        let mut child = command.spawn().with_context(|| {
            format!(
                "Failed to spawn llama-helper at {:?}",
                this.helper_binary_path
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout"))?;

        // Store handles
        {
            let mut child_lock = this.child_process.lock().await;
            *child_lock = Some(child);
        }

        {
            let mut stdin_lock = this.stdin_writer.lock().await;
            *stdin_lock = Some(stdin);
        }

        {
            let mut stdout_lock = this.stdout_reader.lock().await;
            *stdout_lock = Some(BufReader::new(stdout));
        }

        // Update state
        {
            let mut current_model = this.current_model_path.write().await;
            *current_model = Some(model_path);
        }

        this.is_healthy.store(true, Ordering::SeqCst);
        this.should_shutdown.store(false, Ordering::SeqCst);
        this.update_activity().await;

        log::info!("Sidecar spawned successfully");

        // Start background tasks scoped to this spawn — bumping the
        // generation retires the previous spawn's loops.
        let generation = this.spawn_generation.fetch_add(1, Ordering::SeqCst) + 1;
        Self::start_health_check_loop(this, generation);
        Self::start_idle_check_loop(this, generation);

        Ok(())
    }

    /// Send a request to the sidecar and wait for its terminal response.
    ///
    /// `Chunk` lines streamed before the terminal line are forwarded to
    /// `on_chunk` (when provided) and never returned to the caller — the
    /// terminal `Response` carries the complete final text. `timeout` bounds
    /// the whole exchange, chunks included.
    pub async fn send_request(
        &self,
        request: &Request,
        timeout: Duration,
        on_chunk: Option<&(dyn Fn(&str) + Send + Sync)>,
    ) -> Result<Response> {
        let request_json = serde_json::to_string(request)?;

        // Track active request
        let _guard = RequestGuard::new(self.active_request_count.clone());

        // Hold the request lock across the full write+read round trip so no
        // other request (or the health-check ping) can write to stdin or
        // read from stdout in between and end up matched to the wrong
        // response line.
        let _req_lock = self.request_lock.lock().await;

        // Write request to stdin
        {
            let mut stdin_lock = self.stdin_writer.lock().await;
            let stdin = stdin_lock
                .as_mut()
                .ok_or_else(|| anyhow!("Sidecar not running"))?;

            stdin
                .write_all(request_json.as_bytes())
                .await
                .context("Failed to write request to stdin")?;
            stdin
                .write_all(b"\n")
                .await
                .context("Failed to write newline")?;
            stdin.flush().await.context("Failed to flush stdin")?;
        }

        // Read lines until the terminal response, all within one deadline.
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let line = match tokio::time::timeout_at(deadline, self.read_response()).await {
                Ok(Ok(line)) => line,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    // Timeout reached - shutdown sidecar to stop generation
                    log::error!("Request timeout after {:?}, shutting down sidecar", timeout);
                    if let Err(shutdown_err) = self.shutdown().await {
                        log::error!(
                            "Failed to shutdown sidecar after timeout: {}",
                            shutdown_err
                        );
                    }
                    return Err(anyhow!("Request timed out after {:?}", timeout));
                }
            };

            match serde_json::from_str::<Response>(&line) {
                Ok(Response::Chunk { text }) => {
                    if let Some(callback) = on_chunk {
                        callback(&text);
                    }
                }
                Ok(terminal) => {
                    self.update_activity().await;
                    return Ok(terminal);
                }
                Err(e) => {
                    return Err(anyhow!(
                        "Unparseable sidecar response: {} ({})",
                        line,
                        e
                    ))
                }
            }
        }
    }

    /// Read a single line response from stdout
    async fn read_response(&self) -> Result<String> {
        let mut stdout_lock = self.stdout_reader.lock().await;
        let reader = stdout_lock
            .as_mut()
            .ok_or_else(|| anyhow!("Sidecar not running"))?;

        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("Failed to read response from stdout")?;

        if line.is_empty() {
            return Err(anyhow!("Sidecar closed stdout (process may have crashed)"));
        }

        Ok(line.trim().to_string())
    }

    /// Send ping to keep sidecar alive
    async fn send_ping(&self) -> Result<()> {
        // Try to grab the same lock `send_request` uses for its write+read
        // round trip, without blocking. If a real request is in flight (or
        // another ping already holds it), skip this cycle instead of
        // waiting behind a possibly long-running generation — and, more
        // importantly, instead of racing a real request for the same
        // stdin/stdout pair, which could otherwise read back the wrong
        // response (e.g. a generation result mistaken for a pong, or vice
        // versa). The next health-check tick will simply try again.
        let _req_lock = match self.request_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                log::debug!("Health check: sidecar busy, skipping ping this cycle");
                return Ok(());
            }
        };

        let request = serde_json::to_string(&Request::Ping)?;
        let timeout = Duration::from_secs(5);

        // Note: We don't use send_request here to avoid incrementing active_request_count
        // for internal health checks, as that would prevent graceful shutdown

        // Write request
        {
            let mut stdin_lock = self.stdin_writer.lock().await;
            if let Some(stdin) = stdin_lock.as_mut() {
                stdin.write_all(request.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            } else {
                return Err(anyhow!("Sidecar not running"));
            }
        }

        // Read response
        let response = tokio::time::timeout(timeout, self.read_response()).await??;

        match serde_json::from_str::<Response>(&response)? {
            Response::Pong => Ok(()),
            other => Err(anyhow!("Unexpected ping response: {:?}", other)),
        }
    }

    /// Gracefully shutdown the sidecar
    /// Waits for active requests to complete before killing the process
    pub async fn shutdown_gracefully(&self) -> Result<()> {
        log::info!("Initiating graceful shutdown of sidecar");

        // Set shutdown flag to prevent new internal tasks
        self.should_shutdown.store(true, Ordering::SeqCst);

        // Wait for active requests to complete
        // We poll every 500ms
        let start = Instant::now();
        let max_wait = Duration::from_secs(600); // Wait up to 10 minutes for long generations

        loop {
            let count = self.active_request_count.load(Ordering::SeqCst);
            if count == 0 {
                log::info!("No active requests, proceeding with shutdown");
                break;
            }

            if start.elapsed() > max_wait {
                log::warn!(
                    "Timed out waiting for active requests ({} active), forcing shutdown",
                    count
                );
                break;
            }

            log::debug!("Waiting for {} active requests to complete...", count);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        self.shutdown().await
    }

    /// Force shutdown the sidecar
    pub async fn shutdown(&self) -> Result<()> {
        // Set shutdown flag
        self.should_shutdown.store(true, Ordering::SeqCst);

        // Send shutdown command
        if self.is_healthy() {
            let request = serde_json::to_string(&Request::Shutdown)?;

            // Try to send shutdown command, but ignore errors
            // We don't use send_request to avoid incrementing counter
            let _ = async {
                let mut stdin_lock = self.stdin_writer.lock().await;
                if let Some(stdin) = stdin_lock.as_mut() {
                    stdin.write_all(request.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
        }

        // Kill process if still running
        {
            let mut child_lock = self.child_process.lock().await;
            if let Some(mut child) = child_lock.take() {
                match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                    Ok(Ok(status)) => {
                        log::info!("Sidecar exited with status: {}", status);
                    }
                    Ok(Err(e)) => {
                        log::error!("Failed to wait for sidecar: {}", e);
                    }
                    Err(_) => {
                        log::warn!("Sidecar didn't exit gracefully, killing");
                        let _ = child.kill().await;
                    }
                }
            }
        }

        // Clear handles
        {
            let mut stdin_lock = self.stdin_writer.lock().await;
            *stdin_lock = None;
        }

        {
            let mut stdout_lock = self.stdout_reader.lock().await;
            *stdout_lock = None;
        }

        {
            let mut current_model = self.current_model_path.write().await;
            *current_model = None;
        }

        self.is_healthy.store(false, Ordering::SeqCst);

        log::info!("Sidecar shutdown complete");
        Ok(())
    }

    /// Check if sidecar is healthy
    pub fn is_healthy(&self) -> bool {
        self.is_healthy.load(Ordering::SeqCst)
    }

    /// Update last activity timestamp
    async fn update_activity(&self) {
        let mut last_activity = self.last_activity.write().await;
        *last_activity = Instant::now();
    }

    /// Get seconds since last activity
    async fn seconds_since_activity(&self) -> u64 {
        let last_activity = self.last_activity.read().await;
        last_activity.elapsed().as_secs()
    }

    /// Whether a background loop started for `generation` should keep
    /// running: exits on shutdown, or when a newer spawn has taken over.
    fn loop_is_current(&self, generation: u64) -> bool {
        !self.should_shutdown.load(Ordering::SeqCst)
            && self.spawn_generation.load(Ordering::SeqCst) == generation
    }

    /// Start health check loop for one spawn (runs in background)
    fn start_health_check_loop(this: &Arc<Self>, generation: u64) {
        let manager = Arc::clone(this);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                if !manager.loop_is_current(generation) {
                    break;
                }

                if !manager.is_healthy() {
                    log::debug!("Health check loop: sidecar unhealthy, skipping ping");
                    continue;
                }

                // Don't ping if we are busy with a request. This is just a
                // cheap pre-filter — send_ping() also try-locks request_lock
                // so any race left over from this check is handled safely.
                if manager.active_request_count.load(Ordering::SeqCst) > 0 {
                    continue;
                }

                log::debug!("Health check: sending ping");
                if let Err(e) = manager.send_ping().await {
                    log::warn!("Health check failed: {}", e);
                    manager.is_healthy.store(false, Ordering::SeqCst);
                }
            }

            log::debug!("Health check loop exited (generation {})", generation);
        });
    }

    /// Start idle check loop for one spawn (runs in background)
    fn start_idle_check_loop(this: &Arc<Self>, generation: u64) {
        let manager = Arc::clone(this);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                if !manager.loop_is_current(generation) {
                    break;
                }

                // Don't shutdown if we are busy
                if manager.active_request_count.load(Ordering::SeqCst) > 0 {
                    // Update activity to prevent timeout immediately after request finishes
                    manager.update_activity().await;
                    continue;
                }

                let idle_secs = manager.seconds_since_activity().await;
                log::debug!("Idle check: {}s since last activity", idle_secs);

                if idle_secs > manager.idle_timeout_secs {
                    log::info!(
                        "Sidecar idle for {}s (timeout: {}s), shutting down",
                        idle_secs,
                        manager.idle_timeout_secs
                    );

                    if let Err(e) = manager.shutdown().await {
                        log::error!("Failed to shutdown idle sidecar: {}", e);
                    }

                    break;
                }
            }

            log::debug!("Idle check loop exited (generation {})", generation);
        });
    }
}

impl Drop for SidecarManager {
    fn drop(&mut self) {
        // Set shutdown flag
        self.should_shutdown.store(true, Ordering::SeqCst);

        // Note: Actual cleanup happens in shutdown() method
        // We can't do async work in Drop, so this is best-effort
        log::debug!("SidecarManager dropped");
    }
}
