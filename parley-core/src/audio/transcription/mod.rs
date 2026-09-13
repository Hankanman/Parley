// audio/transcription/mod.rs
//
// Transcription module: engine management and worker pool. Whisper is the
// sole local ASR engine (a prior remote-provider abstraction was removed as
// dead code — see worker.rs for TranscriptionError).

// Issue #56 spike: runtime sidecar backend probing/selection. Off by
// default — see backend_probe.rs's module doc comment for scope and status.
#[cfg(feature = "backend_probe")]
pub mod backend_probe;
pub mod echo_dedup;
pub mod engine;
pub mod partial_worker;
pub mod queue;
pub mod worker;

// Re-export commonly used types
pub use engine::{
    get_or_init_transcription_engine, get_or_init_whisper, validate_transcription_model_ready,
    TranscriptionEngine,
};
pub use partial_worker::{start_partial_decode_task, take_partial_task_handle};
pub use queue::{queue_depth, reset_queue_depth, spawn_counting_forwarder};
pub use worker::{
    reset_speech_detected_flag, start_transcription_task, TranscriptUpdate, TranscriptionError,
};
