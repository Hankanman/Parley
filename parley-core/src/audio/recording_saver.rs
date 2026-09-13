use anyhow::Result;
use log::{error, info, warn};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

use super::audio_processing::create_meeting_folder;
use super::common::{
    self, write_metadata as common_write_metadata, write_transcripts_json as common_write_transcripts_json,
    DeviceInfo, MeetingMetadata, TRANSCRIPTS_JOURNAL_FILENAME,
};
use super::incremental_saver::IncrementalAudioSaver;
use super::recording_state::AudioChunk;
use crate::events::EventSinkExt;

/// Canonical transcript segment type — re-exported here for compatibility
/// with the many call sites that already `use
/// crate::audio::recording_saver::TranscriptSegment` (the live-recording
/// path's historical name for it). See `audio::common::TranscriptSegment`
/// for the type itself.
pub use super::common::TranscriptSegment;

/// New recording saver using incremental saving strategy
pub struct RecordingSaver {
    incremental_saver: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    meeting_folder: Option<PathBuf>,
    meeting_name: Option<String>,
    /// The `meetings` row id for this session (issue #57 slice 2). Minted
    /// by `recording_commands::start_recording*` before the manager is
    /// stored globally, so it's available for the whole session — used to
    /// key live transcript upserts and to finalise the row at stop.
    meeting_id: Option<String>,
    metadata: Option<MeetingMetadata>,
    transcript_segments: Arc<Mutex<Vec<TranscriptSegment>>>,
    chunk_receiver: Option<mpsc::UnboundedReceiver<AudioChunk>>,
    is_saving: Arc<Mutex<bool>>,
    // Handle to the accumulation task spawned in `start_accumulation`, so
    // `stop_and_save` can wait for it to fully drain the channel (and write
    // every already-queued chunk) instead of racing it with a fixed sleep.
    accumulation_task: Option<tokio::task::JoinHandle<()>>,
    // Sender side of the dedicated transcript journal writer task (issue
    // #48). `add_transcript_segment` pushes every segment here instead of
    // synchronously rewriting the whole transcripts.json on every call; the
    // writer task appends it to transcripts.ndjson and periodically rebuilds
    // transcripts.json in the background. `None` before `start_accumulation`
    // sets up a meeting folder, and set back to `None` (closing the channel)
    // once the writer task is stopped in `stop_and_save`/`discard_empty_session`.
    journal_sender: Option<mpsc::UnboundedSender<TranscriptSegment>>,
    // Handle to the journal writer task, joined (stop_and_save) or aborted
    // (discard_empty_session) alongside dropping `journal_sender`.
    journal_task: Option<tokio::task::JoinHandle<()>>,
}

impl RecordingSaver {
    pub fn new() -> Self {
        Self {
            incremental_saver: None,
            meeting_folder: None,
            meeting_name: None,
            meeting_id: None,
            metadata: None,
            transcript_segments: Arc::new(Mutex::new(Vec::new())),
            chunk_receiver: None,
            is_saving: Arc::new(Mutex::new(false)),
            accumulation_task: None,
            journal_sender: None,
            journal_task: None,
        }
    }

    /// Set the meeting name for this recording session
    pub fn set_meeting_name(&mut self, name: Option<String>) {
        self.meeting_name = name;
    }

    /// Set the `meetings` row id for this recording session (issue #57
    /// slice 2). Set once, right after the id is minted in
    /// `recording_commands::start_recording*`.
    pub fn set_meeting_id(&mut self, id: Option<String>) {
        self.meeting_id = id;
    }

    /// Get the `meetings` row id for this recording session, if set.
    pub fn get_meeting_id(&self) -> Option<String> {
        self.meeting_id.clone()
    }

    /// Set device information in metadata
    pub fn set_device_info(&mut self, mic_name: Option<String>, sys_name: Option<String>) {
        if let Some(ref mut metadata) = self.metadata {
            metadata.devices = Some(DeviceInfo {
                microphone: mic_name,
                system_audio: sys_name,
            });

            // Write updated metadata to disk if folder exists
            if let Some(folder) = &self.meeting_folder {
                let metadata_clone = metadata.clone();
                if let Err(e) = self.write_metadata(folder, &metadata_clone) {
                    warn!("Failed to update metadata with device info: {}", e);
                }
            }
        }
    }

    /// Add or update a structured transcript segment (upserts based on sequence_id)
    /// Also saves incrementally to disk
    pub fn add_transcript_segment(&self, segment: TranscriptSegment) {
        if let Ok(mut segments) = self.transcript_segments.lock() {
            // Check if segment with same sequence_id exists (update it)
            if let Some(existing) = segments
                .iter_mut()
                .find(|s| s.sequence_id == segment.sequence_id)
            {
                *existing = segment.clone();
                info!(
                    "Updated transcript segment {} (seq: {:?}) - total segments: {}",
                    segment.id,
                    segment.sequence_id,
                    segments.len()
                );
            } else {
                // New segment, add it
                segments.push(segment.clone());
                info!(
                    "Added new transcript segment {} (seq: {:?}) - total segments: {}",
                    segment.id,
                    segment.sequence_id,
                    segments.len()
                );
            }
        } else {
            error!(
                "Failed to lock transcript segments for adding segment {}",
                segment.id
            );
        }

        // Persist incrementally via the dedicated journal writer task
        // (issue #48) rather than synchronously re-serialising and
        // rewriting the whole transcripts.json here on every segment: this
        // is a cheap, non-blocking channel send, and it's the writer task
        // that appends the line to transcripts.ndjson and periodically (at
        // most every 5s while dirty) rebuilds transcripts.json in the
        // background.
        if let Some(sender) = &self.journal_sender {
            if sender.send(segment.clone()).is_err() {
                warn!(
                    "Transcript journal writer task is gone; segment {} not persisted to disk",
                    segment.id
                );
            }
        } else if self.meeting_folder.is_some() {
            warn!(
                "No transcript journal writer available; segment {} not persisted to disk",
                segment.id
            );
        }
    }

    /// Legacy method for backward compatibility - converts text to basic segment
    pub fn add_transcript_chunk(&self, text: String) {
        let segment = TranscriptSegment {
            id: format!("seg_{}", chrono::Utc::now().timestamp_millis()),
            text,
            timestamp: None,
            audio_start_time: Some(0.0),
            audio_end_time: Some(0.0),
            duration: Some(0.0),
            display_time: Some("[00:00]".to_string()),
            confidence: Some(1.0),
            sequence_id: Some(0),
            speaker: None,
            voice_profile_id: None,
            source: None,
        };
        self.add_transcript_segment(segment);
    }

    /// Start accumulation with optional incremental saving
    ///
    /// # Arguments
    /// * `auto_save` - If true, creates checkpoints and enables saving. If false, audio chunks are discarded.
    pub fn start_accumulation(&mut self, auto_save: bool) -> mpsc::UnboundedSender<AudioChunk> {
        if auto_save {
            info!("Initializing incremental audio saver for recording (auto-save ENABLED)");
        } else {
            info!(
                "Starting recording without audio saving (auto-save DISABLED - transcripts only)"
            );
        }

        // Create channel for receiving audio chunks
        let (sender, receiver) = mpsc::unbounded_channel::<AudioChunk>();
        self.chunk_receiver = Some(receiver);

        // Initialize meeting folder and incremental saver ONLY if auto_save is enabled
        if auto_save {
            if let Some(name) = self.meeting_name.clone() {
                match self.initialize_meeting_folder(&name, true) {
                    Ok(()) => info!("Successfully initialized meeting folder with checkpoints"),
                    Err(e) => {
                        error!("Failed to initialize meeting folder: {}", e);
                        // Continue anyway - will use fallback flat structure
                    }
                }
            }
        } else {
            // When auto_save is false, still create meeting folder for transcripts/metadata
            // but skip .checkpoints directory
            if let Some(name) = self.meeting_name.clone() {
                match self.initialize_meeting_folder(&name, false) {
                    Ok(()) => info!("Successfully initialized meeting folder (transcripts only)"),
                    Err(e) => {
                        error!("Failed to initialize meeting folder: {}", e);
                    }
                }
            }
        }

        // Start accumulation task
        let incremental_saver_arc = self.incremental_saver.clone();
        let save_audio = auto_save;

        if let Some(mut receiver) = self.chunk_receiver.take() {
            let handle = tokio::spawn(async move {
                info!(
                    "Recording saver accumulation task started (save_audio: {})",
                    save_audio
                );

                // Run until the channel closes and drains, not until some external
                // "stop" flag flips. This guarantees every chunk already queued by
                // the pipeline (including trailing chunks sent right before the
                // pipeline shuts down) gets written before the task ends - the
                // sender side is dropped only once the pipeline itself is fully
                // stopped, so recv() naturally returns None only after that.
                while let Some(chunk) = receiver.recv().await {
                    // Only process audio chunks if auto_save is enabled
                    if save_audio {
                        // Add chunk to incremental saver
                        if let Some(saver_arc) = &incremental_saver_arc {
                            let mut saver_guard = saver_arc.lock().await;
                            if let Err(e) = saver_guard.add_chunk(chunk) {
                                error!("Failed to add chunk to incremental saver: {}", e);
                            }
                        } else {
                            error!("Incremental saver not available while accumulating");
                        }
                    } else {
                        // auto_save is false: discard audio chunk (no-op)
                        // Transcription already happened in the pipeline before this point
                    }
                }

                info!("Recording saver accumulation task ended");
            });
            self.accumulation_task = Some(handle);
        }

        // Set saving flag
        if let Ok(mut is_saving) = self.is_saving.lock() {
            *is_saving = true;
        }

        sender
    }

    /// Initialize meeting folder structure and metadata
    ///
    /// # Arguments
    /// * `meeting_name` - Name of the meeting
    /// * `create_checkpoints` - Whether to create .checkpoints/ directory and IncrementalAudioSaver
    fn initialize_meeting_folder(
        &mut self,
        meeting_name: &str,
        create_checkpoints: bool,
    ) -> Result<()> {
        // Load preferences to get base recordings folder
        let base_folder = super::recording_preferences::get_default_recordings_folder();

        // Create meeting folder structure (with or without .checkpoints/ subdirectory)
        let meeting_folder = create_meeting_folder(&base_folder, meeting_name, create_checkpoints)?;

        // Only initialize incremental saver if checkpoints are needed (auto_save is true)
        if create_checkpoints {
            // Stereo: mic on the left channel, system on the right (the
            // pipeline interleaves them). Keeps the two sources separable for
            // source-aware playback instead of pre-mixing to mono.
            let incremental_saver =
                IncrementalAudioSaver::new(meeting_folder.clone(), 48000, 2)?;
            self.incremental_saver = Some(Arc::new(AsyncMutex::new(incremental_saver)));
            info!(
                "✅ Incremental audio saver initialized for meeting: {}",
                meeting_name
            );
        } else {
            info!("⚠️  Skipped incremental audio saver (auto-save disabled)");
        }

        // Create initial metadata
        let metadata = MeetingMetadata {
            version: Some("1.0".to_string()),
            meeting_id: None, // Will be set by backend
            meeting_name: Some(meeting_name.to_string()),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            completed_at: None,
            retranscribed_at: None,
            duration_seconds: None,
            devices: Some(DeviceInfo {
                microphone: None, // Could be enhanced to store actual device names
                system_audio: None,
            }),
            audio_file: Some(
                if create_checkpoints {
                    "audio.mp4".to_string()
                } else {
                    "".to_string()
                },
            ),
            transcript_file: Some("transcripts.json".to_string()),
            sample_rate: Some(48000),
            status: Some("recording".to_string()),
            origin: Some("recording".to_string()),
            auto_refined_at: None,
        };

        // Write initial metadata.json
        self.write_metadata(&meeting_folder, &metadata)?;

        // Start the dedicated transcript journal writer task (issue #48).
        // Any writer left over from a previous session should already have
        // been stopped by `stop_and_save`/`discard_empty_session`, but drop
        // it defensively rather than leak a task talking to the old folder.
        self.journal_sender = None;
        if let Some(old_task) = self.journal_task.take() {
            old_task.abort();
        }
        let (sender, task) =
            Self::spawn_journal_writer(meeting_folder.clone(), self.transcript_segments.clone());
        self.journal_sender = Some(sender);
        self.journal_task = Some(task);

        self.meeting_folder = Some(meeting_folder);
        self.metadata = Some(metadata);

        Ok(())
    }

    /// Spawn the dedicated transcript journal writer task (issue #48).
    ///
    /// Owns the write side of `transcripts.ndjson`: every segment received
    /// from `rx` is appended as one JSON line (never blocking a tokio
    /// worker — the append itself runs on this task, off the caller's
    /// path), and `transcripts.json` is rebuilt from `segments` (the same
    /// `Arc<Mutex<Vec<TranscriptSegment>>>` `add_transcript_segment` already
    /// keeps deduped/upserted) at most once every `DEBOUNCE` while dirty.
    /// The task exits once `rx` closes (all senders dropped) — it does not
    /// perform a final rebuild itself; the caller (`stop_and_save`) does
    /// that explicitly after joining this task, so the "always write at
    /// stop" guarantee doesn't depend on debounce timing.
    fn spawn_journal_writer(
        folder: PathBuf,
        segments: Arc<Mutex<Vec<TranscriptSegment>>>,
    ) -> (
        mpsc::UnboundedSender<TranscriptSegment>,
        tokio::task::JoinHandle<()>,
    ) {
        const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

        let (sender, mut receiver) = mpsc::unbounded_channel::<TranscriptSegment>();

        let task = tokio::spawn(async move {
            info!("Transcript journal writer task started for {}", folder.display());
            let mut dirty = false;

            loop {
                tokio::select! {
                    biased;

                    maybe_segment = receiver.recv() => {
                        match maybe_segment {
                            Some(segment) => {
                                if let Err(e) = common::append_transcript_journal_line(&folder, &segment).await {
                                    warn!(
                                        "Failed to append transcript journal line for {}: {}",
                                        segment.id, e
                                    );
                                }
                                dirty = true;
                            }
                            None => {
                                info!("Transcript journal writer task ended (channel closed)");
                                break;
                            }
                        }
                    }

                    _ = tokio::time::sleep(DEBOUNCE), if dirty => {
                        Self::rebuild_transcripts_json(&folder, &segments).await;
                        dirty = false;
                    }
                }
            }
        });

        (sender, task)
    }

    /// Rebuild transcripts.json from the current in-memory segment set.
    /// Used both by the journal writer task's debounced rebuild and, via
    /// `write_transcripts_json`'s callers, at final save time.
    async fn rebuild_transcripts_json(folder: &Path, segments: &Arc<Mutex<Vec<TranscriptSegment>>>) {
        let mut segments_clone = match segments.lock() {
            Ok(guard) => guard.clone(),
            Err(e) => {
                error!("Failed to lock transcript segments for debounced rebuild: {}", e);
                return;
            }
        };
        Self::sort_segments_chronologically(&mut segments_clone);

        let folder = folder.to_path_buf();
        let write_result =
            tokio::task::spawn_blocking(move || common_write_transcripts_json(&folder, &segments_clone))
                .await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Debounced transcripts.json rebuild failed: {}", e),
            Err(e) => warn!("Debounced transcripts.json rebuild task panicked: {}", e),
        }
    }

    /// Stop the transcript journal writer task, waiting (bounded) for it to
    /// drain and append every already-queued segment before returning. Used
    /// by `stop_and_save` so the journal on disk is complete before the
    /// caller does the final `transcripts.json` rewrite from it.
    async fn stop_journal_writer(&mut self) {
        // Dropping the sender closes the channel, which is what lets the
        // writer task's `receiver.recv()` return `None` and exit its loop.
        self.journal_sender = None;
        if let Some(task) = self.journal_task.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), task).await {
                Ok(Ok(())) => info!("Transcript journal writer task drained cleanly"),
                Ok(Err(e)) => warn!("Transcript journal writer task panicked: {}", e),
                Err(_) => warn!("Timed out waiting for transcript journal writer task to drain"),
            }
        }
    }

    /// Write metadata.json to disk (atomic write with temp file, merging
    /// with whatever's already there — see `common::write_metadata`).
    fn write_metadata(&self, folder: &PathBuf, metadata: &MeetingMetadata) -> Result<()> {
        common_write_metadata(folder, metadata, || metadata.clone())
    }

    /// Write transcripts.json to disk (atomic write with temp file).
    fn write_transcripts_json(&self, folder: &PathBuf) -> Result<()> {
        // Clone segments to avoid holding lock during I/O
        let mut segments_clone = if let Ok(segments) = self.transcript_segments.lock() {
            segments.clone()
        } else {
            error!("Failed to lock transcript segments for writing");
            return Err(anyhow::anyhow!("Failed to lock transcript segments"));
        };

        // Segments arrive in completion order, not chronological order: with
        // dual-VAD (mic + system) sources, a segment that started earlier can
        // finish later (e.g. a long system-audio segment force-cut well after
        // a short mic segment that started after it). Re-order chronologically
        // by audio start time before persisting, with sequence_id as a
        // tie-breaker for segments that share (or lack) a start time.
        Self::sort_segments_chronologically(&mut segments_clone);

        info!(
            "Writing {} transcript segments to JSON",
            segments_clone.len()
        );

        common_write_transcripts_json(folder, &segments_clone)?;

        info!(
            "✅ Successfully wrote transcripts.json with {} segments",
            segments_clone.len()
        );
        Ok(())
    }

    /// Wait for the accumulation task spawned by `start_accumulation` to
    /// finish draining its channel, without finalizing anything (no
    /// `audio.mp4` merge, no metadata/transcript writes). Used when aborting
    /// a recording start that failed before real capture began (issue #45),
    /// so `discard_empty_session` can safely inspect (and remove) the
    /// meeting folder once the task is no longer writing to it.
    ///
    /// The caller is responsible for dropping/closing whatever sender the
    /// accumulation task's receiver is reading from (directly, or by
    /// stopping the pipeline that owns it) — otherwise this waits out its
    /// full timeout with nothing to show for it.
    pub async fn abort_accumulation(&mut self) {
        if let Some(task) = self.accumulation_task.take() {
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), task).await {
                Ok(Ok(())) => info!("Recording saver accumulation task drained cleanly (abort)"),
                Ok(Err(e)) => warn!("Recording saver accumulation task panicked (abort): {}", e),
                Err(_) => warn!(
                    "Timed out waiting for recording saver accumulation task to drain (abort)"
                ),
            }
        }
    }

    /// Discard the current session's meeting folder if it was created but
    /// never actually captured anything (issue #45).
    ///
    /// `start_accumulation` creates the meeting folder (plus `.checkpoints/`,
    /// `format.json` and `metadata.json` with status "recording") before the
    /// caller has actually managed to start audio capture. If capture then
    /// fails to start (pipeline or stream startup error), that folder is
    /// left behind on disk with no audio and no transcript, and the crash
    /// recovery dialog can later offer it as a recoverable meeting even
    /// though nothing was ever recorded.
    ///
    /// This removes the folder ONLY when it is safe to do so — no
    /// checkpoint audio chunks were written and no transcript segments were
    /// persisted — and resets this saver's session-scoped fields so it is
    /// ready to start a fresh session. If the folder holds real data (or no
    /// folder was created at all, e.g. auto_save-disabled + no meeting
    /// name), nothing is deleted.
    ///
    /// Returns `true` if the folder was discarded.
    pub fn discard_empty_session(&mut self) -> bool {
        let Some(folder) = self.meeting_folder.clone() else {
            return false;
        };

        if !Self::session_is_empty(&folder) {
            info!(
                "Not discarding meeting folder (contains audio or transcript data): {}",
                folder.display()
            );
            return false;
        }

        match std::fs::remove_dir_all(&folder) {
            Ok(()) => {
                info!(
                    "Discarded empty meeting folder from failed recording start: {}",
                    folder.display()
                );
            }
            Err(e) => {
                warn!(
                    "Failed to discard empty meeting folder {}: {}",
                    folder.display(),
                    e
                );
                // Fall through and reset our fields regardless — the caller
                // is aborting the session either way.
            }
        }

        self.meeting_folder = None;
        self.metadata = None;
        self.incremental_saver = None;
        // Drop the sender (closing its channel) and abort the journal
        // writer task rather than waiting for a graceful drain: this
        // session produced no data and its folder is already gone, so
        // there's nothing left for the writer to usefully flush.
        self.journal_sender = None;
        if let Some(task) = self.journal_task.take() {
            task.abort();
        }
        if let Ok(mut segments) = self.transcript_segments.lock() {
            segments.clear();
        }
        if let Ok(mut is_saving) = self.is_saving.lock() {
            *is_saving = false;
        }

        true
    }

    /// The actual discard decision (issue #45): a meeting folder is "empty"
    /// — safe to silently delete rather than leaving it for crash recovery
    /// to offer — only when `.checkpoints/` holds no `audio_chunk_*` files
    /// (covers both the current `.f32` checkpoints and the legacy `.mp4`
    /// ones) and neither `transcripts.json` nor `transcripts.ndjson`
    /// (issue #48: a session can have produced segments that only made it
    /// into the journal, not yet into a debounced transcripts.json rewrite)
    /// contains any segments.
    fn session_is_empty(meeting_folder: &std::path::Path) -> bool {
        let checkpoints_dir = meeting_folder.join(".checkpoints");
        let has_checkpoint_audio = match std::fs::read_dir(&checkpoints_dir) {
            Ok(entries) => entries.filter_map(|e| e.ok()).any(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|stem| stem.starts_with("audio_chunk_"))
                    .unwrap_or(false)
            }),
            Err(_) => false,
        };
        if has_checkpoint_audio {
            return false;
        }

        let has_transcript_segments = match common::load_transcripts_from_folder(meeting_folder) {
            Ok(segments) => !segments.is_empty(),
            // A transcripts.json that exists but fails to parse — treat
            // non-empty/unrecognized content as "has data" rather than risk
            // deleting real transcripts on a format we don't recognize
            // (mirrors the previous direct-read behavior here).
            Err(_) => true,
        };

        !has_transcript_segments
    }

    // in frontend/src-tauri/src/audio/recording_saver.rs
    pub fn get_stats(&self) -> (usize, u32) {
        if let Some(ref saver) = self.incremental_saver {
            if let Ok(guard) = saver.try_lock() {
                (guard.get_checkpoint_count() as usize, 48000)
            } else {
                (0, 48000)
            }
        } else {
            (0, 48000)
        }
    }

    /// Stop and save using incremental saving approach
    ///
    /// # Arguments
    /// * `sink` - event sink for emitting `recording-saved`
    /// * `recording_duration` - Actual recording duration in seconds (from RecordingState)
    pub async fn stop_and_save(
        &mut self,
        sink: &dyn crate::events::EventSink,
        recording_duration: Option<f64>,
    ) -> Result<Option<String>, String> {
        info!("Stopping recording saver");

        // Mark accumulation as stopping (informational only - the accumulation
        // task itself only terminates once its channel closes and drains, so
        // no chunk already queued at this point is lost).
        if let Ok(mut is_saving) = self.is_saving.lock() {
            *is_saving = false;
        }

        // Wait for the accumulation task to fully drain and write every chunk
        // the pipeline already queued before finalizing. The pipeline's sender
        // is dropped before stop_and_save is called, so recv() returns None -
        // and this resolves - once the queue is empty. Bounded so a leaked
        // sender elsewhere can't hang shutdown forever.
        if let Some(task) = self.accumulation_task.take() {
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), task).await {
                Ok(Ok(())) => info!("Recording saver accumulation task drained cleanly"),
                Ok(Err(e)) => warn!("Recording saver accumulation task panicked: {}", e),
                Err(_) => {
                    warn!("Timed out waiting for recording saver accumulation task to drain")
                }
            }
        }

        // Stop the transcript journal writer task and wait (bounded) for it
        // to drain: every segment already queued gets appended to
        // transcripts.ndjson before we replay the in-memory segment set
        // into the final transcripts.json rewrite below (issue #48 —
        // guarantees the "always write transcripts.json at stop" contract
        // doesn't race the debounce timer).
        self.stop_journal_writer().await;

        // Save final transcripts.json with validation. Done unconditionally
        // (regardless of whether audio auto-save is enabled below) since
        // the debounced background rebuild may be up to 5s stale.
        if let Some(folder) = self.meeting_folder.clone() {
            if let Err(e) = self.write_transcripts_json(&folder) {
                error!("❌ Failed to write final transcripts: {}", e);
                return Err(format!("Failed to save transcripts: {}", e));
            }

            // Verify transcripts were written correctly
            let transcript_path = folder.join("transcripts.json");
            if !transcript_path.exists() {
                error!(
                    "❌ Transcript file was not created at: {}",
                    transcript_path.display()
                );
                return Err("Transcript file verification failed".to_string());
            }
            info!(
                "✅ Transcripts saved and verified at: {}",
                transcript_path.display()
            );

            // The journal is now fully superseded by this fresh
            // transcripts.json — remove it so a stale ndjson never shadows
            // the just-written json for a later reader (see
            // `common::load_transcripts_from_folder`). Deliberately left in
            // place on every earlier error-return above, so crash recovery
            // can still replay it if something above failed.
            let journal_path = folder.join(TRANSCRIPTS_JOURNAL_FILENAME);
            if journal_path.exists() {
                if let Err(e) = std::fs::remove_file(&journal_path) {
                    warn!(
                        "Failed to remove transcript journal {}: {}",
                        journal_path.display(),
                        e
                    );
                }
            }
        }

        // Check if incremental saver exists (indicates auto_save was enabled)
        let should_save_audio = self.incremental_saver.is_some();

        if !should_save_audio {
            info!("⚠️  No audio saver initialized (auto-save was disabled) - skipping audio finalization");
            info!("✅ Transcripts and metadata already saved");
            return Ok(None);
        }

        // Finalize incremental saver (merge checkpoints into final audio.mp4)
        let final_audio_path = if let Some(saver_arc) = &self.incremental_saver {
            let mut saver = saver_arc.lock().await;
            match saver.finalize().await {
                Ok(path) => {
                    info!("✅ Successfully finalized audio: {}", path.display());
                    path
                }
                Err(e) => {
                    error!("❌ Failed to finalize incremental saver: {}", e);
                    return Err(format!("Failed to finalize audio: {}", e));
                }
            }
        } else {
            error!("No incremental saver initialized - cannot save recording");
            return Err("No incremental saver initialized".to_string());
        };

        // Update metadata to completed status with actual recording duration
        if let (Some(folder), Some(mut metadata)) = (&self.meeting_folder, self.metadata.clone()) {
            metadata.status = Some("completed".to_string());
            metadata.completed_at = Some(chrono::Utc::now().to_rfc3339());

            // Use actual recording duration from RecordingState (more accurate than transcript segments)
            // Falls back to last transcript segment if duration not provided
            metadata.duration_seconds = recording_duration.or_else(|| {
                if let Ok(segments) = self.transcript_segments.lock() {
                    segments.last().and_then(|seg| seg.audio_end_time)
                } else {
                    None
                }
            });

            if let Err(e) = self.write_metadata(folder, &metadata) {
                error!("❌ Failed to update metadata to completed: {}", e);
                return Err(format!("Failed to update metadata: {}", e));
            }

            info!(
                "✅ Metadata updated with duration: {:?}s",
                metadata.duration_seconds
            );
        }

        // Emit save event with audio and transcript paths
        let save_event = serde_json::json!({
            "audio_file": final_audio_path.to_string_lossy(),
            "transcript_file": self.meeting_folder.as_ref()
                .map(|f| f.join("transcripts.json").to_string_lossy().to_string()),
            "meeting_name": self.meeting_name,
            "meeting_folder": self.meeting_folder.as_ref()
                .map(|f| f.to_string_lossy().to_string())
        });

        if let Err(e) = sink.emit_event("recording-saved", &save_event) {
            warn!("Failed to emit recording-saved event: {}", e);
        }

        // Clean up transcript segments
        if let Ok(mut segments) = self.transcript_segments.lock() {
            segments.clear();
        }

        Ok(Some(final_audio_path.to_string_lossy().to_string()))
    }

    /// Get the meeting folder path (for passing to backend)
    pub fn get_meeting_folder(&self) -> Option<&PathBuf> {
        self.meeting_folder.as_ref()
    }

    /// Get accumulated transcript segments (for reload sync)
    pub fn get_transcript_segments(&self) -> Vec<TranscriptSegment> {
        if let Ok(segments) = self.transcript_segments.lock() {
            segments.clone()
        } else {
            Vec::new()
        }
    }

    /// Get meeting name (for reload sync)
    pub fn get_meeting_name(&self) -> Option<String> {
        self.meeting_name.clone()
    }

    /// Sort transcript segments chronologically by `audio_start_time`, using
    /// `sequence_id` as a tie-breaker when the start time is equal or absent.
    /// See `write_transcripts_json` for why this ordering matters. Thin
    /// wrapper over `common::sort_segments_chronologically` so it's shared
    /// with journal-replay callers (kept as an associated fn here since the
    /// existing tests below call it as `RecordingSaver::sort_segments_chronologically`).
    fn sort_segments_chronologically(segments: &mut [TranscriptSegment]) {
        common::sort_segments_chronologically(segments);
    }
}

impl Default for RecordingSaver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(id: &str, audio_start_time: Option<f64>, sequence_id: Option<u64>) -> TranscriptSegment {
        TranscriptSegment {
            id: id.to_string(),
            text: id.to_string(),
            timestamp: None,
            audio_start_time,
            audio_end_time: None,
            duration: None,
            display_time: None,
            confidence: None,
            sequence_id,
            speaker: None,
            voice_profile_id: None,
            source: None,
        }
    }

    #[test]
    fn sorts_out_of_arrival_order_segments_by_audio_start_time() {
        // Mirrors issue #37: a system-audio segment starting at t=10s but
        // finishing (and thus being appended) at t=22s must still land before
        // a mic segment spanning 15-16s in the persisted transcript.
        let mut segments = vec![
            segment("mic-15-16", Some(15.0), Some(2)),
            segment("system-10-22", Some(10.0), Some(1)),
        ];

        RecordingSaver::sort_segments_chronologically(&mut segments);

        assert_eq!(
            segments.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["system-10-22", "mic-15-16"]
        );
    }

    #[test]
    fn falls_back_to_sequence_id_when_start_times_tie_or_are_missing() {
        let mut segments = vec![
            segment("no-time-seq-3", None, Some(3)),
            segment("t5-seq-1", Some(5.0), Some(1)),
            segment("no-time-seq-2", None, Some(2)),
            segment("t5-seq-0", Some(5.0), Some(0)),
        ];

        RecordingSaver::sort_segments_chronologically(&mut segments);

        assert_eq!(
            segments.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["t5-seq-0", "t5-seq-1", "no-time-seq-2", "no-time-seq-3"]
        );
    }

    // ---- discard_empty_session / session_is_empty (issue #45) ------------

    #[test]
    fn session_is_empty_for_a_freshly_created_folder() {
        // Mirrors what start_accumulation() creates before capture has
        // produced anything: a bare meeting folder with an empty
        // .checkpoints/ directory and no transcripts.json yet.
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting_2026-01-01_00-00-00");
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        assert!(RecordingSaver::session_is_empty(&meeting_folder));
    }

    #[test]
    fn session_is_not_empty_with_a_checkpoint_audio_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        let checkpoints_dir = meeting_folder.join(".checkpoints");
        std::fs::create_dir_all(&checkpoints_dir).unwrap();
        std::fs::write(checkpoints_dir.join("audio_chunk_000.f32"), [0u8; 4]).unwrap();

        assert!(!RecordingSaver::session_is_empty(&meeting_folder));
    }

    #[test]
    fn session_is_not_empty_with_a_legacy_mp4_checkpoint_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        let checkpoints_dir = meeting_folder.join(".checkpoints");
        std::fs::create_dir_all(&checkpoints_dir).unwrap();
        std::fs::write(checkpoints_dir.join("audio_chunk_000.mp4"), [0u8; 4]).unwrap();

        assert!(!RecordingSaver::session_is_empty(&meeting_folder));
    }

    #[test]
    fn session_is_not_empty_with_non_empty_transcripts_json() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();
        std::fs::write(
            meeting_folder.join("transcripts.json"),
            r#"[{"id":"seg-1","text":"hello"}]"#,
        )
        .unwrap();

        assert!(!RecordingSaver::session_is_empty(&meeting_folder));
    }

    #[test]
    fn session_is_empty_with_an_empty_transcripts_json_array() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();
        std::fs::write(meeting_folder.join("transcripts.json"), "[]").unwrap();

        assert!(RecordingSaver::session_is_empty(&meeting_folder));
    }

    #[test]
    fn session_is_empty_when_checkpoints_dir_is_missing_entirely() {
        // auto_save disabled: initialize_meeting_folder(..., false) never
        // creates .checkpoints/ at all.
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        assert!(RecordingSaver::session_is_empty(&meeting_folder));
    }

    #[test]
    fn discard_empty_session_removes_the_folder_and_resets_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = RecordingSaver::new();
        saver.meeting_folder = Some(meeting_folder.clone());
        saver.metadata = Some(MeetingMetadata {
            version: Some("1.0".to_string()),
            meeting_id: None,
            meeting_name: Some("Meeting".to_string()),
            created_at: None,
            completed_at: None,
            retranscribed_at: None,
            duration_seconds: None,
            devices: None,
            audio_file: None,
            transcript_file: None,
            sample_rate: None,
            status: Some("recording".to_string()),
            origin: None,
            auto_refined_at: None,
        });
        *saver.is_saving.lock().unwrap() = true;

        assert!(saver.discard_empty_session());

        assert!(!meeting_folder.exists());
        assert!(saver.meeting_folder.is_none());
        assert!(saver.metadata.is_none());
        assert!(!*saver.is_saving.lock().unwrap());
    }

    #[test]
    fn discard_empty_session_leaves_a_non_empty_folder_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        let checkpoints_dir = meeting_folder.join(".checkpoints");
        std::fs::create_dir_all(&checkpoints_dir).unwrap();
        std::fs::write(checkpoints_dir.join("audio_chunk_000.f32"), [0u8; 4]).unwrap();

        let mut saver = RecordingSaver::new();
        saver.meeting_folder = Some(meeting_folder.clone());

        assert!(!saver.discard_empty_session());
        assert!(meeting_folder.exists());
        assert!(saver.meeting_folder.is_some());
    }

    #[test]
    fn discard_empty_session_is_a_no_op_with_no_meeting_folder() {
        let mut saver = RecordingSaver::new();
        assert!(!saver.discard_empty_session());
    }

    // ---- transcript journal writer (issue #48) ----------------------------

    #[tokio::test]
    async fn journal_writer_drains_queued_segments_and_stop_removes_the_journal_file() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        let mut saver = RecordingSaver::new();
        saver.meeting_folder = Some(meeting_folder.clone());
        let (sender, task) = RecordingSaver::spawn_journal_writer(
            meeting_folder.clone(),
            saver.transcript_segments.clone(),
        );
        saver.journal_sender = Some(sender);
        saver.journal_task = Some(task);

        // add_transcript_segment both updates the in-memory (deduped) vec
        // and enqueues the segment for the journal writer task.
        saver.add_transcript_segment(segment("seg-1", Some(1.0), Some(0)));
        saver.add_transcript_segment(segment("seg-2", Some(2.0), Some(1)));

        // Draining (as stop_and_save does) waits for every queued segment
        // to actually land in transcripts.ndjson before returning.
        saver.stop_journal_writer().await;

        let journal_path = meeting_folder.join(common::TRANSCRIPTS_JOURNAL_FILENAME);
        assert!(journal_path.exists(), "journal should exist after draining");
        let contents = std::fs::read_to_string(&journal_path).unwrap();
        assert!(contents.contains("seg-1"));
        assert!(contents.contains("seg-2"));

        // Mirror stop_and_save's final-write step: rewrite transcripts.json
        // from the (already up to date) in-memory vec, then remove the
        // now-superseded journal — exactly what stop_and_save does after a
        // successful final write.
        saver.write_transcripts_json(&meeting_folder).unwrap();
        assert!(meeting_folder.join("transcripts.json").exists());
        std::fs::remove_file(&journal_path).unwrap();

        assert!(
            !journal_path.exists(),
            "journal should be removed once transcripts.json is finalized"
        );

        // A reader loading the folder afterwards sees the same two segments
        // purely from transcripts.json, with no journal left to consult.
        let loaded = common::load_transcripts_from_folder(&meeting_folder).unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn discard_empty_session_stops_the_journal_writer_without_hanging() {
        let tmp = tempfile::tempdir().unwrap();
        let meeting_folder = tmp.path().join("Meeting");
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = RecordingSaver::new();
        saver.meeting_folder = Some(meeting_folder.clone());
        let (sender, task) = RecordingSaver::spawn_journal_writer(
            meeting_folder.clone(),
            saver.transcript_segments.clone(),
        );
        saver.journal_sender = Some(sender);
        saver.journal_task = Some(task);

        assert!(saver.discard_empty_session());
        assert!(saver.journal_sender.is_none());
        assert!(saver.journal_task.is_none());
        assert!(!meeting_folder.exists());
    }
}
