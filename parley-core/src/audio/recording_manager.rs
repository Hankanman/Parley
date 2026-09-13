use anyhow::Result;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::devices::{AudioDevice, DeviceType};
use super::pipeline::AudioPipelineManager;
use super::recording_saver::RecordingSaver;
use super::recording_state::{AudioChunk, PartialAudioChunk, RecordingState};
use super::stream::AudioStreamManager;

/// Recording manager that coordinates all audio components
pub struct RecordingManager {
    state: Arc<RecordingState>,
    stream_manager: AudioStreamManager,
    pipeline_manager: AudioPipelineManager,
    recording_saver: RecordingSaver,
    /// Receiver for streaming-partial snapshots, produced in `start_recording`
    /// when partials are enabled and claimed once by the command layer (which
    /// owns the event sink needed to emit `transcript-partial` events).
    partial_receiver: Option<mpsc::UnboundedReceiver<PartialAudioChunk>>,
}

impl RecordingManager {
    /// Create a new recording manager
    pub fn new() -> Self {
        let state = RecordingState::new();
        let stream_manager = AudioStreamManager::new(state.clone());
        let pipeline_manager = AudioPipelineManager::new();

        Self {
            state,
            stream_manager,
            pipeline_manager,
            recording_saver: RecordingSaver::new(),
            partial_receiver: None,
        }
    }

    /// Claim the streaming-partial receiver (available after `start_recording`
    /// when partials are enabled). Returns None if partials are disabled or
    /// already claimed.
    pub fn take_partial_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<PartialAudioChunk>> {
        self.partial_receiver.take()
    }

    // Remove app handle storage for now - will be passed directly when saving

    /// Start recording with specified devices
    ///
    /// # Arguments
    /// * `microphone_device` - Optional microphone device to use
    /// * `system_device` - Optional system audio device to use
    /// * `auto_save` - Whether to save audio checkpoints (true) or just transcripts/metadata (false)
    pub async fn start_recording(
        &mut self,
        microphone_device: Option<Arc<AudioDevice>>,
        system_device: Option<Arc<AudioDevice>>,
        auto_save: bool,
        enable_partials: bool,
    ) -> Result<mpsc::UnboundedReceiver<AudioChunk>> {
        info!(
            "Starting recording manager (auto_save: {}, partials: {})",
            auto_save, enable_partials
        );

        // Set up transcription channel. The pipeline gets a plain
        // `UnboundedSender` as before (unmodified pipeline.rs); a lightweight
        // forwarding task counts segments in flight for the transcription
        // backlog metric (issue #26) without pipeline.rs needing to know
        // about it. See `transcription::queue` for the accounting and the
        // "channel closed" completion-signal handoff.
        super::transcription::reset_queue_depth();
        let (transcription_sender, raw_transcription_receiver) =
            mpsc::unbounded_channel::<AudioChunk>();
        let transcription_receiver =
            super::transcription::spawn_counting_forwarder(raw_transcription_receiver);

        // Streaming-partial channel (only when enabled). The command layer
        // claims the receiver via take_partial_receiver() and spawns the
        // partial-decode task with its event sink.
        let partial_sender = if enable_partials {
            let (tx, rx) = mpsc::unbounded_channel::<PartialAudioChunk>();
            self.partial_receiver = Some(rx);
            Some(tx)
        } else {
            self.partial_receiver = None;
            None
        };

        // CRITICAL FIX: Create recording sender for pre-mixed audio from pipeline
        // Pipeline will mix mic + system audio professionally and send to this channel
        // Pass auto_save to control whether audio checkpoints are created
        let recording_sender = self.recording_saver.start_accumulation(auto_save);

        // Start recording state first
        self.state.start_recording()?;

        // Device names, used only for logging in the pipeline.
        let mic_name = microphone_device
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "No Microphone".to_string());
        let sys_name = system_device
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "No System Audio".to_string());

        // Update recording metadata with device information
        self.recording_saver.set_device_info(
            microphone_device.as_ref().map(|d| d.name.clone()),
            system_device.as_ref().map(|d| d.name.clone()),
        );

        // Start the audio processing pipeline with FFmpeg adaptive mixer
        // Pipeline will: 1) Mix mic+system audio with adaptive buffering, 2) Send mixed to recording_sender,
        // 3) Apply VAD and send speech segments to transcription
        //
        // issue #45: start_accumulation() above already created the meeting
        // folder (and, when auto_save is on, .checkpoints/ + metadata.json
        // with status "recording") on disk. If pipeline or stream startup
        // fails from here on, that folder must not be left behind as an
        // orphan for crash recovery to later offer — tear everything down
        // and discard it (only if it never actually captured anything).
        if let Err(e) = self.pipeline_manager.start(
            self.state.clone(),
            transcription_sender,
            0,                      // Ignored - using dynamic sizing internally
            48000,                  // 48kHz sample rate
            Some(recording_sender), // CRITICAL: Pass recording sender to receive pre-mixed audio
            partial_sender,         // Streaming partials (None when disabled)
            mic_name,
            sys_name,
            microphone_device.is_some(),
            system_device.is_some(),
        ) {
            error!("Failed to start audio pipeline: {}", e);
            self.abort_failed_start().await;
            return Err(e);
        }

        // Give the pipeline a moment to fully initialize before starting streams
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Start audio streams - they send RAW unmixed chunks to pipeline for mixing
        // Pipeline handles mixing and distribution to both recording and transcription
        if let Err(e) = self
            .stream_manager
            .start_streams(microphone_device.clone(), system_device.clone())
            .await
        {
            error!("Failed to start audio streams: {}", e);
            // The pipeline did start; stop it so it releases the recording
            // sender (letting the saver's accumulation task drain and end)
            // before we inspect/discard the meeting folder.
            if let Err(stop_err) = self.pipeline_manager.stop().await {
                error!(
                    "Error stopping audio pipeline during failed-start cleanup: {}",
                    stop_err
                );
            }
            self.abort_failed_start().await;
            return Err(e);
        }

        info!(
            "Recording manager started successfully with {} active streams",
            self.stream_manager.active_stream_count()
        );

        Ok(transcription_receiver)
    }

    /// Clean up after a `start_recording` failure that happened after
    /// `start_accumulation()` already created the meeting folder (issue
    /// #45): reset recording state and discard the folder if it never
    /// captured anything, so a failed start doesn't leave an orphan
    /// "recording"-status meeting for crash recovery to offer later.
    async fn abort_failed_start(&mut self) {
        self.state.stop_recording();
        // Wait for the accumulation task to see its channel close (dropped
        // by the pipeline stop above, or never handed off if pipeline
        // startup itself failed) before inspecting the folder it wrote to.
        self.recording_saver.abort_accumulation().await;
        self.recording_saver.discard_empty_session();
    }

    /// Start recording with the system default source + sink monitor.
    ///
    /// PipeWire resolves `"default"` to the user's currently selected
    /// default source/sink and reroutes streams if the default changes
    /// mid-recording — no Bluetooth-override heuristics needed.
    pub async fn start_recording_with_defaults_and_auto_save(
        &mut self,
        auto_save: bool,
    ) -> Result<mpsc::UnboundedReceiver<AudioChunk>> {
        info!("Starting recording with default devices");

        let microphone_device = Some(Arc::new(AudioDevice::new(
            "default".to_string(),
            DeviceType::Input,
        )));
        let system_device = Some(Arc::new(AudioDevice::new(
            "default".to_string(),
            DeviceType::Output,
        )));

        self.start_recording(microphone_device, system_device, auto_save, true)
            .await
    }

    /// Stop recording streams without saving (for use when waiting for transcription)
    pub async fn stop_streams_only(&mut self) -> Result<()> {
        info!("Stopping recording streams only");

        // Stop recording state first
        self.state.stop_recording();

        // Stop audio streams
        if let Err(e) = self.stream_manager.stop_streams().await {
            error!("Error stopping audio streams: {}", e);
        }

        // Stop audio pipeline
        if let Err(e) = self.pipeline_manager.stop().await {
            error!("Error stopping audio pipeline: {}", e);
        }

        debug!("Recording streams stopped successfully");
        Ok(())
    }

    /// Stop streams and force immediate pipeline flush to process all accumulated audio
    pub async fn stop_streams_and_force_flush(&mut self) -> Result<()> {
        info!("🚀 Stopping recording streams with IMMEDIATE pipeline flush");

        // Stop recording state first - this clears device references
        self.state.stop_recording();

        // Stop audio streams immediately
        if let Err(e) = self.stream_manager.stop_streams().await {
            error!("Error stopping audio streams: {}", e);
        }

        // CRITICAL: Force pipeline to flush ALL accumulated audio before stopping
        debug!("💨 Forcing pipeline to flush accumulated audio immediately");
        if let Err(e) = self.pipeline_manager.force_flush_and_stop().await {
            error!("Error during force flush: {}", e);
        }

        // CRITICAL: Full cleanup to release all Arc references and resources
        // This ensures microphone is released even if Drop is delayed
        self.state.cleanup();

        info!("✅ Recording streams stopped with immediate flush completed");
        Ok(())
    }

    /// Save recording after transcription is complete. Returns the final
    /// audio file path (`None` if auto-save was disabled or saving failed)
    /// and the active recording duration used to save it, so the caller can
    /// finalise the meeting's database row (issue #57 slice 2) without
    /// re-deriving either value.
    pub async fn save_recording_only(
        &mut self,
        sink: &dyn crate::events::EventSink,
    ) -> Result<(Option<String>, Option<f64>)> {
        debug!("Saving recording with transcript chunks");

        // Get actual recording duration from state
        let recording_duration = self.state.get_active_recording_duration();
        info!("Recording duration from state: {:?}s", recording_duration);

        // Save the recording with actual duration
        let audio_path = match self
            .recording_saver
            .stop_and_save(sink, recording_duration)
            .await
        {
            Ok(Some(file_path)) => {
                info!("Recording saved successfully to: {}", file_path);
                Some(file_path)
            }
            Ok(None) => {
                debug!("Recording not saved (auto-save disabled or no audio data)");
                None
            }
            Err(e) => {
                error!("Failed to save recording: {}", e);
                // Don't fail the stop operation if saving fails
                None
            }
        };

        debug!("Recording save operation completed");
        Ok((audio_path, recording_duration))
    }

    /// Stop recording and save audio (legacy method)
    pub async fn stop_recording(
        &mut self,
        sink: &dyn crate::events::EventSink,
    ) -> Result<()> {
        info!("Stopping recording manager");

        // Get recording duration BEFORE stopping (important!)
        let recording_duration = self.state.get_active_recording_duration();
        info!("Recording duration before stop: {:?}s", recording_duration);

        // Stop recording state first
        self.state.stop_recording();

        // Stop audio streams
        if let Err(e) = self.stream_manager.stop_streams().await {
            error!("Error stopping audio streams: {}", e);
        }

        // Stop audio pipeline
        if let Err(e) = self.pipeline_manager.stop().await {
            error!("Error stopping audio pipeline: {}", e);
        }

        // Save the recording with actual duration
        match self
            .recording_saver
            .stop_and_save(sink, recording_duration)
            .await
        {
            Ok(Some(file_path)) => {
                info!("Recording saved successfully to: {}", file_path);
            }
            Ok(None) => {
                info!("Recording not saved (auto-save disabled or no audio data)");
            }
            Err(e) => {
                error!("Failed to save recording: {}", e);
                // Don't fail the stop operation if saving fails
            }
        }

        info!("Recording manager stopped");
        Ok(())
    }

    /// Get recording stats from the saver
    pub fn get_recording_stats(&self) -> (usize, u32) {
        self.recording_saver.get_stats()
    }

    /// Check if currently recording
    pub fn is_recording(&self) -> bool {
        self.state.is_recording()
    }

    /// Pause the current recording session
    pub fn pause_recording(&self) -> Result<()> {
        info!("Pausing recording");
        self.state.pause_recording()
    }

    /// Resume the current recording session
    pub fn resume_recording(&self) -> Result<()> {
        info!("Resuming recording");
        self.state.resume_recording()
    }

    /// Check if recording is currently paused
    pub fn is_paused(&self) -> bool {
        self.state.is_paused()
    }

    /// Check if recording is active (recording and not paused)
    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    /// Get recording duration
    pub fn get_recording_duration(&self) -> Option<f64> {
        self.state.get_recording_duration()
    }

    /// Get active recording duration (excluding pauses)
    pub fn get_active_recording_duration(&self) -> Option<f64> {
        self.state.get_active_recording_duration()
    }

    /// Get total pause duration
    pub fn get_total_pause_duration(&self) -> f64 {
        self.state.get_total_pause_duration()
    }

    /// Get current pause duration if paused
    pub fn get_current_pause_duration(&self) -> Option<f64> {
        self.state.get_current_pause_duration()
    }

    /// Get error information
    pub fn get_error_info(&self) -> (u32, Option<super::recording_state::AudioError>) {
        (self.state.get_error_count(), self.state.get_last_error())
    }

    /// Get active stream count
    pub fn active_stream_count(&self) -> usize {
        self.stream_manager.active_stream_count()
    }

    /// Set error callback for handling errors
    pub fn set_error_callback<F>(&self, callback: F)
    where
        F: Fn(&super::recording_state::AudioError) + Send + Sync + 'static,
    {
        self.state.set_error_callback(callback);
    }

    /// Check if there's a fatal error
    pub fn has_fatal_error(&self) -> bool {
        self.state.has_fatal_error()
    }

    /// Set the meeting name for this recording session
    pub fn set_meeting_name(&mut self, name: Option<String>) {
        self.recording_saver.set_meeting_name(name);
    }

    /// Set the `meetings` row id for this recording session (issue #57
    /// slice 2).
    pub fn set_meeting_id(&mut self, id: Option<String>) {
        self.recording_saver.set_meeting_id(id);
    }

    /// Get the `meetings` row id for this recording session, if set.
    pub fn get_meeting_id(&self) -> Option<String> {
        self.recording_saver.get_meeting_id()
    }

    /// Add a structured transcript segment to be saved later
    pub fn add_transcript_segment(&self, segment: super::recording_saver::TranscriptSegment) {
        self.recording_saver.add_transcript_segment(segment);
    }

    /// Add a transcript chunk to be saved later (legacy method)
    pub fn add_transcript_chunk(&self, text: String) {
        self.recording_saver.add_transcript_chunk(text);
    }

    /// Get accumulated transcript segments from current recording session
    /// Used for syncing frontend state after page reload during active recording
    pub fn get_transcript_segments(&self) -> Vec<super::recording_saver::TranscriptSegment> {
        self.recording_saver.get_transcript_segments()
    }

    /// Get meeting name from current recording session
    /// Used for syncing frontend state after page reload during active recording
    pub fn get_meeting_name(&self) -> Option<String> {
        self.recording_saver.get_meeting_name()
    }

    /// Cleanup all resources without saving
    pub async fn cleanup_without_save(&mut self) {
        if self.is_recording() {
            debug!("Stopping recording without saving during cleanup");

            // Stop recording state first
            self.state.stop_recording();

            // Stop audio streams
            if let Err(e) = self.stream_manager.stop_streams().await {
                error!("Error stopping audio streams during cleanup: {}", e);
            }

            // Stop audio pipeline
            if let Err(e) = self.pipeline_manager.stop().await {
                error!("Error stopping audio pipeline during cleanup: {}", e);
            }
        }
        self.state.cleanup();
    }

    /// Get the meeting folder path (if available)
    /// Returns None if no meeting name was set or folder structure not initialized
    pub fn get_meeting_folder(&self) -> Option<std::path::PathBuf> {
        self.recording_saver.get_meeting_folder().map(|p| p.clone())
    }

    /// Get reference to recording state for external access
    pub fn get_state(&self) -> &Arc<RecordingState> {
        &self.state
    }
}

impl Default for RecordingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RecordingManager {
    fn drop(&mut self) {
        // Note: Can't call async cleanup in Drop, but streams have their own Drop implementations
        self.state.cleanup();
    }
}
