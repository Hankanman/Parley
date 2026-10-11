//! Wire protocol between the Parley app and a `whisper-helper` sidecar.
//!
//! Modelled on `llama-protocol`: newline-delimited JSON over the child
//! process's stdin/stdout. The app writes one [`Request`] per line, the
//! helper answers with exactly one terminal line per request
//! ([`Response::Loaded`], [`Response::Transcribed`], [`Response::Pong`],
//! [`Response::ProbeResult`], [`Response::Goodbye`], or [`Response::Error`]).
//!
//! Both sides depend on this crate so the two ends of the pipe can't drift
//! apart silently — serde ignores unknown fields on deserialize, so a field
//! added to only one side is dropped rather than erroring.

use serde::{Deserialize, Serialize};

/// App → helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Load (or switch to) a GGML/GGUF whisper model. Mirrors
    /// `WhisperEngine::load_model` in the in-process engine.
    LoadModel { path: String },
    /// Transcribe one chunk of 16 kHz mono f32 PCM audio.
    ///
    /// `samples_b64` is little-endian f32 samples, base64-encoded (the
    /// baseline IPC transport measured by this spike; see
    /// `docs/transcription-backends.md` for the shared-memory/tmpfile
    /// alternative considered for larger chunks).
    Transcribe {
        samples_b64: String,
        language: Option<String>,
        context_prompt: Option<String>,
        /// Per-call overrides — mirrors
        /// `whisper_engine::TranscribeOptions`.
        max_threads: Option<i32>,
        greedy: Option<bool>,
    },
    /// Release the loaded model and its context (frees VRAM/RAM without
    /// exiting the process).
    Unload,
    /// Liveness check; also used by `backend_probe` as the cheapest possible
    /// "is this sidecar binary runnable at all" test.
    Ping,
    /// Run a short synthetic decode (silence, ~1s) to confirm the compiled
    /// backend (cuda/vulkan/cpu) actually initializes on this machine — a
    /// `cuda` build can load and `Ping` fine on a machine with no NVIDIA
    /// driver, but will fail here. `model_path` is optional: with no model
    /// on disk the probe falls back to backend self-report only (see
    /// `Response::ProbeResult::decode_ok`).
    Probe { model_path: Option<String> },
    Shutdown,
}

/// Helper → app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Loaded {
        error: Option<String>,
        load_ms: u64,
    },
    /// One transcribed chunk. Segment-level detail is intentionally kept
    /// (rather than one flat string) so a future UI can render per-segment
    /// confidence the way the in-process engine's timestamp-aware paths do.
    Transcribed {
        text: String,
        confidence: f32,
        is_partial: bool,
        segments: Vec<Segment>,
        error: Option<String>,
        /// Wall-clock time spent inside whisper's `full()` call, for
        /// per-segment round-trip overhead measurements.
        decode_ms: u64,
    },
    Unloaded,
    Pong,
    ProbeResult {
        backend: String,
        /// `None` when no model was available to attempt a decode with —
        /// the probe only confirmed the process starts and responds.
        decode_ok: Option<bool>,
        detail: String,
    },
    Goodbye,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub confidence: f32,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcribe_request_round_trips() {
        let req = Request::Transcribe {
            samples_b64: "AAAA".to_string(),
            language: Some("en".to_string()),
            context_prompt: None,
            max_threads: Some(4),
            greedy: Some(false),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"transcribe\""));
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Transcribe { language, max_threads, .. } => {
                assert_eq!(language.as_deref(), Some("en"));
                assert_eq!(max_threads, Some(4));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn probe_result_round_trips_with_no_model() {
        let resp = Response::ProbeResult {
            backend: "cuda".to_string(),
            decode_ok: None,
            detail: "no model available, ping only".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        match serde_json::from_str::<Response>(&json).unwrap() {
            Response::ProbeResult { backend, decode_ok, .. } => {
                assert_eq!(backend, "cuda");
                assert_eq!(decode_ok, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_response_round_trips() {
        let json = r#"{"type":"error","message":"boom"}"#;
        match serde_json::from_str::<Response>(json).unwrap() {
            Response::Error { message } => assert_eq!(message, "boom"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unknown_field_is_ignored_not_an_error() {
        // Forward-compat: a field only one side knows about must not break
        // deserialization on the other side.
        let json = r#"{"type":"ping","future_field":123}"#;
        assert!(matches!(
            serde_json::from_str::<Request>(json).unwrap(),
            Request::Ping
        ));
    }
}
