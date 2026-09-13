//! Real-time RMS/peak meter for the recording page, on native
//! PipeWire capture streams.
//!
//! The frontend asks to monitor a microphone and/or a system-audio
//! device by PipeWire node id (or `"default"`). Levels are emitted via
//! the `audio-levels` event, keyed by **role** (`"mic"` / `"system"`)
//! — the UI renders exactly one meter per role, so role keys avoid
//! making the frontend track device-id changes.

use anyhow::Result;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::pw::{PwCaptureStream, PwStreamEvent};
use super::recording_state::DeviceType;
use super::stream::capture_target_for;
use crate::events::{EventSinkExt, SharedEventSink};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioLevelData {
    /// Role key: `"mic"` or `"system"`.
    pub device_name: String,
    pub device_type: String,
    pub rms_level: f32,
    pub peak_level: f32,
    pub is_active: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioLevelUpdate {
    pub timestamp: u64,
    pub levels: Vec<AudioLevelData>,
}

/// Monotonically increasing generation; bumping it invalidates the
/// running emit task and any streams from a prior start.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct MonitorSession {
    streams: Vec<PwCaptureStream>,
}

static SESSION: OnceLock<Mutex<Option<MonitorSession>>> = OnceLock::new();

fn session_slot() -> &'static Mutex<Option<MonitorSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/// Pure decision at the heart of the generation race fix: is a call that
/// captured `call_generation` (at its start) still the most recent one,
/// given the generation counter's current value `current_generation`?
/// A call is current only on an exact match — any later start or stop
/// bumps the counter past it, and a call can never see a generation from
/// the future.
fn is_generation_current(call_generation: u64, current_generation: u64) -> bool {
    call_generation == current_generation
}

fn open_role_stream(
    role: &'static str,
    device_id: &str,
    device_type: DeviceType,
    levels: Arc<Mutex<HashMap<&'static str, AudioLevelData>>>,
) -> Result<PwCaptureStream> {
    let target = capture_target_for(device_id, device_type);
    let type_label = match device_type {
        DeviceType::Microphone => "input",
        DeviceType::System => "output",
    };
    PwCaptureStream::open(
        target,
        Box::new(move |samples| {
            if samples.is_empty() {
                return;
            }
            let rms =
                (samples.iter().map(|&x| x * x).sum::<f32>() / samples.len() as f32).sqrt();
            let peak = samples.iter().map(|&x| x.abs()).fold(0.0_f32, f32::max);
            let entry = AudioLevelData {
                device_name: role.to_string(),
                device_type: type_label.to_string(),
                rms_level: rms.min(1.0),
                peak_level: peak.min(1.0),
                is_active: rms > 0.001,
            };
            if let Ok(mut map) = levels.lock() {
                map.insert(role, entry);
            }
        }),
        Box::new(move |event| {
            if !matches!(event, PwStreamEvent::Ended) {
                warn!("level monitor: {} stream event: {:?}", role, event);
            }
        }),
    )
}

/// Start (or restart) level monitoring.
///
/// `mic_device` / `system_device`: PipeWire node id, `"default"`, or
/// `None` to skip that role.
pub async fn start_monitoring(
    sink: SharedEventSink,
    mic_device: Option<String>,
    system_device: Option<String>,
) -> Result<()> {
    info!(
        "level monitor: start (mic={:?}, system={:?})",
        mic_device, system_device
    );

    // Capture our generation *before* doing anything async. Any later
    // `start_monitoring`/`stop_monitoring` call bumps GENERATION further,
    // which is how we notice — after our (slow, blocking) stream-open
    // completes — that we've been superseded and must not touch SESSION.
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    let levels: Arc<Mutex<HashMap<&'static str, AudioLevelData>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Opening streams blocks on the PipeWire roundtrip — keep it off
    // the async runtime.
    let levels_for_open = levels.clone();
    let streams = tokio::task::spawn_blocking(move || {
        let mut streams = Vec::new();
        if let Some(mic) = mic_device {
            match open_role_stream("mic", &mic, DeviceType::Microphone, levels_for_open.clone()) {
                Ok(s) => streams.push(s),
                Err(e) => warn!("level monitor: could not open mic '{}': {}", mic, e),
            }
        }
        if let Some(system) = system_device {
            match open_role_stream(
                "system",
                &system,
                DeviceType::System,
                levels_for_open.clone(),
            ) {
                Ok(s) => streams.push(s),
                Err(e) => warn!(
                    "level monitor: could not open system audio '{}': {}",
                    system, e
                ),
            }
        }
        streams
    })
    .await?;

    if streams.is_empty() {
        warn!("level monitor: no streams opened; UI will receive no events");
        return Ok(());
    }

    // Install the freshly opened streams as SESSION only if we are still
    // the current generation — otherwise a newer `start_monitoring` (or a
    // `stop_monitoring`) raced ahead of us while we were blocked opening
    // streams, and installing now would silently replace its live streams
    // with ours (freezing its meters) while ours leak, uncaptured by
    // anything. Either way, whatever ends up discarded (our own streams if
    // we're stale, or the previous session's streams if we win) is dropped
    // off the async runtime below — dropping a `PwCaptureStream` joins its
    // capture thread, which must never happen on the runtime.
    let (installed, discarded) = {
        let mut guard = session_slot()
            .lock()
            .map_err(|_| anyhow::anyhow!("level monitor: session mutex poisoned"))?;
        if is_generation_current(generation, GENERATION.load(Ordering::SeqCst)) {
            let old = guard.replace(MonitorSession { streams });
            (true, old.map(|s| s.streams))
        } else {
            (false, Some(streams))
        }
    };

    if let Some(streams) = discarded {
        tokio::task::spawn_blocking(move || drop(streams)).await?;
    }

    if !installed {
        debug!(
            "level monitor: generation {} superseded before install",
            generation
        );
        return Ok(());
    }

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        while is_generation_current(generation, GENERATION.load(Ordering::SeqCst)) {
            interval.tick().await;

            let snapshot: Vec<AudioLevelData> = match levels.lock() {
                Ok(guard) => guard.values().cloned().collect(),
                Err(_) => continue,
            };
            if snapshot.is_empty() {
                continue;
            }

            let update = AudioLevelUpdate {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                levels: snapshot,
            };

            if sink.emit_event("audio-levels", &update).is_err() {
                break;
            }
        }
        debug!("level monitor: emit task exiting (generation {})", generation);
    });

    Ok(())
}

pub async fn stop_monitoring() -> Result<()> {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    // Stream teardown joins the PipeWire loop threads — keep it off
    // the async runtime.
    tokio::task::spawn_blocking(stop_current_session).await?;
    Ok(())
}

fn stop_current_session() {
    if let Ok(mut guard) = session_slot().lock() {
        if let Some(session) = guard.take() {
            for stream in session.streams {
                stream.stop();
            }
        }
    }
}

pub fn is_monitoring() -> bool {
    session_slot()
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::is_generation_current;

    #[test]
    fn current_generation_matches_exactly() {
        assert!(is_generation_current(3, 3));
    }

    #[test]
    fn superseded_by_a_later_generation() {
        // A slow start_monitoring call captured generation 3, but by the
        // time it finishes opening streams a newer start/stop call has
        // bumped the counter to 4 (or beyond) — it must not install.
        assert!(!is_generation_current(3, 4));
        assert!(!is_generation_current(3, 10));
    }

    #[test]
    fn stale_call_never_wins_even_against_generation_zero_reset() {
        // Defensive: a call can never be "current" against a smaller
        // counter value either (the counter only ever increases).
        assert!(!is_generation_current(3, 2));
    }
}
