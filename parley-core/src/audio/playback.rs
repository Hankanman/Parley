//! Native audio output for transcript-segment clip playback.
//!
//! The AppImage bundles the GStreamer *core* libraries but none of its
//! plugins, and points WebKit at that empty plugin set — so every webview
//! audio path (both `<audio>` and the Web Audio API) fails with
//! "element appsink not found". Rather than fix the webview media stack, we
//! play the short verification clips natively here, which also keeps the app
//! self-contained and consistent with its native PipeWire capture.
//!
//! rodio's `OutputStream` owns a `!Send` cpal stream, so it can't live in a
//! global or move between threads. We park it forever on a dedicated thread
//! and hand out the (Send) `OutputStreamHandle`; sinks are built from that
//! handle on demand. Only one clip plays at a time — starting a new one
//! replaces the previous.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink};

use crate::events::{EventSinkExt, SharedEventSink};

/// Emitted (payload: the playback generation) when a clip finishes on its own,
/// so the UI can clear the "playing" state. Not emitted when playback is
/// replaced or explicitly stopped.
pub const PLAYBACK_ENDED_EVENT: &str = "segment-playback-ended";

/// Monotonic token identifying the active playback. Bumped on every play and
/// stop so a finishing clip only announces its end if it's still the current
/// one.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Player {
    handle: OutputStreamHandle,
    current: Mutex<Option<Arc<Sink>>>,
}

/// Lazily bring up the audio output thread. Only a successful outcome is
/// cached: a failure (no output device available at the time, e.g. a
/// Bluetooth sink not yet connected) is returned as an error but NOT
/// memoized, so the next call retries device creation from scratch instead
/// of repeating the same stale error forever.
fn player() -> Result<&'static Player, String> {
    static PLAYER: OnceLock<Player> = OnceLock::new();
    // Serializes concurrent init attempts so we don't spawn multiple output
    // threads racing to win `PLAYER.set(..)`.
    static INIT_LOCK: Mutex<()> = Mutex::new(());

    if let Some(p) = PLAYER.get() {
        return Ok(p);
    }

    let _guard = INIT_LOCK.lock().unwrap();
    // Another thread may have finished initializing while we waited for the lock.
    if let Some(p) = PLAYER.get() {
        return Ok(p);
    }

    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("parley-audio-out".into())
        .spawn(move || match OutputStream::try_default() {
            Ok((stream, handle)) => {
                let _ = tx.send(Ok(handle));
                // `stream` is `!Send` and must outlive every sink built
                // from its handle; keep it alive here for the app's life.
                let _keep_alive = stream;
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                let _ = tx.send(Err(format!("No audio output device: {e}")));
            }
        })
        .map_err(|e| format!("Failed to start audio thread: {e}"))?;

    let handle = rx
        .recv()
        .map_err(|_| "Audio output thread exited".to_string())??;

    // If another thread beat us to it (shouldn't happen under INIT_LOCK, but
    // set() only fails if already-initialized), fall back to that instance.
    let _ = PLAYER.set(Player {
        handle,
        current: Mutex::new(None),
    });
    Ok(PLAYER.get().expect("just set above"))
}

/// Play interleaved 16-bit PCM natively, replacing any clip already playing.
/// Returns as soon as playback starts.
pub fn play_pcm_i16(
    event_sink: &SharedEventSink,
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
) -> Result<(), String> {
    let active = player()?;
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    // Stop whatever was playing before starting the replacement.
    if let Some(prev) = active.current.lock().unwrap().take() {
        prev.stop();
    }

    let sink =
        Sink::try_new(&active.handle).map_err(|e| format!("Failed to create audio sink: {e}"))?;
    sink.append(SamplesBuffer::new(channels, sample_rate, samples));
    let sink = Arc::new(sink);
    *active.current.lock().unwrap() = Some(sink.clone());

    // Watch for natural completion. `sleep_until_end` also returns when the
    // sink is stopped (by a replacement or an explicit stop); the generation
    // check ensures only a genuine, still-current end emits the event.
    let event_sink = event_sink.clone();
    std::thread::spawn(move || {
        sink.sleep_until_end();
        if GENERATION.load(Ordering::SeqCst) == generation {
            if let Ok(active) = player() {
                let mut current = active.current.lock().unwrap();
                if current.as_ref().is_some_and(|s| Arc::ptr_eq(s, &sink)) {
                    *current = None;
                }
            }
            let _ = event_sink.emit_event(PLAYBACK_ENDED_EVENT, &generation);
        }
    });

    Ok(())
}

/// Stop the currently playing clip, if any.
pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(player) = player() {
        if let Some(sink) = player.current.lock().unwrap().take() {
            sink.stop();
        }
    }
}
