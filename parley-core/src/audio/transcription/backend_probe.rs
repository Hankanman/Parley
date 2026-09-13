//! Runtime whisper-backend discovery and selection (issue #56 spike).
//!
//! Compiled only under the `backend_probe` Cargo feature, which is **off by
//! default** — this module is inert unless a build explicitly opts in. It is
//! not wired into the live recording path: `WhisperEngine` (in-process,
//! `whisper_engine/whisper_engine.rs`) remains the only transcription path
//! actually used today. This exists to prototype the selection logic
//! described in `docs/transcription-backends.md` ahead of that migration.
//!
//! # What this does
//!
//! Enumerates candidate `whisper-helper-{cuda,vulkan,cpu}` sidecar binaries
//! next to the running executable, launches each with a `probe` request (see
//! `whisper-protocol::Request::Probe`), and picks the first that succeeds in
//! priority order cuda → vulkan → cpu. The result is intended to be
//! persisted alongside the rest of the transcription config, with a manual
//! override — see [`BackendChoice`] and [`get_transcription_backends`].
//!
//! # What this does not do (yet)
//!
//! - No caller in `lib.rs` invokes this at startup or exposes it as a Tauri
//!   command; registering `get_transcription_backends` in
//!   `invoke_handler![]` is a follow-up once a settings UI exists for it.
//! - No sidecar binaries actually ship next to the app yet — `whisper-helper`
//!   is a standalone workspace crate today (`cargo build -p whisper-helper`).
//! - Persistence of the manual override piggybacks on
//!   `WHISPER_MODEL_CATALOG`'s config file conventions in spirit only; the
//!   actual read/write wiring is left to the implementation phase.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use whisper_protocol::{Request, Response};

/// Priority order for automatic selection: prefer the fastest backend that
/// actually works on this machine.
const CANDIDATE_ORDER: [&str; 3] = ["cuda", "vulkan", "cpu"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendCandidate {
    pub backend: String,
    pub binary_path: String,
    /// `None` when the binary wasn't found next to the executable at all.
    pub available: bool,
    pub probe_ok: Option<bool>,
    pub detail: String,
    pub probe_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendSelection {
    pub candidates: Vec<BackendCandidate>,
    /// The backend `backend_probe` would pick automatically (first
    /// `probe_ok: Some(true)` in `CANDIDATE_ORDER`), if any.
    pub recommended: Option<String>,
    /// A user's manual override, if one has been persisted. When present,
    /// callers should prefer this over `recommended`.
    pub manual_override: Option<String>,
}

/// Build the expected path for a given backend's sidecar binary, sitting
/// next to `exe_dir` (mirrors how `llama-helper` is located relative to the
/// Tauri app's resource/exe directory today).
fn candidate_path(exe_dir: &Path, backend: &str) -> PathBuf {
    exe_dir.join(format!("whisper-helper-{backend}"))
}

/// Run one candidate's `probe` (or, if it can't even be spawned, report
/// that). `model_path` is forwarded as-is to `Request::Probe` — `None` falls
/// back to a ping-only check (see `whisper-protocol`'s doc comment on
/// `Request::Probe`).
fn probe_one(exe_dir: &Path, backend: &str, model_path: Option<&str>) -> BackendCandidate {
    let path = candidate_path(exe_dir, backend);
    if !path.is_file() {
        return BackendCandidate {
            backend: backend.to_string(),
            binary_path: path.display().to_string(),
            available: false,
            probe_ok: None,
            detail: "binary not found".to_string(),
            probe_ms: None,
        };
    }

    let start = Instant::now();
    let result = (|| -> anyhow::Result<Response> {
        let mut child = Command::new(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child.stdin.as_mut().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let request = Request::Probe {
            model_path: model_path.map(|s| s.to_string()),
        };
        let line = serde_json::to_string(&request)?;
        use std::io::Write;
        writeln!(stdin, "{line}")?;

        use std::io::{BufRead, BufReader};
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let mut reader = BufReader::new(stdout);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line)?;
        let response: Response = serde_json::from_str(resp_line.trim())?;

        // Best-effort cleanup; the child exits on its own once stdin closes
        // (dropping `child` closes the pipe), but don't block indefinitely
        // waiting on a stuck process during a probe.
        let _ = child.kill();

        Ok(response)
    })();

    let probe_ms = start.elapsed();
    match result {
        Ok(Response::ProbeResult {
            decode_ok, detail, ..
        }) => BackendCandidate {
            backend: backend.to_string(),
            binary_path: path.display().to_string(),
            available: true,
            probe_ok: decode_ok.or(Some(true)), // ping-only probe still counts as "runnable"
            detail,
            probe_ms: Some(probe_ms.as_millis() as u64),
        },
        Ok(other) => BackendCandidate {
            backend: backend.to_string(),
            binary_path: path.display().to_string(),
            available: true,
            probe_ok: Some(false),
            detail: format!("unexpected response to probe: {other:?}"),
            probe_ms: Some(probe_ms.as_millis() as u64),
        },
        Err(e) => BackendCandidate {
            backend: backend.to_string(),
            binary_path: path.display().to_string(),
            available: true,
            probe_ok: Some(false),
            detail: format!("probe failed: {e}"),
            probe_ms: Some(probe_ms.as_millis() as u64),
        },
    }
}

/// Enumerate and probe all candidate sidecars, returning them alongside the
/// recommended automatic choice. `timeout` bounds each individual probe
/// (unused in this prototype — `probe_one` is synchronous and the `Probe`
/// request itself is meant to complete in ~1s; a production version should
/// wrap the spawn+read in a thread with a hard timeout so a hung sidecar
/// can't block the whole scan).
pub fn scan_backends(exe_dir: &Path, model_path: Option<&str>, _timeout: Duration) -> BackendSelection {
    let candidates: Vec<BackendCandidate> = CANDIDATE_ORDER
        .iter()
        .map(|backend| probe_one(exe_dir, backend, model_path))
        .collect();

    let recommended = CANDIDATE_ORDER.iter().find_map(|backend| {
        candidates
            .iter()
            .find(|c| c.backend == *backend && c.probe_ok == Some(true))
            .map(|c| c.backend.clone())
    });

    BackendSelection {
        candidates,
        recommended,
        manual_override: None,
    }
}

/// Tauri-command-shaped entry point for a future settings UI. Not currently
/// registered in `invoke_handler![]` — see module doc comment.
///
/// `model_path` should be the tiny/base model's path if one is downloaded
/// (gives a real decode_ok signal); `None` degrades to ping-only probing.
pub fn get_transcription_backends(model_path: Option<&str>) -> anyhow::Result<BackendSelection> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?
        .to_path_buf();
    Ok(scan_backends(&exe_dir, model_path, Duration::from_secs(5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_reports_unavailable() {
        let dir = std::env::temp_dir();
        let candidate = probe_one(&dir, "cuda-definitely-not-here", None);
        assert!(!candidate.available);
        assert_eq!(candidate.probe_ok, None);
    }

    #[test]
    fn candidate_path_matches_expected_naming() {
        let dir = PathBuf::from("/opt/app");
        assert_eq!(
            candidate_path(&dir, "vulkan"),
            PathBuf::from("/opt/app/whisper-helper-vulkan")
        );
    }
}
