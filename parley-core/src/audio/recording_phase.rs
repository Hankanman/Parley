// audio/recording_phase.rs
//
// Canonical recording-state machine (GitHub issue #57, slice 1): a single
// Rust-owned source of truth for recording lifecycle state, broadcast to the
// frontend via one `recording-state` event instead of the frontend polling
// `get_recording_state` every 500ms.
//
// This module owns only the *phase* and the handful of fields that are
// phase-scoped (when the current session started recording, its meeting
// name/folder, the last fatal error). Live numeric fields (transcription
// queue depth, active/pause durations) come from the caller —
// `recording_commands.rs`, which owns `RECORDING_MANAGER` — at the moment a
// snapshot is built or emitted; this module never reaches for them itself,
// so it stays independently unit-testable.

use crate::events::EventSinkExt;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Recording lifecycle phase, owned entirely by the Rust side. The frontend
/// maps this onto its own `RecordingStatus` enum rather than deriving status
/// from polled booleans.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPhase {
    Idle,
    Starting,
    Recording,
    Paused,
    Stopping,
    Finalising,
    Error,
}

/// Full state snapshot broadcast to the frontend on every phase change (as
/// the `recording-state` event payload) and returned by the
/// `get_recording_state` command.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RecordingSnapshot {
    pub phase: RecordingPhase,
    /// Unix ms the current session entered `Recording` (None once idle).
    /// The frontend ticks elapsed time client-side from this instead of
    /// polling a duration.
    pub started_at_ms: Option<u64>,
    pub active_duration_secs: Option<f64>,
    pub total_pause_secs: f64,
    pub meeting_name: Option<String>,
    pub folder_path: Option<String>,
    /// The `meetings` row id for the in-progress session (issue #57 slice
    /// 2), set as soon as `start_recording*` mints it — before the DB
    /// insert even completes, since the id is generated first and used
    /// either way. `None` once idle.
    pub meeting_id: Option<String>,
    pub chunks_in_queue: usize,
    pub error: Option<String>,
    /// Monotonically increasing with every emitted snapshot, so a listener
    /// that ends up observing two snapshots out of order (unlikely over a
    /// single Tauri event channel, but cheap to guard against) can tell
    /// which one is newer.
    pub seq: u64,
}

struct PhaseState {
    phase: RecordingPhase,
    started_at_ms: Option<u64>,
    meeting_name: Option<String>,
    folder_path: Option<String>,
    meeting_id: Option<String>,
    error: Option<String>,
}

static PHASE_STATE: Mutex<PhaseState> = Mutex::new(PhaseState {
    phase: RecordingPhase::Idle,
    started_at_ms: None,
    meeting_name: None,
    folder_path: None,
    meeting_id: None,
    error: None,
});

static SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure transition check: is `current -> target` an allowed move in the
/// recording lifecycle? Kept separate from `set_phase` so the transition
/// table is unit-testable without touching the static holder or emitting
/// anything.
///
/// Transition table:
///
/// | from        | to                                    |
/// |-------------|----------------------------------------|
/// | Idle        | Starting                                |
/// | Starting    | Recording, Idle (start failed)          |
/// | Recording   | Paused, Stopping                        |
/// | Paused      | Recording, Stopping                     |
/// | Stopping    | Finalising                               |
/// | Finalising  | Idle                                    |
/// | Error       | Stopping (auto-stop), Idle              |
/// | *           | Error (fatal error, any phase)          |
pub fn next_phase(
    current: RecordingPhase,
    target: RecordingPhase,
) -> Result<RecordingPhase, String> {
    use RecordingPhase::*;

    let allowed = target == Error
        || matches!(
            (current, target),
            (Idle, Starting)
                | (Starting, Recording)
                | (Starting, Idle)
                | (Recording, Paused)
                | (Paused, Recording)
                | (Recording, Stopping)
                | (Paused, Stopping)
                | (Stopping, Finalising)
                | (Finalising, Idle)
                | (Error, Stopping)
                | (Error, Idle)
        );

    if allowed {
        Ok(target)
    } else {
        Err(format!(
            "invalid recording phase transition: {:?} -> {:?}",
            current, target
        ))
    }
}

/// Current phase, read without building a full snapshot — cheap and sync,
/// used by the tray to decide whether a click should start, stop, or be a
/// no-op.
pub fn current_phase() -> RecordingPhase {
    PHASE_STATE.lock().unwrap().phase
}

/// Move to `phase`, stamping/clearing the phase-scoped fields that follow
/// from the transition, and bump `seq`.
fn apply(phase: RecordingPhase) {
    let mut state = PHASE_STATE.lock().unwrap();
    if let Err(e) = next_phase(state.phase, phase) {
        // Never block a real transition on this — the table above documents
        // the intended lifecycle, but the actual command flow in
        // recording_commands.rs remains the real source of truth for what's
        // allowed to happen. Just flag the mismatch for whoever's debugging
        // the state machine.
        log::warn!("recording_phase: {} (applying anyway)", e);
    }
    match phase {
        RecordingPhase::Recording if state.started_at_ms.is_none() => {
            state.started_at_ms = Some(now_ms());
        }
        RecordingPhase::Idle => {
            state.started_at_ms = None;
            state.meeting_name = None;
            state.folder_path = None;
            state.meeting_id = None;
            state.error = None;
        }
        _ => {}
    }
    state.phase = phase;
    SEQ.fetch_add(1, Ordering::SeqCst);
}

/// Record the meeting name / folder path / meeting_id for the in-progress
/// session. Set once `start_recording` knows them; read back by
/// `get_recording_state` and included in every subsequent `recording-state`
/// emit until the phase returns to `Idle`.
pub fn set_meeting_info(
    meeting_name: Option<String>,
    folder_path: Option<String>,
    meeting_id: Option<String>,
) {
    let mut state = PHASE_STATE.lock().unwrap();
    state.meeting_name = meeting_name;
    state.folder_path = folder_path;
    state.meeting_id = meeting_id;
}

/// Record the last fatal error's user-facing message. Cleared automatically
/// on the next transition back to `Idle`.
pub fn set_error_message(message: Option<String>) {
    PHASE_STATE.lock().unwrap().error = message;
}

/// Build the full outward-facing snapshot. `active_duration_secs`,
/// `total_pause_secs` and `chunks_in_queue` are supplied by the caller
/// (`recording_commands.rs`, which owns `RECORDING_MANAGER` and the
/// transcription queue) — this module only tracks the phase-scoped fields.
pub fn build_snapshot(
    active_duration_secs: Option<f64>,
    total_pause_secs: f64,
    chunks_in_queue: usize,
) -> RecordingSnapshot {
    let state = PHASE_STATE.lock().unwrap();
    RecordingSnapshot {
        phase: state.phase,
        started_at_ms: state.started_at_ms,
        active_duration_secs,
        total_pause_secs,
        meeting_name: state.meeting_name.clone(),
        folder_path: state.folder_path.clone(),
        meeting_id: state.meeting_id.clone(),
        chunks_in_queue,
        error: state.error.clone(),
        seq: SEQ.load(Ordering::SeqCst),
    }
}

/// Move to `phase` and emit the canonical `recording-state` event with the
/// resulting snapshot, merged with live duration/queue data from the
/// caller. This is the only place `recording-state` is emitted — every
/// `RecordingPhase` transition in `recording_commands.rs` goes through it,
/// so `seq` is a true total order over every transition regardless of which
/// command triggered it.
pub fn set_phase(
    sink: &dyn crate::events::EventSink,
    phase: RecordingPhase,
    active_duration_secs: Option<f64>,
    total_pause_secs: f64,
    chunks_in_queue: usize,
) {
    apply(phase);
    let snapshot = build_snapshot(active_duration_secs, total_pause_secs, chunks_in_queue);
    log::debug!(
        "recording_phase -> {:?} (seq {})",
        snapshot.phase,
        snapshot.seq
    );
    if let Err(e) = sink.emit_event("recording-state", &snapshot) {
        log::warn!("Failed to emit recording-state: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RecordingPhase::*;

    #[test]
    fn happy_path_transitions_are_allowed() {
        assert_eq!(next_phase(Idle, Starting), Ok(Starting));
        assert_eq!(next_phase(Starting, Recording), Ok(Recording));
        assert_eq!(next_phase(Recording, Paused), Ok(Paused));
        assert_eq!(next_phase(Paused, Recording), Ok(Recording));
        assert_eq!(next_phase(Recording, Stopping), Ok(Stopping));
        assert_eq!(next_phase(Paused, Stopping), Ok(Stopping));
        assert_eq!(next_phase(Stopping, Finalising), Ok(Finalising));
        assert_eq!(next_phase(Finalising, Idle), Ok(Idle));
    }

    #[test]
    fn a_failed_start_returns_to_idle() {
        assert_eq!(next_phase(Starting, Idle), Ok(Idle));
    }

    #[test]
    fn any_phase_can_go_fatal() {
        for phase in [Idle, Starting, Recording, Paused, Stopping, Finalising, Error] {
            assert_eq!(next_phase(phase, Error), Ok(Error));
        }
    }

    #[test]
    fn error_recovers_via_the_stop_flow_or_directly_to_idle() {
        assert_eq!(next_phase(Error, Stopping), Ok(Stopping));
        assert_eq!(next_phase(Error, Idle), Ok(Idle));
    }

    #[test]
    fn skipping_a_phase_is_rejected() {
        assert!(next_phase(Idle, Recording).is_err());
        assert!(next_phase(Recording, Finalising).is_err());
        assert!(next_phase(Idle, Stopping).is_err());
        assert!(next_phase(Idle, Finalising).is_err());
    }

    // The only test that touches the process-global PHASE_STATE — kept to a
    // single test so parallel test execution can never race it against
    // another test mutating the same static.
    #[test]
    fn apply_stamps_started_at_on_entering_recording_and_clears_on_idle() {
        apply(Idle);
        assert!(PHASE_STATE.lock().unwrap().started_at_ms.is_none());

        apply(Starting);
        apply(Recording);
        assert!(PHASE_STATE.lock().unwrap().started_at_ms.is_some());

        apply(Idle);
        assert!(PHASE_STATE.lock().unwrap().started_at_ms.is_none());

        // set_phase emits `recording-state` with a strictly increasing seq
        // on every call; exercised here (rather than in its own #[test])
        // since it touches the same process-global PHASE_STATE/SEQ statics.
        use crate::events::RecordingSink;
        let sink = RecordingSink::new();
        set_phase(&sink, Idle, None, 0.0, 0);
        set_phase(&sink, Idle, None, 0.0, 0);
        let payloads = sink.payloads("recording-state");
        assert_eq!(payloads.len(), 2);
        let seq_before = payloads[0]["seq"].as_u64().unwrap();
        let seq_after = payloads[1]["seq"].as_u64().unwrap();
        assert!(seq_after > seq_before);
    }
}
