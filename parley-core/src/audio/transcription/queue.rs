// audio/transcription/queue.rs
//
// Backlog accounting for the pipeline -> transcription-worker channel
// (issue #26: the transcription queue was unbounded with zero visibility
// into how far behind real time Whisper had fallen).
//
// `pipeline.rs::dispatch_segments` sends into an `UnboundedSender<AudioChunk>`
// it already owns (`AudioPipeline::transcription_sender`). To add depth
// accounting without touching pipeline.rs, `recording_manager.rs::start_recording`
// hands the pipeline a plain, uninstrumented sender as before, and inserts a
// small forwarding task between the raw receiver and the worker:
//
//   pipeline --(raw tx)--> [ spawn_counting_forwarder ] --(counted tx)--> worker
//
// The forwarder increments `QUEUE_DEPTH` as it forwards each segment; the
// worker (`worker.rs::start_transcription_task`) calls `dequeued()` after
// every successful `recv()`. When the pipeline drops the raw sender (stop),
// the forwarder's `recv()` returns `None` and it drops the counted sender in
// turn, so the worker's own "channel closed" completion signal is preserved
// exactly as before.

use log::warn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::audio::AudioChunk;

/// Segments enqueued for transcription but not yet picked up by the worker.
static QUEUE_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Current transcription backlog depth.
pub fn queue_depth() -> usize {
    QUEUE_DEPTH.load(Ordering::SeqCst)
}

/// Reset the backlog counter. Call at the start of every recording so a
/// prior session can never leak a stale depth into a new one.
pub fn reset_queue_depth() {
    QUEUE_DEPTH.store(0, Ordering::SeqCst);
}

/// Log a warning once the backlog passes this many queued segments.
const BACKLOG_WARN_THRESHOLD: usize = 10;
/// Rate-limit the backlog warning so a sustained backlog doesn't spam logs.
const WARN_INTERVAL: Duration = Duration::from_secs(30);

static LAST_WARN: Mutex<Option<Instant>> = Mutex::new(None);

fn maybe_warn_backlog(depth: usize) {
    if depth <= BACKLOG_WARN_THRESHOLD {
        return;
    }
    let mut last = LAST_WARN.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let should_warn = last.map_or(true, |t| now.duration_since(t) >= WARN_INTERVAL);
    if should_warn {
        *last = Some(now);
        warn!(
            "⚠️ Transcription queue backlog: {} segments queued — Whisper is falling behind real time",
            depth
        );
    }
}

/// Called by the transcription worker once per segment successfully
/// received off the counted channel, to reflect it leaving the queue.
/// Saturating: never underflows even if called more times than segments
/// were enqueued (shouldn't happen, but a metric must never panic).
pub fn dequeued() {
    let _ = QUEUE_DEPTH.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |d| {
        Some(d.saturating_sub(1))
    });
}

/// Spawn the counting forwarder described above. `raw_rx` is the receiver
/// half handed to the pipeline's sender; the returned receiver is what the
/// transcription worker should consume instead.
pub fn spawn_counting_forwarder(
    mut raw_rx: mpsc::UnboundedReceiver<AudioChunk>,
) -> mpsc::UnboundedReceiver<AudioChunk> {
    let (counted_tx, counted_rx) = mpsc::unbounded_channel::<AudioChunk>();
    // Dropping the JoinHandle detaches the task (tokio's default) — it keeps
    // running until `raw_rx` closes, same as any other fire-and-forget task
    // in this module.
    let _handle = tokio::spawn(async move {
        while let Some(chunk) = raw_rx.recv().await {
            let depth = QUEUE_DEPTH.fetch_add(1, Ordering::SeqCst) + 1;
            maybe_warn_backlog(depth);
            if counted_tx.send(chunk).is_err() {
                // Worker side is gone (e.g. aborted) — stop forwarding.
                break;
            }
        }
        // `raw_rx` closed (pipeline dropped its sender) or the worker is
        // gone: `counted_tx` is dropped here either way, which is what lets
        // the worker's `recv() == None` completion path fire.
    });
    counted_rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::recording_state::DeviceType;

    /// `QUEUE_DEPTH` is process-global, so these tests must not interleave:
    /// one test's `reset_queue_depth()` or `dequeued()` would otherwise land
    /// in the middle of another's send/receive sequence.
    static QUEUE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn sample_chunk(id: u64) -> AudioChunk {
        AudioChunk {
            data: vec![0.0; 1600],
            sample_rate: 16000,
            timestamp: 0.0,
            chunk_id: id,
            device_type: DeviceType::Microphone,
        }
    }

    #[tokio::test]
    async fn depth_increments_on_send_and_decrements_on_dequeue() {
        let _serial = QUEUE_TEST_LOCK.lock().await;
        reset_queue_depth();
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<AudioChunk>();
        let mut counted_rx = spawn_counting_forwarder(raw_rx);

        raw_tx.send(sample_chunk(1)).unwrap();
        raw_tx.send(sample_chunk(2)).unwrap();

        // Give the forwarder task a chance to run.
        let first = counted_rx.recv().await.expect("first chunk");
        assert_eq!(first.chunk_id, 1);
        // At least one segment has been forwarded and not yet dequeued.
        assert!(queue_depth() >= 1);

        dequeued();
        let second = counted_rx.recv().await.expect("second chunk");
        assert_eq!(second.chunk_id, 2);
        dequeued();

        assert_eq!(queue_depth(), 0);
    }

    #[tokio::test]
    async fn completion_signal_survives_the_forwarder() {
        let _serial = QUEUE_TEST_LOCK.lock().await;
        reset_queue_depth();
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<AudioChunk>();
        let mut counted_rx = spawn_counting_forwarder(raw_rx);

        raw_tx.send(sample_chunk(1)).unwrap();
        drop(raw_tx); // simulate the pipeline dropping its sender at stop

        let chunk = counted_rx.recv().await.expect("queued chunk still delivered");
        assert_eq!(chunk.chunk_id, 1);
        dequeued();

        // Forwarder should end and drop counted_tx, so recv() now yields None.
        assert!(counted_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn dequeue_never_underflows() {
        let _serial = QUEUE_TEST_LOCK.lock().await;
        reset_queue_depth();
        dequeued();
        dequeued();
        assert_eq!(queue_depth(), 0);
    }
}
