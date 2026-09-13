// audio/transcription/partial_worker.rs
//
// Streaming partial-transcription task. Runs ALONGSIDE the authoritative
// VAD-final transcription worker and is purely additive: it decodes
// in-progress utterance audio periodically and emits `transcript-partial`
// preview events, which the frontend renders distinctly and discards once the
// real (final) transcript for that segment arrives. Nothing here is saved, and
// the final path is entirely independent — if partial decoding fails or lags,
// the committed transcript is unaffected.

use std::collections::HashMap;
use std::sync::Mutex;

use log::{debug, info};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::audio::recording_state::{DeviceType, PartialAudioChunk};
use crate::events::{EventSinkExt, SharedEventSink};

/// Sliding-window size for partial decoding: only the trailing 6s of an
/// in-progress utterance is re-decoded on each tick, instead of the whole
/// (up to 12s) buffer. Redecoding the full buffer every ~1.2s of new audio
/// is O(n^2) over the utterance's lifetime and, run concurrently with the
/// final worker on the same Whisper context, can push CPU-only hardware
/// past real time (issue #26).
const PARTIAL_WINDOW_SAMPLES: usize = 6 * 16_000;

/// Skip a partial decode entirely once the final-path backlog passes this
/// depth — partials are best-effort preview only, and burning CPU on them
/// while the authoritative path is already behind makes the backlog worse.
const SKIP_PARTIAL_QUEUE_DEPTH: usize = 2;

/// Handle to the currently-running partial-decode task, so the command
/// layer (which owns recording stop/start sequencing) can await or abort it
/// instead of leaving it detached. A detached task can otherwise emit a
/// late `transcript-partial` after `recording-stopped` has already fired,
/// re-populating an overlay the frontend believes is done with.
static PARTIAL_TASK: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Take (and clear) the stored handle for the currently-running (or most
/// recently started) partial-decode task. Returns `None` if no task has
/// been started, or it was already taken.
pub fn take_partial_task_handle() -> Option<JoinHandle<()>> {
    PARTIAL_TASK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// Return the last `window` samples of `samples` (or all of them, if
/// shorter). Used to bound partial-decode work to a fixed-size sliding
/// window instead of the whole growing utterance buffer.
fn sliding_window(samples: &[f32], window: usize) -> &[f32] {
    let len = samples.len();
    if len <= window {
        samples
    } else {
        &samples[len - window..]
    }
}

#[derive(Debug, Serialize, Clone)]
struct PartialUpdate {
    /// "mic" | "system"
    source: String,
    /// Stabilized preview text (may be empty to clear the current partial).
    text: String,
    utterance_id: u64,
}

/// Per-source LocalAgreement stabilization state.
#[derive(Default)]
struct SourceState {
    /// Utterance the state belongs to; a change resets stabilization.
    utterance_id: u64,
    /// Word list of the previous decode hypothesis for this utterance.
    prev_words: Vec<String>,
    /// Words committed (agreed by two consecutive hypotheses) so far. Grows
    /// monotonically within an utterance — the emitted text never shrinks.
    committed: Vec<String>,
}

/// Longest common prefix of two word slices.
fn common_prefix_len(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn source_str(source: DeviceType) -> &'static str {
    match source {
        DeviceType::Microphone => "mic",
        DeviceType::System => "system",
    }
}

/// Spawn the streaming partial-decode task. It owns the receiver end of the
/// pipeline's partial channel and emits `transcript-partial` events.
///
/// The task's `JoinHandle` is stashed in a module-level slot
/// (`take_partial_task_handle`) rather than returned, so the command layer
/// can retrieve it later — e.g. at recording stop — to await or abort it
/// instead of leaving it fully detached. A fully-detached task could
/// otherwise emit a late `transcript-partial` after `recording-stopped` has
/// already fired, re-populating an overlay the frontend believes is done
/// with.
pub fn start_partial_decode_task(
    sink: SharedEventSink,
    mut receiver: mpsc::UnboundedReceiver<PartialAudioChunk>,
) {
    let handle = tokio::spawn(async move {
        info!("🎬 Streaming partial-decode task started");

        // This task decodes off the same shared Whisper engine as the final
        // worker (start_transcription_task), independently and concurrently.
        // Hold the live-transcription lease for its whole lifetime too, so a
        // batch job can't swap/unload the model mid-decode here either. See
        // `whisper_engine::lease`.
        let _live_engine_lease = crate::whisper_engine::LIVE_ENGINE_LEASE.acquire_live();

        // Reduced thread budget for partial decodes (issue #26): partials
        // run concurrently with the final worker on the same Whisper
        // context, so on CPU-only hardware giving them the full adaptive
        // thread count starves the authoritative path. Computed once here
        // (hardware doesn't change mid-recording) and applied to every
        // decode via `TranscribeOptions::max_threads`.
        let adaptive_threads = crate::audio::HardwareProfile::detect()
            .get_whisper_config()
            .max_threads
            .unwrap_or(4) as i32;
        let partial_max_threads = (adaptive_threads / 2).max(1);

        let mut states: HashMap<DeviceType, SourceState> = HashMap::new();

        while let Some(mut chunk) = receiver.recv().await {
            // Latest-wins: if the pipeline queued several snapshots while a
            // decode was in flight, skip to the newest one PER SOURCE so we
            // never backlog stale previews. Drain everything immediately
            // available, keeping the last chunk seen for each source.
            let mut latest: HashMap<DeviceType, PartialAudioChunk> = HashMap::new();
            latest.insert(chunk.source, chunk.clone());
            while let Ok(next) = receiver.try_recv() {
                chunk = next.clone();
                latest.insert(next.source, next);
            }
            let _ = chunk; // last value already captured in `latest`

            // Best-effort: if the final (authoritative) transcription path
            // already has a meaningful backlog, skip partial decoding this
            // tick entirely rather than adding more CPU contention on the
            // same Whisper context (issue #26).
            if super::queue::queue_depth() > SKIP_PARTIAL_QUEUE_DEPTH {
                debug!(
                    "Skipping partial decode tick: transcription backlog is {} segments",
                    super::queue::queue_depth()
                );
                continue;
            }

            for (source, chunk) in latest {
                if let Err(e) =
                    decode_and_emit(&sink, &mut states, source, chunk, partial_max_threads).await
                {
                    debug!("partial decode skipped for {:?}: {}", source, e);
                }
            }
        }

        // Channel closed: the pipeline dropped its partial sender (recording
        // stopped). Emit an explicit empty-text update for every source that
        // still had a non-empty preview showing, so the frontend's "empty
        // text clears" contract is actually exercised instead of leaving a
        // stale overlay up until the next recording overwrites it.
        for (source, state) in states {
            if !state.committed.is_empty() {
                let update = PartialUpdate {
                    source: source_str(source).to_string(),
                    text: String::new(),
                    utterance_id: state.utterance_id,
                };
                let _ = sink.emit_event("transcript-partial", &update);
            }
        }

        info!("🎬 Streaming partial-decode task exiting");
    });

    // Stash the handle so the command layer can retrieve it later (e.g. at
    // recording stop) via `take_partial_task_handle` to await or abort it,
    // instead of the task staying fully detached. Any previous handle left
    // here is stale (that task has either finished or was already taken) —
    // dropping a finished JoinHandle is a no-op, and dropping a still-running
    // one just detaches it rather than aborting it.
    *PARTIAL_TASK.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
}

async fn decode_and_emit(
    sink: &SharedEventSink,
    states: &mut HashMap<DeviceType, SourceState>,
    source: DeviceType,
    chunk: PartialAudioChunk,
    max_threads: i32,
) -> Result<(), String> {
    // Grab the shared whisper engine (same instance the final worker uses).
    let engine = {
        let guard = crate::whisper_engine::models::WHISPER_ENGINE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    };
    let Some(engine) = engine else {
        return Err("engine not loaded".into());
    };
    if !engine.is_model_loaded().await {
        return Err("model not loaded".into());
    }

    let language = crate::utils::get_language_preference_internal();
    // Only re-decode the trailing PARTIAL_WINDOW_SAMPLES of the utterance
    // (issue #26) instead of the whole up-to-12s buffer the pipeline hands
    // us in `chunk.samples` — decoding the full buffer on every ~1.2s tick
    // is O(n^2) over an utterance's lifetime.
    //
    // Thread budget (issue #26): partials run concurrently with the final
    // worker on the same Whisper context, so they're capped at half the
    // hardware-adaptive thread count (computed once by the caller) — this
    // leaves the authoritative final path headroom on CPU-only hardware.
    // The queue-depth skip above (`SKIP_PARTIAL_QUEUE_DEPTH`) is the other
    // half of the mitigation: once the final path is behind, partials stop
    // competing with it for CPU entirely.
    let windowed: Vec<f32> = sliding_window(&chunk.samples, PARTIAL_WINDOW_SAMPLES).to_vec();
    // No context prompt: a partial is a fresh best-effort decode of the
    // in-progress utterance; cross-segment context is a final-path concern.
    // `greedy: true` skips beam search — partials are discarded previews,
    // not the committed transcript, so the speed win is worth the lower
    // per-decode quality here.
    let options = crate::whisper_engine::TranscribeOptions {
        max_threads: Some(max_threads),
        greedy: true,
        purpose: crate::whisper_engine::DecodePurpose::Partial,
    };
    let (text, _conf, _partial) = engine
        .transcribe_audio_with_confidence_opts(windowed, language, None, options)
        .await
        .map_err(|e| e.to_string())?;

    let state = states.entry(source).or_default();
    if state.utterance_id != chunk.utterance_id {
        // New utterance — reset stabilization.
        *state = SourceState {
            utterance_id: chunk.utterance_id,
            prev_words: Vec::new(),
            committed: Vec::new(),
        };
    }

    let cur_words: Vec<String> = text.split_whitespace().map(|s| s.to_string()).collect();

    // LocalAgreement-2: commit the prefix agreed by the last two hypotheses.
    // The committed prefix only ever grows, so the preview never flickers
    // backward; the still-unstable tail is withheld until the next decode
    // confirms it (the classic stability-vs-latency tradeoff).
    let agreed = common_prefix_len(&state.prev_words, &cur_words);
    if agreed > state.committed.len() {
        state.committed = cur_words[..agreed].to_vec();
    }
    state.prev_words = cur_words;

    if state.committed.is_empty() {
        return Ok(()); // nothing stable to show yet
    }

    let update = PartialUpdate {
        source: source_str(source).to_string(),
        text: state.committed.join(" "),
        utterance_id: chunk.utterance_id,
    };
    let _ = sink.emit_event("transcript-partial", &update);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{common_prefix_len, sliding_window};

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(|w| w.to_string()).collect()
    }

    #[test]
    fn sliding_window_passes_short_buffers_through() {
        let samples = vec![1.0_f32, 2.0, 3.0];
        assert_eq!(sliding_window(&samples, 10), &samples[..]);
        assert_eq!(sliding_window(&samples, 3), &samples[..]);
    }

    #[test]
    fn sliding_window_trims_to_the_tail() {
        let samples: Vec<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(sliding_window(&samples, 4), &[6.0, 7.0, 8.0, 9.0]);
    }

    #[test]
    fn sliding_window_zero_window_yields_empty() {
        let samples = vec![1.0_f32, 2.0, 3.0];
        assert_eq!(sliding_window(&samples, 0), &[] as &[f32]);
    }

    #[test]
    fn prefix_of_growing_hypotheses() {
        // Second decode extends the first: agreed prefix is the whole first.
        assert_eq!(
            common_prefix_len(&words("let us start the"), &words("let us start the meeting")),
            4
        );
    }

    #[test]
    fn prefix_stops_at_first_divergence() {
        // whisper revised "there" → "their"; only "we think" is agreed.
        assert_eq!(
            common_prefix_len(&words("we think there is"), &words("we think their is time")),
            2
        );
    }

    #[test]
    fn no_agreement_and_empty() {
        assert_eq!(common_prefix_len(&words("hello world"), &words("goodbye now")), 0);
        assert_eq!(common_prefix_len(&[], &words("anything")), 0);
        assert_eq!(common_prefix_len(&words("anything"), &[]), 0);
    }
}
