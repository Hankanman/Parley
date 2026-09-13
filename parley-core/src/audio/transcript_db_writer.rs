// audio/transcript_db_writer.rs
//
// Issue #57 slice 2: persist live-recording transcript segments to SQLite as
// they arrive, instead of the frontend bulk-inserting the whole meeting once
// at the end. The `transcript-update` listener in `recording_commands.rs`
// already stores every segment in `RecordingSaver` (and, via the journal
// writer, on disk) synchronously; this module gives it a second, equally
// cheap, non-blocking place to hand the same segment off to — a channel
// send — so the listener itself never touches the database directly.
//
// A dedicated task owns the actual writes: it batches whatever arrives on
// the channel and flushes to `TranscriptsRepository::upsert_transcript_segments`
// either once 20 segments have queued up or every second, whichever comes
// first. Dropping the `TranscriptDbWriter` (its `Sender`) closes the
// channel; the task then flushes anything left and returns, which is what
// `stop_recording` waits on to guarantee every segment from the session is
// written before it moves on.

use log::{info, warn};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::audio::common::TranscriptSegment;
use crate::database::repositories::transcript::TranscriptsRepository;

/// Flush once this many segments have queued up, even if the interval
/// hasn't ticked yet.
const BATCH_SIZE: usize = 20;
/// Otherwise flush at most this often.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Handle held by the command layer for the lifetime of one recording
/// session. `enqueue` is a plain unbounded-channel send — cheap enough to
/// call directly from the synchronous `transcript-update` event listener.
pub struct TranscriptDbWriter {
    sender: mpsc::UnboundedSender<(String, TranscriptSegment)>,
}

impl TranscriptDbWriter {
    /// Spawn the writer task against `pool` and return a handle to enqueue
    /// segments plus the task's `JoinHandle` (for the caller to await at
    /// stop, after dropping the handle to close the channel).
    pub fn start(pool: SqlitePool) -> (Self, JoinHandle<()>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_writer(pool, receiver));
        (Self { sender }, task)
    }

    /// Queue `segment` for `meeting_id` to be written on the next flush.
    /// Never blocks; silently drops (with a warning) if the writer task has
    /// already ended — the transcripts.ndjson journal is still the
    /// authoritative record for the session in that case.
    pub fn enqueue(&self, meeting_id: String, segment: TranscriptSegment) {
        if self.sender.send((meeting_id, segment)).is_err() {
            warn!("Transcript DB writer task is gone; a segment was not queued for SQLite");
        }
    }
}

async fn run_writer(
    pool: SqlitePool,
    mut receiver: mpsc::UnboundedReceiver<(String, TranscriptSegment)>,
) {
    let mut batch: Vec<(String, TranscriptSegment)> = Vec::with_capacity(BATCH_SIZE);
    let mut ticker = interval(FLUSH_INTERVAL);
    // The first tick fires immediately; skip it so an idle recording
    // doesn't run a pointless empty-batch flush right at startup, and
    // catch-up ticks after a slow flush don't pile up.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            maybe_item = receiver.recv() => {
                match maybe_item {
                    Some(item) => {
                        batch.push(item);
                        if batch.len() >= BATCH_SIZE {
                            flush(&pool, &mut batch).await;
                        }
                    }
                    None => {
                        // Sender dropped (recording stopped): flush whatever
                        // is left and end the task.
                        flush(&pool, &mut batch).await;
                        info!("Transcript DB writer task ending (channel closed)");
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                flush(&pool, &mut batch).await;
            }
        }
    }
}

async fn flush(pool: &SqlitePool, batch: &mut Vec<(String, TranscriptSegment)>) {
    if batch.is_empty() {
        return;
    }
    let items = std::mem::take(batch);
    let count = items.len();
    if let Err(e) = TranscriptsRepository::upsert_transcript_segments(pool, &items).await {
        warn!(
            "Failed to write a batch of {} transcript segment(s) to SQLite: {} (transcripts.ndjson on disk is unaffected)",
            count, e
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::meeting::MeetingsRepository;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    fn segment(sequence_id: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            id: format!("seg_{}", sequence_id),
            text: text.to_string(),
            timestamp: None,
            audio_start_time: Some(sequence_id as f64),
            audio_end_time: Some(sequence_id as f64 + 1.0),
            duration: Some(1.0),
            display_time: None,
            confidence: None,
            sequence_id: Some(sequence_id),
            speaker: None,
            voice_profile_id: None,
            source: Some("mic".to_string()),
        }
    }

    /// Two segments below `BATCH_SIZE`, sent then the channel closed
    /// immediately: the writer task must still flush them on shutdown
    /// rather than dropping whatever hadn't reached a full batch.
    #[tokio::test]
    async fn flushes_remaining_segments_when_channel_closes() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "meeting-1", "Test", None)
            .await
            .unwrap();

        let (writer, task) = TranscriptDbWriter::start(pool.clone());
        writer.enqueue("meeting-1".to_string(), segment(1, "hello"));
        writer.enqueue("meeting-1".to_string(), segment(2, "world"));
        drop(writer); // closes the channel

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("writer task should finish promptly")
            .expect("writer task should not panic");

        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT transcript FROM transcripts WHERE meeting_id = ? ORDER BY sequence_id")
                .bind("meeting-1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![("hello".to_string(),), ("world".to_string(),)]);
    }

    /// A later segment with the same `sequence_id` (the transcription
    /// worker revising a partial into its final text) must update the
    /// existing row in place, not insert a duplicate.
    #[tokio::test]
    async fn upserts_by_sequence_id_instead_of_duplicating() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "meeting-2", "Test", None)
            .await
            .unwrap();

        let (writer, task) = TranscriptDbWriter::start(pool.clone());
        writer.enqueue("meeting-2".to_string(), segment(1, "partial text"));
        writer.enqueue("meeting-2".to_string(), segment(1, "final text"));
        drop(writer);

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("writer task should finish promptly")
            .expect("writer task should not panic");

        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT transcript FROM transcripts WHERE meeting_id = ?")
                .bind("meeting-2")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![("final text".to_string(),)]);
    }

    /// A batch that reaches `BATCH_SIZE` flushes on its own, without
    /// waiting for the channel to close or the interval to tick.
    #[tokio::test]
    async fn flushes_once_batch_size_is_reached() {
        let pool = test_pool().await;
        MeetingsRepository::create_recording_meeting(&pool, "meeting-3", "Test", None)
            .await
            .unwrap();

        let (writer, task) = TranscriptDbWriter::start(pool.clone());
        for i in 0..BATCH_SIZE as u64 {
            writer.enqueue("meeting-3".to_string(), segment(i, "segment"));
        }

        // Give the task a moment to process the size-triggered flush without
        // relying on the 1s interval tick or channel closure.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let count: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM transcripts WHERE meeting_id = ?")
                    .bind("meeting-3")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if count.0 == BATCH_SIZE as i64 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "size-triggered flush did not happen in time"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
}
