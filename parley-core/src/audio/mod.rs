// src/audio/mod.rs
pub mod audio_processing;
pub mod aec;
pub mod clip;
pub mod decoder;
pub mod playback;
pub mod encode;
pub mod ffmpeg;
pub mod vad;

// Device model + PipeWire-backed discovery
pub mod devices;

// Native PipeWire capture layer (Linux)
#[cfg(target_os = "linux")]
pub mod pw;

// Recording system
pub mod hardware_detector;
pub mod incremental_saver;
pub mod pipeline;
pub mod recording_manager;
pub mod recording_service;
pub mod recording_phase;
pub mod recording_preferences;
pub mod recording_saver;
pub mod recording_state;
pub mod simple_level_monitor;
pub mod stream;

// Batched SQLite writer for live-recording transcript segments (issue #57
// slice 2). The crash-recovery commands (`list_interrupted_meetings`,
// `recover_meeting`) that read the meeting-row lifecycle it feeds live in
// the shell crate's `commands::audio::recovery_commands`.
pub mod transcript_bus;
pub mod transcript_db_writer;

// Transcription module (provider abstraction, engine management, worker pool)
pub mod transcription;

// Shared utilities for import and retranscription
pub mod common;

// Shared constants
pub mod constants;

// Retranscription module (re-process stored audio with different settings)
pub mod retranscription;

// Import module (import external audio files as new meetings)
pub mod import;

pub use devices::{list_audio_devices, trigger_audio_permission, AudioDevice, DeviceType};

pub use hardware_detector::{AdaptiveWhisperConfig, GpuType, HardwareProfile, PerformanceTier};
pub use pipeline::AudioPipelineManager;
pub use recording_service::{get_transcription_status, is_recording, RecordingArgs, TranscriptionStatus};
pub use recording_manager::RecordingManager;
pub use transcription::TranscriptUpdate;
pub use recording_preferences::{get_default_recordings_folder, RecordingPreferences};
pub use recording_saver::RecordingSaver;
pub use recording_state::{
    AudioChunk, AudioError, DeviceType as RecordingDeviceType, ProcessedAudioChunk, RecordingState,
};
pub use stream::AudioStreamManager;

// Export decoder for retranscription
pub use decoder::{decode_audio_file, DecodedAudio};

// Export audio constants
pub use constants::AUDIO_EXTENSIONS;
