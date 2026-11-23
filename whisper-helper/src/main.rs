//! `whisper-helper`: a stdio JSON-lines whisper.cpp transcription sidecar.
//!
//! Prototype for issue #56 (runtime backend selection). Modelled directly on
//! `llama-helper/src/main.rs`'s request loop and lifecycle: read one
//! [`whisper_protocol::Request`] per line from stdin, reply with exactly one
//! terminal [`whisper_protocol::Response`] line on stdout, serve until stdin
//! closes or `Shutdown` arrives. Params-building and confidence scoring are
//! shared with the in-process engine via the `whisper-core` crate rather
//! than duplicated here — see that crate's doc comment.
//!
//! Not wired into the live recording path yet (see
//! `docs/transcription-backends.md`); this binary is a standalone
//! measurement + design vehicle.

use std::io::{self, BufRead, Write};
use std::time::Instant;

use anyhow::{anyhow, Result};
use base64::Engine;
use whisper_core::{DecodeConfig, TranscribeOptions};
use whisper_protocol::{Request, Response, Segment};
use whisper_rs::{WhisperContext, WhisperContextParameters};

/// Which backend this binary was compiled for — self-reported in `Probe`
/// responses and used by `backend_probe` (the future selection logic) to
/// label results without having to parse the binary's filename.
fn compiled_backend() -> &'static str {
    if cfg!(feature = "vulkan") {
        "vulkan"
    } else {
        "cpu"
    }
}

struct State {
    ctx: Option<WhisperContext>,
    decode_config: DecodeConfig,
}

impl State {
    fn new() -> Self {
        Self {
            ctx: None,
            decode_config: DecodeConfig::default(),
        }
    }

    fn load_model(&mut self, path: &str) -> Result<()> {
        let context_param = WhisperContextParameters {
            use_gpu: cfg!(feature = "vulkan"),
            gpu_device: 0,
            ..Default::default()
        };
        let ctx = WhisperContext::new_with_params(path, context_param)
            .map_err(|e| anyhow!("failed to load model {path}: {e}"))?;
        self.ctx = Some(ctx);
        Ok(())
    }

    fn transcribe(
        &self,
        samples: Vec<f32>,
        language: Option<&str>,
        context_prompt: Option<&str>,
        max_threads: Option<i32>,
        greedy: bool,
    ) -> Result<whisper_core::TranscriptionOutcome> {
        let ctx = self
            .ctx
            .as_ref()
            .ok_or_else(|| anyhow!("no model loaded"))?;
        whisper_core::transcribe_pcm16k(
            ctx,
            samples,
            language,
            context_prompt,
            self.decode_config,
            TranscribeOptions {
                max_threads,
                greedy,
            },
        )
    }
}

fn send_response(response: &Response) -> Result<()> {
    let json = serde_json::to_string(response)?;
    println!("{json}");
    io::stdout().flush()?;
    Ok(())
}

/// A short burst of near-silent samples (1s @ 16kHz) used by `Probe` when no
/// real model/audio is available — enough to exercise `full()` end-to-end
/// (model load + a decode pass) without needing a recorded clip on disk.
fn synthetic_probe_samples() -> Vec<f32> {
    vec![0.0f32; whisper_core::WHISPER_MIN_INPUT_SAMPLES]
}

fn handle_request(state: &mut State, request: Request) -> Result<Response> {
    match request {
        Request::LoadModel { path } => {
            let start = Instant::now();
            match state.load_model(&path) {
                Ok(()) => Ok(Response::Loaded {
                    error: None,
                    load_ms: start.elapsed().as_millis() as u64,
                }),
                Err(e) => Ok(Response::Loaded {
                    error: Some(e.to_string()),
                    load_ms: start.elapsed().as_millis() as u64,
                }),
            }
        }
        Request::Transcribe {
            samples_b64,
            language,
            context_prompt,
            max_threads,
            greedy,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(samples_b64)
                .map_err(|e| anyhow!("invalid base64 samples: {e}"))?;
            if bytes.len() % 4 != 0 {
                return Ok(Response::Transcribed {
                    text: String::new(),
                    confidence: 0.0,
                    is_partial: false,
                    segments: vec![],
                    error: Some("sample buffer length is not a multiple of 4 bytes (f32le)".into()),
                    decode_ms: 0,
                });
            }
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();

            let start = Instant::now();
            match state.transcribe(
                samples,
                language.as_deref(),
                context_prompt.as_deref(),
                max_threads,
                greedy.unwrap_or(false),
            ) {
                Ok(outcome) => Ok(Response::Transcribed {
                    text: outcome.text,
                    confidence: outcome.confidence,
                    is_partial: outcome.is_partial,
                    segments: outcome
                        .segments
                        .into_iter()
                        .map(|s| Segment {
                            text: s.text,
                            confidence: s.confidence,
                            start_ms: s.start_ms,
                            end_ms: s.end_ms,
                        })
                        .collect(),
                    error: None,
                    decode_ms: start.elapsed().as_millis() as u64,
                }),
                Err(e) => Ok(Response::Transcribed {
                    text: String::new(),
                    confidence: 0.0,
                    is_partial: false,
                    segments: vec![],
                    error: Some(e.to_string()),
                    decode_ms: start.elapsed().as_millis() as u64,
                }),
            }
        }
        Request::Unload => {
            state.ctx = None;
            Ok(Response::Unloaded)
        }
        Request::Ping => Ok(Response::Pong),
        Request::Probe { model_path } => {
            let backend = compiled_backend().to_string();
            match model_path {
                None => Ok(Response::ProbeResult {
                    backend,
                    decode_ok: None,
                    detail: "no model available; probe verified process spawn + ping only".into(),
                }),
                Some(path) => {
                    let mut probe_state = State::new();
                    let load_result = probe_state.load_model(&path);
                    if let Err(e) = load_result {
                        return Ok(Response::ProbeResult {
                            backend,
                            decode_ok: Some(false),
                            detail: format!("model load failed: {e}"),
                        });
                    }
                    match probe_state.transcribe(synthetic_probe_samples(), None, None, None, true)
                    {
                        Ok(_) => Ok(Response::ProbeResult {
                            backend,
                            decode_ok: Some(true),
                            detail: "1s synthetic decode succeeded".into(),
                        }),
                        Err(e) => Ok(Response::ProbeResult {
                            backend,
                            decode_ok: Some(false),
                            detail: format!("synthetic decode failed: {e}"),
                        }),
                    }
                }
            }
        }
        Request::Shutdown => Ok(Response::Goodbye),
    }
}

fn main() -> Result<()> {
    // See llama-helper's main.rs: writing to a stdout whose reader has gone
    // away must not panic on EPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    eprintln!(
        "whisper-helper ({}) starting",
        compiled_backend()
    );

    let mut state = State::new();
    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut buffer = String::new();

    loop {
        buffer.clear();
        match stdin_lock.read_line(&mut buffer) {
            Ok(0) => {
                eprintln!("EOF received, shutting down");
                break;
            }
            Ok(_) => {
                let line = buffer.trim();
                if line.is_empty() {
                    continue;
                }
                let is_shutdown;
                match serde_json::from_str::<Request>(line) {
                    Ok(request) => {
                        is_shutdown = matches!(request, Request::Shutdown);
                        match handle_request(&mut state, request) {
                            Ok(response) => send_response(&response)?,
                            Err(e) => send_response(&Response::Error {
                                message: e.to_string(),
                            })?,
                        }
                    }
                    Err(e) => {
                        is_shutdown = false;
                        eprintln!("failed to parse request: {e}");
                        send_response(&Response::Error {
                            message: format!("invalid request: {e}"),
                        })?;
                    }
                }
                if is_shutdown {
                    break;
                }
            }
            Err(e) => {
                eprintln!("error reading stdin: {e}");
                break;
            }
        }
    }

    eprintln!("whisper-helper exiting");
    Ok(())
}
