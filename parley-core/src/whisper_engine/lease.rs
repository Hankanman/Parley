// whisper_engine/lease.rs
//
// A "live transcription lease" on the shared Whisper engine.
//
// The engine (WHISPER_ENGINE, see commands.rs) is a single global instance
// shared between the live-recording transcription path and background batch
// jobs (audio import, retranscription / auto-refine). Batch jobs call
// `load_model` / `unload_model` on that same instance to switch to a
// different-accuracy model for the batch, or to free memory once done.
//
// `load_model` unloads whatever is currently loaded *before* loading the
// replacement (see `WhisperEngine::load_model`), so there is a multi-second
// window where `is_model_loaded()` is false. If a live recording's worker
// task is draining chunks through that window, every chunk is silently
// skipped (see `audio::transcription::worker::process_chunk`), and once the
// swap completes the live session keeps running on whatever model the batch
// job picked instead of the one it started with.
//
// This lease closes that window: the live worker task holds it for its
// entire lifetime (from the moment it starts, until its receive loop has
// fully drained and it is about to exit), and every call site that would
// change the loaded model (batch `load_model`, and `unload_engine_after_batch`)
// must check/wait on it first. See `audio/common.rs`, `audio/import.rs` and
// `audio/retranscription.rs` for the call sites.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Global live-transcription lease on the shared Whisper engine. A plain
/// counter (not a bool) so the live final-transcription worker and the
/// partial/streaming-preview worker can each hold their own guard
/// independently without racing to clear each other's hold.
pub struct EngineLease {
    count: AtomicUsize,
    notify: Notify,
}

impl EngineLease {
    const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            notify: Notify::const_new(),
        }
    }

    /// Take the lease for the lifetime of a live-recording consumer. Hold
    /// the returned guard for as long as that consumer may touch the
    /// engine; dropping it releases the lease and wakes any batch job
    /// waiting in `wait_until_free`.
    pub fn acquire_live(&'static self) -> EngineLeaseGuard {
        let previous = self.count.fetch_add(1, Ordering::SeqCst);
        log::debug!(
            "EngineLease: acquired (live holders now {})",
            previous + 1
        );
        EngineLeaseGuard { lease: self }
    }

    /// True while at least one live-recording consumer holds the lease.
    pub fn is_live_leased(&self) -> bool {
        self.count.load(Ordering::SeqCst) > 0
    }

    /// Wait until no live-recording consumer holds the lease, or `timeout`
    /// elapses — whichever comes first. Returns `true` if the lease is free,
    /// `false` on timeout (still held). Intended for background batch jobs
    /// (import / retranscription / auto-refine), which can afford to wait a
    /// long time rather than risk swapping the model out from under a live
    /// recording; callers should pass a generous timeout (e.g. 30 minutes)
    /// and log/handle the `false` case rather than proceeding to change the
    /// loaded model.
    pub async fn wait_until_free(&'static self, timeout: Duration) -> bool {
        if !self.is_live_leased() {
            return true;
        }
        log::info!(
            "EngineLease: waiting up to {:?} for live recording to release the Whisper engine",
            timeout
        );
        let deadline = Instant::now() + timeout;
        loop {
            // Register interest before re-checking the condition so a
            // release that happens between the check and the await can't be
            // missed (Notify's documented wait pattern).
            let notified = self.notify.notified();
            if !self.is_live_leased() {
                return true;
            }
            tokio::select! {
                _ = notified => {
                    if !self.is_live_leased() {
                        return true;
                    }
                    // Spurious wake (e.g. one of several live holders
                    // dropped while others remain) — loop and re-check.
                }
                _ = tokio::time::sleep_until(deadline) => {
                    log::warn!(
                        "EngineLease: timed out after {:?} still waiting for live recording to release the engine",
                        timeout
                    );
                    return false;
                }
            }
            if Instant::now() >= deadline {
                log::warn!(
                    "EngineLease: timed out after {:?} still waiting for live recording to release the engine",
                    timeout
                );
                return false;
            }
        }
    }
}

/// RAII handle for a held [`EngineLease`]. Drop releases the lease.
pub struct EngineLeaseGuard {
    lease: &'static EngineLease,
}

impl Drop for EngineLeaseGuard {
    fn drop(&mut self) {
        let previous = self.lease.count.fetch_sub(1, Ordering::SeqCst);
        log::debug!("EngineLease: released (live holders now {})", previous - 1);
        if previous == 1 {
            self.lease.notify.notify_waiters();
        }
    }
}

/// The single shared lease guarding the global Whisper engine's loaded model.
pub static LIVE_ENGINE_LEASE: EngineLease = EngineLease::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_sets_leased_drop_clears_it() {
        // A dedicated static so this test can't interact with other tests
        // (or other code in the same process) sharing LIVE_ENGINE_LEASE.
        static LEASE: EngineLease = EngineLease::new();

        assert!(!LEASE.is_live_leased());
        let guard = LEASE.acquire_live();
        assert!(LEASE.is_live_leased());
        drop(guard);
        assert!(!LEASE.is_live_leased());
    }

    #[tokio::test]
    async fn wait_until_free_resolves_after_drop() {
        static LEASE: EngineLease = EngineLease::new();

        let guard = LEASE.acquire_live();
        assert!(LEASE.is_live_leased());

        let wait_task = tokio::spawn(async {
            LEASE.wait_until_free(Duration::from_secs(5)).await
        });

        // Give the waiter a moment to start waiting, then release.
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(guard);

        let freed = wait_task.await.expect("wait task panicked");
        assert!(freed, "wait_until_free should report the lease as free");
        assert!(!LEASE.is_live_leased());
    }

    #[tokio::test]
    async fn wait_until_free_times_out_while_held() {
        // No `tokio` "test-util" feature is enabled in this crate (only
        // "full"), so this uses a real, short timeout rather than
        // `start_paused` virtual time.
        static LEASE: EngineLease = EngineLease::new();

        let _guard = LEASE.acquire_live();
        let freed = LEASE.wait_until_free(Duration::from_millis(50)).await;
        assert!(!freed, "wait_until_free should time out while the lease is held");
    }
}
